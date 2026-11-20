// 文件: 基础功能/src/运行时/FFI代码生成器.rs
// 职责: 从 FFI 元数据生成 Rust FFI shim 代码，复用 rustc 的 C ABI 能力
//
// 设计: 读取 FFI 注册表中的外部函数和导出函数元数据，
//       生成一个完整的 Rust crate（shim.rs），包含：
//       - 外部函数: extern "C" { fn ... } + #[link(name=..)] + 包装函数
//       - 导出函数: extern "C" fn 智语言_导出_N(..) thunk
//       - 编组函数: 运行时值 ↔ C 原生类型 的转换
//
// 生成方式: 运行时字符串拼接，输出到 产出/外部语言文件/执行文件/shim.rs
// 编译模式: rustc --crate-type staticlib 编译 shim → .a/.lib，追加到链接器目标文件列表
// 解释模式: rustc --crate-type cdylib 编译 shim → .so/.dll，dlopen 加载
//
// 【统一架构】双模式共用同一份 shim.rs，外部调用契约不变（系统调用 119 扁平运行时值）。

use std::collections::{HashMap, HashSet};
use std::sync::LazyLock;

/// 【G5】最近一次 shim 生成的导出函数元数据缓存，供 生成导出头文件 复用类型信息。
static 导出函数缓存: LazyLock<std::sync::Mutex<Vec<导出函数元数据>>> =
    LazyLock::new(|| std::sync::Mutex::new(Vec::new()));

/// FFI 外部函数导入声明的元数据（从 外部调用注册.rs 的 FFI项 转换而来）
#[derive(Debug, Clone)]
pub struct 导入函数元数据 {
    pub 键: usize,
    pub c符号: String,
    pub 库标识: String,
    pub 形参类型: Vec<C类型>,
    pub 返回类型: C类型,
    pub 输出位图: Vec<bool>,
    pub 是否静态: bool,  // 🆕 是否为静态链接库（控制 #[link] kind：static / raw-dylib）
    pub 原生约定: bool,  // true=真实C ABI（生成 shim 包装），false=扁平约定（不生成 shim）
    /// 🆕 FFI（§6.5.2 第5项）：是否外部全局变量（true=C 全局变量，shim 走 `static` 读取而非 `fn` 调用）
    pub 是否全局变量: bool,
    /// 🆕 2026-08-29 FFI 数组：每形参的数组元素外部类型码（非数组形参=0；数组形参=元素精确码，
    /// shim 按元素码把宿主传入的缓冲地址还原为 *const <元素Rust类型>）
    pub 形参数组元素C类型: Vec<u8>,
}

/// FFI 导出函数 thunk 的元数据
#[derive(Debug, Clone)]
pub struct 导出函数元数据 {
    pub 导出序号: usize,
    pub 智语言函数名: String,
    pub c符号: String,  // 🆕【G5】真实 C 符号名（如 math_add），与导出库实际导出符号对齐
    pub 形参类型: Vec<C类型>,
    pub 返回类型: C类型,
}

/// C ABI 类型枚举（对应文档 6.5.6 类型映射表）
///
/// 定宽变体（整数8..无符号64）对应 C 定宽整型；平台相关变体
/// （C长整数/C无符号长整数/C字节数量/C有符号字节数量/C字符/C无符号字符）
/// 直接映射 Rust `std::os::raw`（c_long/c_ulong/c_size_t/c_ssize_t/c_char/c_uchar），
/// 平台差异（long 在 Windows=32 位、Linux=64 位等）由 Rust 编译器自动消化，
/// 智语言侧零平台判断代码。
#[derive(Debug, Clone, PartialEq)]
pub enum C类型 {
    空值,       // void
    整数8,      // int8_t / char
    整数16,     // int16_t / short
    整数32,     // int32_t / int
    整数64,     // int64_t / long long
    无符号8,    // uint8_t / unsigned char
    无符号16,   // uint16_t / unsigned short
    无符号32,   // uint32_t / unsigned int
    无符号64,   // uint64_t / unsigned long long
    小数32,     // float
    小数64,     // double
    逻辑,       // bool
    指针,       // void* / 任意指针
    字符串,     // const char* (文本→C字符串)
    // ── 平台相关变体（直接映射 Rust std::os::raw，委托 Rust 消化平台差异）──
    C长整数,          // long（Windows=32 位、Linux/macOS=64 位）→ c_long
    C无符号长整数,    // unsigned long → c_ulong
    C字节数量,        // size_t → c_size_t
    C有符号字节数量,  // ssize_t → c_ssize_t
    C字符,            // char → c_char
    C无符号字符,      // unsigned char → c_uchar
    结构体(String, Vec<C类型>), // 外部结构体
    /// 回调函数指针：u8 = trampoline 形状码（与 shim 预生成的 5 种 trampoline 对齐）。
    /// 0=void fn(i64)、1=i64 fn()、2=i64 fn(i64)、3=i64 fn(i64,*mut c_void)、4=void fn(*mut c_void)、
    /// 255=动态（无匹配签名，回退 fn()，仅作不透明句柄传递，调用方不得按特定签名调用）。
    函数指针(u8),              // 回调函数指针
    /// 🆕 2026-09-25 外部数组字段（规范 §6.5.6 ③ + 数组映射表 17080-17086）：数组字段 → C 指针 `X*`。
    /// 与 `指针` 的差别仅在**元素类型已知**：shim 据此生成 `*mut X`，并按元素宽度做序列化/还原/长度校验。
    /// 来源：布局码 `布局码_外部数组字段`(22) + 紧随一字节元素码 ⇒ Box 内即元素 C类型。
    数组(Box<C类型>),
}

/// 🆕 2026-09-25 外部结构体布局码：**数组字段标记码**（其后紧跟一字节元素精确码）。
/// ⚠ 与 前端 `降级/公共降级.rs::布局码_外部数组字段` **严格对齐**（跨层码表按既有惯例各自登记，
/// 与 `精确类型码到C类型` 的对齐要求相同）。
pub const 布局码_外部数组字段: u8 = 22;

impl C类型 {
    pub fn 取Rust类型名(&self) -> String {
        match self {
            C类型::空值 => "()".to_string(),
            C类型::整数8 => "i8".to_string(),
            C类型::整数16 => "i16".to_string(),
            C类型::整数32 => "i32".to_string(),
            C类型::整数64 => "i64".to_string(),
            C类型::无符号8 => "u8".to_string(),
            C类型::无符号16 => "u16".to_string(),
            C类型::无符号32 => "u32".to_string(),
            C类型::无符号64 => "u64".to_string(),
            C类型::小数32 => "f32".to_string(),
            C类型::小数64 => "f64".to_string(),
            C类型::逻辑 => "bool".to_string(),
            C类型::指针 => "*mut std::ffi::c_void".to_string(),
            C类型::字符串 => "*const i8".to_string(),
            // ★平台相关：长整型/字符直接映射 std::os::raw（Rust 编译器按目标平台自动消化）；
            // 字节数量 = usize（恒等于指针宽度，即 C size_t 语义，Rust 无 c_size_t 用 usize 等价），
            // 有符号字节数量 = isize（ssize_t 语义）。
            C类型::C长整数 => "std::os::raw::c_long".to_string(),
            C类型::C无符号长整数 => "std::os::raw::c_ulong".to_string(),
            C类型::C字节数量 => "usize".to_string(),
            C类型::C有符号字节数量 => "isize".to_string(),
            C类型::C字符 => "std::os::raw::c_char".to_string(),
            C类型::C无符号字符 => "std::os::raw::c_uchar".to_string(),
            C类型::结构体(名, _) => format!("{}_布局", 名),
            // 🆕 数组字段（规范数组映射表）：→ C 指针 `*mut X`（元素类型用于按宽度编组）
            C类型::数组(元素) => format!("*mut {}", 元素.取Rust类型名()),
            // ★trampoline 形状 → Rust 函数签名（与 shim 预生成的 5 种 trampoline 一一对齐）
            C类型::函数指针(形状) => Self::形状到函数签名(*形状).to_string(),
        }
    }

    /// trampoline 形状码 → 对应 Rust/C ABI 函数签名字符串。
    /// 形状码与 `外部调用注册::注册ffi回调` 经 `转换签名形状` 调度的 5 种 trampoline 严格对应，
    /// 保证 shim 把地址 `transmute` 成正确签名后，C 侧按此签名调用回调不会 ABI 错乱。
    pub fn 形状到函数签名(形状: u8) -> &'static str {
        match 形状 {
            0 => "Option<unsafe extern \"C\" fn(i64)>",
            1 => "Option<unsafe extern \"C\" fn() -> i64>",
            2 => "Option<unsafe extern \"C\" fn(i64) -> i64>",
            3 => "Option<unsafe extern \"C\" fn(i64, *mut std::ffi::c_void) -> i64>",
            4 => "Option<unsafe extern \"C\" fn(*mut std::ffi::c_void)>",
            _ => "Option<unsafe extern \"C\" fn()>",
        }
    }

    pub fn 取智语言类型标签(&self) -> u64 {
        match self {
            C类型::空值 => 0,
            C类型::整数8 | C类型::整数16 | C类型::整数32 | C类型::整数64
            | C类型::无符号8 | C类型::无符号16 | C类型::无符号32 | C类型::无符号64
            | C类型::C长整数 | C类型::C无符号长整数
            | C类型::C字节数量 | C类型::C有符号字节数量
            | C类型::C字符 | C类型::C无符号字符 => 1,
            C类型::小数32 | C类型::小数64 => 2,
            C类型::逻辑 => 3,
            C类型::指针 => 4,     // 句柄
            C类型::字符串 => 10,   // 外部文本
            C类型::结构体(_, _) => 4,
            C类型::函数指针(_) => 6,
            // 🆕 数组字段：运行时值为**紧凑缓冲地址**（标签_整数），与数组形参同款（见 生成参数读取代码）
            C类型::数组(_) => 1,
        }
    }

    /// 输出参数的本地临时变量默认值字面量
    pub fn 取默认值字面量(&self) -> String {
        match self {
            C类型::整数8 | C类型::整数16 | C类型::整数32 | C类型::整数64
            | C类型::无符号8 | C类型::无符号16 | C类型::无符号32 | C类型::无符号64
            | C类型::C长整数 | C类型::C无符号长整数
            | C类型::C字节数量 | C类型::C有符号字节数量
            | C类型::C字符 | C类型::C无符号字符 => "0".to_string(),
            C类型::小数32 | C类型::小数64 => "0.0".to_string(),
            C类型::逻辑 => "false".to_string(),
            C类型::指针 => "std::ptr::null_mut()".to_string(),
            C类型::字符串 => "std::ptr::null()".to_string(),
            // ★类型名必须与 取Rust类型名 一致（`<名>_布局`）；旧写法 `<名>::default()`
            // 指向不存在的类型，结构体作输出参数时 shim 直接编译失败。
            C类型::结构体(名, _) => format!("{}_布局::default()", 名),
            // 🆕 数组字段默认值 = 空指针
            C类型::数组(_) => "std::ptr::null_mut()".to_string(),
            C类型::空值 | C类型::函数指针(_) => "std::ptr::null_mut()".to_string(),
        }
    }

    /// 是否整数类（有符号/无符号/平台相关整型）
    pub fn 是整数类(&self) -> bool {
        matches!(
            self,
            C类型::整数8 | C类型::整数16 | C类型::整数32 | C类型::整数64
            | C类型::无符号8 | C类型::无符号16 | C类型::无符号32 | C类型::无符号64
            | C类型::C长整数 | C类型::C无符号长整数
            | C类型::C字节数量 | C类型::C有符号字节数量
            | C类型::C字符 | C类型::C无符号字符
        )
    }

    /// 是否无符号整数类（写回智语言整数时需无符号扩展，避免负值化）
    pub fn 是无符号整数类(&self) -> bool {
        matches!(
            self,
            C类型::无符号8 | C类型::无符号16 | C类型::无符号32 | C类型::无符号64
            | C类型::C无符号长整数 | C类型::C字节数量 | C类型::C无符号字符
        )
    }
}

/// ============================================================
///  Rust FFI shim 代码生成器
/// ============================================================

pub struct FFI代码生成器;

impl FFI代码生成器 {
    /// 生成完整的 shim.rs 文件内容（诊断被忽略，保留给既有调用方）
    pub fn 生成shim(
        导入函数列表: &[导入函数元数据],
        导出函数列表: &[导出函数元数据],
    ) -> String {
        Self::生成shim带诊断(导入函数列表, 导出函数列表).0
    }

    /// 【G5】缓存最近一次 shim 生成收到的导出函数元数据，供生成导出 .h 时复用类型信息。
    /// 导出函数元数据含完整 C 类型（形参类型/返回类型/c符号），而语义层只透传 (智名, c符号)
    /// 二元组，故 .h 真实签名必须回 base 取此缓存，而非用二元组重建（会丢失类型）。
    fn 缓存导出函数(导出函数列表: &[导出函数元数据]) {
        let mut 缓存 = 导出函数缓存.lock().unwrap();
        *缓存 = 导出函数列表.to_vec();
    }

    /// 【G5】生成导出库配套 .h 头文件内容（真实 C 签名 + 扁平契约兼容别名）。
    /// 复用 头文件生成器 的类型映射，符号名用真实 c符号（与双模导出库符号对齐）。
    pub fn 生成导出头文件(库名: &str) -> String {
        let 导出函数列表 = 导出函数缓存.lock().unwrap();
        super::头文件生成器::头文件生成器::生成头文件(&导出函数列表, 库名)
    }

    /// 🆕 G2：生成 shim.rs 并返回编译期诊断（如外部结构体含不支持的字段类型）。
    /// 调用方应在诊断非空时报错退出，而不是继续编译一个语义已错的 shim。
    pub fn 生成shim带诊断(
        导入函数列表: &[导入函数元数据],
        导出函数列表: &[导出函数元数据],
    ) -> (String, Vec<String>) {
        let mut 诊断列表: Vec<String> = Vec::new();
        let mut 代码 = String::new();

        // 【G5】缓存导出函数元数据（含完整 C 类型），供生成导出 .h 时复用。
        Self::缓存导出函数(导出函数列表);

        // ── 预处理：过滤空 c符号 并按 键 去重 ──
        // 外部函数表可能含有空 c符号 的占位项（运行时未解析），或重复 键，
        // 直接生成会得到非法 Rust（fn () -> ();）或重复定义。
        // 扁平 运行时值 约定的实际调用走 transmute 回退路径，不依赖 shim 包装，
        // 故此处仅保证生成的 shim.rs 可编译、链接即可。
        let mut 已见键 = HashSet::new();
        let 导入函数列表: Vec<导入函数元数据> = 导入函数列表
            .iter()
            .filter(|f| !f.c符号.is_empty())
            .filter(|f| 已见键.insert(f.键))
            .cloned()
            .collect();

        // ── 文件头 ──
        代码.push_str("// 自动生成: 智语言 FFI shim — 请勿手动编辑\n");
        代码.push_str("#![allow(non_snake_case)]\n");
        代码.push_str("#![allow(non_camel_case_types)]\n");
        代码.push_str("#![allow(dead_code)]\n");
        // 这两个 use 只在部分签名形状下用到（无字符串/无指针参数的 shim 用不上），
        // 加 allow 而非删除，避免按形状条件生成导致的分支膨胀与漏引用。
        代码.push_str("#![allow(unused_imports)]\n\n");
        代码.push_str("use std::ffi::CString;\n");
        代码.push_str("use std::os::raw::c_void;\n\n");

        // 【G5】导出 thunk 经 智语言_系统调用回调 C ABI 入口调真实智语言导出函数。
        // 该符号由 base crate 经 #[export_name] 导出（含非 ASCII 名），shim 编为 cdylib 后由宿主 dlopen 提供。
        // Rust 限制：extern 块内标识符不能用非 ASCII（issue #83942），故 Rust 绑定名用 ASCII
        // （z_sysc_cb），经 #[link_name] 指回真实导出的非 ASCII 符号名。
        代码.push_str("extern \"C\" {\n");
        代码.push_str("    #[link_name = \"智语言_系统调用回调\"]\n");
        代码.push_str("    fn z_sysc_cb(函数索引: u64, 参数指针: *const 运行时值, 数量: u64, 输出: *mut 运行时值);\n");
        代码.push_str("}\n\n");

        // ── 外部结构体布局 ──
        // 【缺口 D 修复】先按「首次出现顺序」建立 外部结构体名→类型索引 映射，
        // 该序号用于 shim 序列化 线_结构体 时的 [类型索引] 字段，与宿主 从外部值还原
        // （透传 分配结构体(类型索引,...)）对齐。宿主侧按声明顺序注册，shim 按收集顺序，
        // 同源文件声明顺序通常一致；即便不一致，运行时字段访问按字段索引读取、不依赖索引语义。
        let mut 外部结构体序号: HashMap<String, usize> = HashMap::new();
        {
            let mut 序号计数 = 0usize;
            let mut 登记 = |名: &str| {
                if !外部结构体序号.contains_key(名) {
                    外部结构体序号.insert(名.to_string(), 序号计数);
                    序号计数 += 1;
                }
            };
            for 导入 in &导入函数列表 {
                Self::登记结构体名递归(&导入.形参类型, &mut 登记);
                Self::登记结构体名递归(&[导入.返回类型.clone()], &mut 登记);
            }
        }

        代码.push_str("// ====================================\n");
        代码.push_str("//  外部结构体 #[repr(C)] 布局\n");
        代码.push_str("// ====================================\n");
        let mut 已生成布局 = HashSet::new();
        for 导入 in &导入函数列表 {
            Self::收集外部结构体(&导入.形参类型, &mut 代码, &mut 已生成布局);
            Self::收集外部结构体(&[导入.返回类型.clone()], &mut 代码, &mut 已生成布局);
        }

        // 【缺口 D 修复】为每个外部结构体生成递归序列化 helper（结构体值 → 扁平缓冲），
        // 与宿主 从外部值还原 的 线_结构体 分支（[类型索引][字段数][字段...]）严格对齐。
        // 🆕 ① 递归覆盖嵌套结构体（序列化 helper 内部再递归生成嵌套 helper）。
        let mut 已生成序列化 = HashSet::new();
        for 导入 in &导入函数列表 {
            for t in 导入.形参类型.iter().chain(std::iter::once(&导入.返回类型)) {
                if let C类型::结构体(名, 字段) = t {
                    Self::生成结构体序列化辅助(名, 字段, &外部结构体序号, &mut 代码, &mut 诊断列表, &mut 已生成序列化);
                }
            }
        }

        // ── 运行时值布局 ──
        代码.push_str("\n// ====================================\n");
        代码.push_str("//  运行时值布局（与 基础功能 crate 对齐）\n");
        代码.push_str("// ====================================\n");
        代码.push_str("#[repr(C)]\n");
        代码.push_str("#[derive(Debug, Clone, Copy)]\n");
        代码.push_str("pub struct 运行时值 {\n");
        代码.push_str("    pub 标签: u64,\n");
        代码.push_str("    pub 整数: i64,\n");
        代码.push_str("    pub 小数: f64,\n");
        代码.push_str("    pub 句柄: u64,\n");
        代码.push_str("}\n\n");
        代码.push_str("impl 运行时值 {\n");
        代码.push_str("    pub fn 空值() -> Self { 运行时值 { 标签: 0, 整数: 0, 小数: 0.0, 句柄: 0 } }\n");
        代码.push_str("    pub fn 整数(v: i64) -> Self { 运行时值 { 标签: 1, 整数: v, 小数: 0.0, 句柄: 0 } }\n");
        代码.push_str("    pub fn 小数(v: f64) -> Self { 运行时值 { 标签: 2, 整数: 0, 小数: v, 句柄: 0 } }\n");
        代码.push_str("    pub fn 逻辑(v: bool) -> Self { 运行时值 { 标签: 3, 整数: if v { 1 } else { 0 }, 小数: 0.0, 句柄: 0 } }\n");
        代码.push_str("}\n\n");

        // ── 外部函数导入 ──
        if !导入函数列表.is_empty() {
            代码.push_str("// ====================================\n");
            代码.push_str("//  外部函数导入 — extern \"C\" 声明\n");
            代码.push_str("// ====================================\n");
            // 按库标识分组
            let mut 按库分组: HashMap<String, Vec<&导入函数元数据>> = HashMap::new();
            for f in &导入函数列表 {
                按库分组.entry(f.库标识.clone()).or_default().push(f);
            }
            for (库标识, 函数列表) in &按库分组 {
                // 静态链接库（用户以 静态 关键字声明）生成 #[link(kind="static")]，需提供导入库(.lib/.a)；
                // 动态链接库：Windows 用 #[link(kind="raw-dylib")]，由 rustc 从 -L 路径的 .dll
                // 自动生成 .dll_imports.lib 导入表（要求 .dll 在 -L 搜索路径，已通过 执行文件目录 加入）；
                // POSIX（Linux/macOS）原生不支持 raw-dylib，改为 #[link(kind="dylib")]，由 -L 路径中的
                // .so/.dylib 直接解析（跨平台编译/链接修复，原 raw-dylib 在 POSIX 上 rustc 直接报错）。
                let 是否静态组 = 函数列表.first().map_or(false, |f| f.是否静态);
                let 链接种类 = if 是否静态组 {
                    "static"
                } else if cfg!(target_os = "windows") {
                    "raw-dylib"
                } else {
                    "dylib"
                };
                // 【G10】链接名必须 ASCII：中文库名会让 rustc 生成损坏的 .dll_imports.lib（LNK1107）。
                // 部署侧（生成并编译shim 前置步骤）已按同名落一份库文件副本到 -L 搜索路径。
                代码.push_str(&format!(
                    "#[link(name = \"{}\", kind = \"{}\")]\n",
                    Self::链接库安全名(库标识),
                    链接种类
                ));
                代码.push_str("extern \"C\" {\n");
                // 同一库内允许多个声明指向同一 C 符号（别名/重复关联），extern 项须按符号去重
                let mut 已声明: std::collections::HashSet<String> = std::collections::HashSet::new();
                for f in 函数列表.iter() {
                    let rust_fn = Self::外部符号Rust名(&f.库标识, &f.c符号);
                    if !已声明.insert(rust_fn.clone()) {
                        continue;
                    }
                    let 返回类型 = f.返回类型.取Rust类型名();
                    // 🆕 FFI: extern "C" 声明需包含输出参数（作为 C 指针 out 参数），
                    // 且参数名与 生成导入包装 的调用实参一致：输入 a{i}、输出 pout{i}。
                    let 形参声明: Vec<String> = f.形参类型.iter()
                        .enumerate()
                        .map(|(i, t)| {
                            let 是输出 = f.输出位图.get(i).copied().unwrap_or(false);
                            if 是输出 {
                                format!("pout{}: *mut {}", i, t.取Rust类型名())
                            } else {
                                // 🆕 2026-08-29 FFI 数组：extern 形参用 *const <元素Rust类型>
                                // （与 生成参数读取代码 一致；宿主已把数组序列化为紧凑缓冲地址）
                                let 数组元素码 = f.形参数组元素C类型.get(i).copied().unwrap_or(0);
                                if 数组元素码 != 0 {
                                    let 元素类型名 = super::外部调用注册::精确类型码到C类型(数组元素码).取Rust类型名();
                                    format!("a{}: *const {}", i, 元素类型名)
                                } else {
                                    format!("a{}: {}", i, t.取Rust类型名())
                                }
                            }
                        })
                        .collect();
                    let 形参声明串 = 形参声明.join(", ");
                    if f.是否全局变量 {
                        // 🆕 FFI（§6.5.2 第5项）：C 全局变量走 `static` 读取，不能当 `fn` 调用。
                        // global 为不可变静态引用，类型须为非 ()；字符串常量用 *const c_char。
                        let 静态类型 = if 返回类型 == "()" {
                            "*mut std::os::raw::c_void".to_string()
                        } else {
                            返回类型.clone()
                        };
                        代码.push_str(&format!("    #[link_name = \"{}\"]\n    static {}: {};\n", f.c符号, rust_fn, 静态类型));
                    } else if 返回类型 == "()" {
                        代码.push_str(&format!("    #[link_name = \"{}\"]\n    fn {}({}) -> ();\n", f.c符号, rust_fn, 形参声明串));
                    } else {
                        代码.push_str(&format!("    #[link_name = \"{}\"]\n    fn {}({}) -> {};\n", f.c符号, rust_fn, 形参声明串, 返回类型));
                    }
                }
                代码.push_str("}\n\n");
            }

            // ── 编组包装函数 ──
            代码.push_str("// ====================================\n");
            代码.push_str("//  导入包装函数 — 运行时值 ↔ C 类型编组\n");
            代码.push_str("// ====================================\n");
            代码.push_str("#[export_name = \"z_shim_import_dispatch\"]\n");
            代码.push_str("pub unsafe extern \"C\" fn z_shim_import_dispatch(\n");
            代码.push_str("    函数索引: u64,\n");
        代码.push_str("    // 双模式统一契约：void fn(*mut 运行时值 输出, i64 数量, *mut 运行时值 输入)\n");
        代码.push_str("    输出: *mut 运行时值,\n");
        代码.push_str("    数量: i64,\n");
        代码.push_str("    输入数组: *mut 运行时值,\n");
            代码.push_str(") {\n");
            代码.push_str("    match 函数索引 {\n");
            for f in &导入函数列表 {
                代码.push_str(&format!("        {} => z_shim_import_{}(输出, 数量, 输入数组),\n",
                    f.键, f.键));
            }
            代码.push_str("        _ => {}\n");
            代码.push_str("    }\n");
            代码.push_str("}\n\n");

            // 每个导入函数的包装函数
            for f in 导入函数列表.iter() {
                Self::生成导入包装(f, &外部结构体序号, &mut 代码);
            }
        }

        // ── 导出函数 thunk ──
        if !导出函数列表.is_empty() {
            代码.push_str("// ====================================\n");
            代码.push_str("//  导出函数 thunk — C ABI 导出\n");
            代码.push_str("// ====================================\n");
            for f in 导出函数列表 {
                Self::生成导出thunk(f, &mut 代码);
            }
        }

        // ── 回调槽位表 + trampoline 函数 ──
        代码.push_str("\n// ====================================\n");
        代码.push_str("//  回调槽位表 + 各形状 trampoline 函数\n");
        代码.push_str("//  5 种预设形状 × 256 槽位\n");
        代码.push_str("//  每槽对应一个 extern \"C\" 函数，地址 = trampoline\n");
        代码.push_str("// ====================================\n");
        代码.push_str("// 槽位表：[形状][槽位] = 函数索引（0=空闲）\n");
        代码.push_str("static mut 回调槽位表: [[u64; 256]; 5] = [[0; 256]; 5];\n\n");

        // 内部调度函数（通过函数指针调用，兼容 cdylib 不直连宿主符号）
        代码.push_str("static mut 回调系统调用: Option<unsafe extern \"C\" fn(u64, *const 运行时值, u64, *mut 运行时值)> = None;\n\n");
        代码.push_str("#[export_name = \"z_shim_set_callback_dispatch\"]\n");
        代码.push_str("pub unsafe extern \"C\" fn z_shim_set_callback_dispatch(\n");
        代码.push_str("    调度: unsafe extern \"C\" fn(u64, *const 运行时值, u64, *mut 运行时值),\n");
        代码.push_str(") {\n");
        代码.push_str("    回调系统调用 = Some(调度);\n");
        代码.push_str("}\n\n");
        代码.push_str("unsafe fn 回调内部调度(形状: usize, 槽位: usize, 参数: &[运行时值]) -> 运行时值 {\n");
        代码.push_str("    let 函数索引 = 回调槽位表[形状][槽位];\n");
        代码.push_str("    if 函数索引 == 0 { return 运行时值::空值(); }\n");
        代码.push_str("    if let Some(系统调用) = 回调系统调用 {\n");
        代码.push_str("        let mut 输出 = 运行时值::空值();\n");
        代码.push_str("        // 【缺口 C 修复】直接把用户函数索引（≥200）传给回调 dispatch 入口\n");
        代码.push_str("        // （智语言_系统调用回调），不再硬编码 119（否则会被当 FFI 命令码误路由）。\n");
        代码.push_str("        系统调用(函数索引, 参数.as_ptr(), 参数.len() as u64, &mut 输出);\n");
        代码.push_str("        输出\n");
        代码.push_str("    } else {\n");
        代码.push_str("        运行时值::空值()\n");
        代码.push_str("    }\n");
        代码.push_str("}\n\n");

        // 形状 0: void fn(i64)
        for 槽位 in 0..256 {
            代码.push_str(&format!("#[export_name = \"z_cb_0_{s}\"]\npub unsafe extern \"C\" fn z_cb_0_{s}(v: i64) {{\n", s=槽位));
            代码.push_str(&format!("    let _ = 回调内部调度(0, {s}, &[运行时值::整数(v)]);\n", s=槽位));
            代码.push_str("}\n");
        }

        // 形状 1: i64 fn()
        for 槽位 in 0..256 {
            代码.push_str(&format!("#[export_name = \"z_cb_1_{s}\"]\npub unsafe extern \"C\" fn z_cb_1_{s}() -> i64 {{\n", s=槽位));
            代码.push_str(&format!("    let 结果 = 回调内部调度(1, {s}, &[]);\n", s=槽位));
            代码.push_str("    结果.整数\n");
            代码.push_str("}\n");
        }

        // 形状 2: i64 fn(i64)
        for 槽位 in 0..256 {
            代码.push_str(&format!("#[export_name = \"z_cb_2_{s}\"]\npub unsafe extern \"C\" fn z_cb_2_{s}(v: i64) -> i64 {{\n", s=槽位));
            代码.push_str(&format!("    let 结果 = 回调内部调度(2, {s}, &[运行时值::整数(v)]);\n", s=槽位));
            代码.push_str("    结果.整数\n");
            代码.push_str("}\n");
        }

        // 形状 3: i64 fn(i64, void*)
        for 槽位 in 0..256 {
            代码.push_str(&format!("#[export_name = \"z_cb_3_{s}\"]\npub unsafe extern \"C\" fn z_cb_3_{s}(v: i64, p: *mut std::ffi::c_void) -> i64 {{\n", s=槽位));
            代码.push_str(&format!("    let 结果 = 回调内部调度(3, {s}, &[\n", s=槽位));
            代码.push_str("        运行时值::整数(v),\n");
            代码.push_str("        运行时值::整数(p as i64),\n");
            代码.push_str("    ]);\n");
            代码.push_str("    结果.整数\n");
            代码.push_str("}\n");
        }

        // 形状 4: void fn(void*)
        for 槽位 in 0..256 {
            代码.push_str(&format!("#[export_name = \"z_cb_4_{s}\"]\npub unsafe extern \"C\" fn z_cb_4_{s}(p: *mut std::ffi::c_void) {{\n", s=槽位));
            代码.push_str(&format!("    let 结果 = 回调内部调度(4, {s}, &[\n", s=槽位));
            代码.push_str("        运行时值::整数(p as i64),\n");
            代码.push_str("    ]);\n");
            代码.push_str("    let _ = 结果;\n");
            代码.push_str("}\n");
        }

        // ── 回调函数指针地址表（便于注册时取地址） ──
        代码.push_str("\n// 根据形状+槽位取 trampoline 函数地址\n");
        代码.push_str("#[inline(always)]\n");
        代码.push_str("unsafe fn 取回调地址(形状: usize, 槽位: usize) -> u64 {\n");
        代码.push_str("    match 形状 {\n");
        let 形状签名: [(usize, &str); 5] = [
            (0, "unsafe extern \"C\" fn(i64)"),
            (1, "unsafe extern \"C\" fn() -> i64"),
            (2, "unsafe extern \"C\" fn(i64) -> i64"),
            (3, "unsafe extern \"C\" fn(i64, *mut std::ffi::c_void) -> i64"),
            (4, "unsafe extern \"C\" fn(*mut std::ffi::c_void)"),
        ];
        for (形状id, 签名) in &形状签名 {
            代码.push_str(&format!("        {} => match 槽位 {{\n", 形状id));
            for 槽位 in 0..256 {
                代码.push_str(&format!(
                    "            {} => std::mem::transmute::<{}, u64>(z_cb_{}_{} as {}),\n",
                    槽位, 签名, 形状id, 槽位, 签名
                ));
            }
            代码.push_str("            _ => 0,\n");
            代码.push_str("        },\n");
        }
        代码.push_str("        _ => 0,\n");
        代码.push_str("    }\n");
        代码.push_str("}\n\n");

        // ── 注册回调入口 ──
        代码.push_str("#[export_name = \"z_shim_register_callback\"]\n");
        代码.push_str("pub unsafe extern \"C\" fn z_shim_register_callback(\n");
        代码.push_str("    形状: u64,\n");
        代码.push_str("    函数索引: u64,\n");
        代码.push_str(") -> u64 {\n");
        代码.push_str("    let 形状usize = 形状 as usize;\n");
        代码.push_str("    if 形状usize >= 5 { return 0; }\n");
        // 同一个函数重复注册时复用它已有的槽位。槽位从不释放，旧实现每次带回调参数的外部调用
        // 都新占一个槽：同一形状累计第 257 次调用起注册失败，C 侧拿到空函数指针后崩溃。
        // 槽位只记录「函数索引」，同一函数对应同一 trampoline，复用对仍持有旧地址的 C 代码无影响。
        代码.push_str("    for 槽位 in 0..256usize {\n");
        代码.push_str("        if 回调槽位表[形状usize][槽位] == 函数索引 && 函数索引 != 0 {\n");
        代码.push_str("            return 取回调地址(形状usize, 槽位);\n");
        代码.push_str("        }\n");
        代码.push_str("    }\n");
        代码.push_str("    for 槽位 in 0..256usize {\n");
        代码.push_str("        if 回调槽位表[形状usize][槽位] == 0 {\n");
        代码.push_str("            回调槽位表[形状usize][槽位] = 函数索引;\n");
        代码.push_str("            return 取回调地址(形状usize, 槽位);\n");
        代码.push_str("        }\n");
        代码.push_str("    }\n");
        代码.push_str("    0\n");
        代码.push_str("}\n");

        (代码, 诊断列表)
    }

    /// 🆕 ① 递归登记结构体名 → 类型索引（含嵌套结构体），供 线_结构体 头 [类型索引] 使用。
    /// 类型索引仅作堆对象标签（分配结构体 不据其查布局，字段还原按位置），顺序不一致亦无害。
    fn 登记结构体名递归(类型列表: &[C类型], 登记: &mut impl FnMut(&str)) {
        for t in 类型列表 {
            if let C类型::结构体(名, 字段) = t {
                登记(名);
                Self::登记结构体名递归(字段, 登记);
            }
        }
    }

    fn 收集外部结构体(类型列表: &[C类型], 代码: &mut String, 已生成: &mut HashSet<String>) {
        for t in 类型列表 {
            if let C类型::结构体(名, 字段) = t {
                // 🆕 ① 嵌套结构体：同一结构体可能经多函数/嵌套路径重复到达，仅生成一次定义
                if 已生成.insert(名.clone()) {
                    代码.push_str(&format!("#[repr(C)]\n"));
                    代码.push_str(&format!("#[derive(Debug, Clone, Copy)]\n"));
                    代码.push_str(&format!("pub struct {}_布局 {{\n", 名));
                    for (i, 字段类型) in 字段.iter().enumerate() {
                        let 字段名 = format!("字段_{}", i);
                        代码.push_str(&format!("    pub {}: {},\n", 字段名, 字段类型.取Rust类型名()));
                    }
                    代码.push_str("}\n");
                    // 🆕 G5: 生成 Default 实现（字段默认值 = 类型零值）
                    代码.push_str(&format!("impl Default for {}_布局 {{\n", 名));
                    代码.push_str("    fn default() -> Self {\n");
                    代码.push_str("        Self {\n");
                    for (i, 字段类型) in 字段.iter().enumerate() {
                        let 字段名 = format!("字段_{}", i);
                        let 默认值 = match 字段类型 {
                            C类型::整数8 | C类型::整数16 | C类型::整数32 | C类型::整数64
                            | C类型::无符号8 | C类型::无符号16 | C类型::无符号32 | C类型::无符号64
                            | C类型::C长整数 | C类型::C无符号长整数
                            | C类型::C字节数量 | C类型::C有符号字节数量
                            | C类型::C字符 | C类型::C无符号字符 => "0",
                            C类型::小数32 | C类型::小数64 => "0.0",
                            C类型::逻辑 => "false",
                            C类型::指针 | C类型::字符串 => "std::ptr::null_mut()",
                            C类型::函数指针(_) => "None",
                            C类型::结构体(_, _) => "unsafe { std::mem::zeroed() }",
                            // 🆕 数组字段：默认 = 空指针
                            C类型::数组(_) => "std::ptr::null_mut()",
                            C类型::空值 => "()",
                        };
                        代码.push_str(&format!("            {}: {},\n", 字段名, 默认值));
                    }
                    代码.push_str("        }\n");
                    代码.push_str("    }\n");
                    代码.push_str("}\n\n");
                }
                // 🆕 ① 递归生成嵌套结构体布局（无论是否已生成，内层均需登记）
                Self::收集外部结构体(字段, 代码, 已生成);
            }
        }
    }

    // ============================================================
    // 【缺口 D 修复】外部结构体序列化辅助生成
    // 为每个外部结构体生成 `fn 序列化_名(值: &名_布局, 扁平: &mut Vec<u8>)`：
    // 把每个字段按其 C类型 转为 运行时值（32 字节），顺序写入 扁平缓冲。
    // 该 扁平缓冲 随后由调用方包裹 [子标签=线_结构体][总长][类型索引][字段数] 头，
    // 与宿主 从外部值还原 的 线_结构体 分支（按字段索引递归还原）严格对齐。
    // 仅支持标量字段（整数/无符号/小数/逻辑/指针）；含 字符串/嵌套结构体/数组 字段
    // 的外部结构体返回值暂不支持（编译期诊断），避免产生错误线格式。
    // ============================================================
    /// 生成外部结构体的序列化 helper。
    ///
    /// 🆕 G2：复杂字段由 `panic!` 改为**返回诊断**——panic 会让整个开发套件进程崩溃，
    /// 用户只看到堆栈而非可行动的错误信息；改为收集诊断后由调用方统一报告并正常退出。
    /// 同时新增 `字符串` 字段支持：按 `[子标签=线_文本][字节长度][UTF-8]` 内嵌，
    /// 与宿主 `从外部值还原` 的递归还原路径对齐。
    fn 生成结构体序列化辅助(
        名: &str,
        字段: &[C类型],
        外部结构体序号: &HashMap<String, usize>,
        代码: &mut String,
        诊断: &mut Vec<String>,
        已生成: &mut HashSet<String>,
    ) {
        // 同一结构体可能经多个函数/嵌套路径重复到达，仅生成一次（避免 shim 重复定义）
        if !已生成.insert(名.to_string()) {
            return;
        }
        // 复杂字段检测：支持标量字段 + 字符串字段 + 嵌套结构体字段；函数指针仍不支持
        for (i, 字段类型) in 字段.iter().enumerate() {
            let 可支持 = matches!(字段类型,
                C类型::整数8 | C类型::整数16 | C类型::整数32 | C类型::整数64
                | C类型::无符号8 | C类型::无符号16 | C类型::无符号32 | C类型::无符号64
                | C类型::C长整数 | C类型::C无符号长整数 | C类型::C字节数量 | C类型::C有符号字节数量
                | C类型::小数32 | C类型::小数64 | C类型::逻辑 | C类型::指针
                | C类型::C字符 | C类型::C无符号字符
                | C类型::字符串 | C类型::结构体(..) | C类型::数组(..));
            if !可支持 {
                诊断.push(format!(
                    "外部结构体「{}」的第 {} 个字段类型 {:?} 暂不支持跨 FFI 边界传递。\n\
                     当前支持：整数(各位宽)/小数/逻辑/指针/字符/字节数量/文本/嵌套结构体/\
                     数组字段(`数组<标量外部类型>`，C 侧为指针 `X*`)。\n\
                     不支持：函数指针。建议改用 指针 承载并在 C 侧自行解引用。",
                    名, i + 1, 字段类型
                ));
                return;
            }
        }

        代码.push_str(&format!("fn 序列化_{}(值: &{}_布局, 扁平: &mut Vec<u8>) {{\n", 名, 名));
        for (i, 字段类型) in 字段.iter().enumerate() {
            let 字段访问 = format!("值.字段_{}", i);
            if matches!(字段类型, C类型::字符串) {
                // 🆕 2026-09-25 文本字段（协议修正）：构造 线_文本 子缓冲（[4][总长=16+len][UTF-8 字节]）
                //   → 泄漏所有权 → 按 **32 字节 运行时值 { 标签_外部值(10), 整数=子缓冲指针 }** 写入。
                //   ⚠ 为何必须如此：宿主 从外部值还原 的 线_结构体 分支对**每个字段**读 32 字节
                //     运行时值后递归还原；只有 标签_外部值 才会被解引用为子缓冲。
                //     旧实现把子缓冲**内联**写入（无 32 字节运行时值包装）⇒ 后续字段整体错位。
                代码.push_str(&format!(
                    "    {{\n\
                     \x20       let 字段指针 = {} as *const std::os::raw::c_char;\n\
                     \x20       let 字节: &[u8] = if 字段指针.is_null() {{ &[] }} else {{\n\
                     \x20           unsafe {{ std::ffi::CStr::from_ptr(字段指针).to_bytes() }}\n\
                     \x20       }};\n\
                     \x20       let mut 文本缓冲 = Vec::new();\n\
                     \x20       文本缓冲.extend_from_slice(&4u64.to_le_bytes());\n\
                     \x20       文本缓冲.extend_from_slice(&((16 + 字节.len()) as u64).to_le_bytes());\n\
                     \x20       文本缓冲.extend_from_slice(字节);\n\
                     \x20       let 文本泄漏 = Box::leak(文本缓冲.into_boxed_slice());\n\
                     \x20       扁平.extend_from_slice(&10u64.to_le_bytes());\n\
                     \x20       扁平.extend_from_slice(&(文本泄漏.as_ptr() as i64).to_le_bytes());\n\
                     \x20       扁平.extend_from_slice(&0.0f64.to_le_bytes());\n\
                     \x20       扁平.extend_from_slice(&0u64.to_le_bytes());\n\
                     \x20   }}\n",
                    字段访问
                ));
                continue;
            }
            if let C类型::结构体(嵌套名, 嵌套字段) = 字段类型 {
                // 🆕 ① 嵌套结构体字段：递归序列化到 线_结构体 子缓冲，与宿主 从外部值还原 严格对齐。
                // 子缓冲 = 序列化_嵌套名(值.字段_i as *const 嵌套名_布局)；外层包裹
                // [子标签=线_结构体(7)][总长][类型索引][字段数] 头（与返回值分支一致）。
                // 类型索引仅作堆对象标签（分配结构体 不据其查布局），顺序不一致无害。
                let 类型索引 = 外部结构体序号.get(嵌套名).copied().unwrap_or(0);
                let 字段数 = 嵌套字段.len();
                代码.push_str(&format!(
                    "    {{ let mut 子缓冲 = Vec::new(); 序列化_{}(&值.字段_{}, &mut 子缓冲); \
                     let mut 嵌套缓冲 = Vec::new(); \
                     嵌套缓冲.extend_from_slice(&7u64.to_le_bytes()); \
                     嵌套缓冲.extend_from_slice(&((16 + 子缓冲.len()) as u64).to_le_bytes()); \
                     嵌套缓冲.extend_from_slice(&({}u64).to_le_bytes()); \
                     嵌套缓冲.extend_from_slice(&({}u64).to_le_bytes()); \
                     嵌套缓冲.extend_from_slice(&子缓冲); \
                     let 嵌套泄漏 = Box::leak(嵌套缓冲.into_boxed_slice()); \
                     扁平.extend_from_slice(&10u64.to_le_bytes()); \
                     扁平.extend_from_slice(&(嵌套泄漏.as_ptr() as i64).to_le_bytes()); \
                     扁平.extend_from_slice(&0.0f64.to_le_bytes()); \
                     扁平.extend_from_slice(&0u64.to_le_bytes()); }}\n",
                    嵌套名, i, 类型索引, 字段数
                ));
                continue;
            }
            if let C类型::数组(元素) = 字段类型 {
                // 🆕 2026-09-25 数组字段（规范 §6.5.6 ③，A 指针口径）：
                //   C 侧字段 = 指针 `X*`；元素个数取「**紧邻**的 `无符号位宽整数` 字段」（规范
                //   §6.5「数组长度参数识别规则」紧邻模式：`T* data, size_t len`）。序列化为 线_数组 子缓冲
                //   · 信任边界：本方向（C → 智语言）由 **C 侧**保证 `长度` 与指针实际容量一致；
                //     规范要求的「长度 ≤ 数组实际长度」校验属**参数方向**（智语言 → C，容器自带长度）。
                //     [子标签=线_数组(5)][总长][数量 u64][各元素 运行时值(32B)]
                //   外层按 32 字节 运行时值{标签_外部值(10), 整数=子缓冲指针} 写入 ——
                //   与宿主 从外部值还原 的 线_数组 分支**严格对齐**（宿主自动 分配数组(元素)）。
                {
                    let 标量元素 = 元素.是整数类()
                        || matches!(元素.as_ref(),
                            C类型::小数32 | C类型::小数64 | C类型::逻辑 | C类型::指针
                            | C类型::C字符 | C类型::C无符号字符);
                    if !标量元素 {
                        诊断.push(format!(
                            "外部结构体「{}」的第 {} 个数组字段的元素类型 {:?} 暂不支持\
                             （当前仅支持标量外部类型的数组元素；文本/结构体元素为后续增量）。",
                            名, i + 1, 元素
                        ));
                        return;
                    }
                    // 规范 §6.5（数组长度参数识别规则）：**紧邻模式**仅当数组后紧跟
                    // `无符号位宽整数`（FFI 精确码 16）时成立；其它整数类型（如 `无符号短整数`）
                    // 按规范属**非紧邻模式**（长度语义由用户自管）。而「C 结构体 → 智语言」方向
                    // **没有长度来源** ⇒ 非紧邻无法还原数组 ⇒ 明确诊断，绝不静默按 0 元素或读越界。
                    if 字段.get(i + 1).filter(|t| matches!(t, C类型::C字节数量)).is_none() {
                        诊断.push(format!(
                            "外部结构体「{}」的第 {} 个字段是数组（`数组<…>` ↦ C 指针 `X*`），\
                             但其后**紧跟的字段不是 `无符号位宽整数`**。\
                             仅当数组后紧跟 `无符号位宽整数` 时才构成**紧邻模式**（经典 `T* data, size_t len`）；\
                             非紧邻模式在「C 结构体 → 智语言」方向无长度来源，无法还原数组。\
                             请在数组字段后紧跟一个 `无符号位宽整数` 字段表示元素个数。",
                            名, i + 1
                        ));
                        return;
                    }
                }
                代码.push_str(&format!(
                    "    {{\n\
                     \x20       let 数量 = {} as usize;\n\
                     \x20       let 基指针 = {};\n\
                     \x20       let mut 元素扁平 = Vec::new();\n\
                     \x20       元素扁平.extend_from_slice(&(数量 as u64).to_le_bytes());\n\
                     \x20       for 下标 in 0..数量 {{\n\
                     \x20           let 元素原始 = unsafe {{ *基指针.add(下标) }};\n\
                     \x20           let 元素值 = {};\n\
                     \x20           元素扁平.extend_from_slice(&元素值.标签.to_le_bytes());\n\
                     \x20           元素扁平.extend_from_slice(&元素值.整数.to_le_bytes());\n\
                     \x20           元素扁平.extend_from_slice(&元素值.小数.to_le_bytes());\n\
                     \x20           元素扁平.extend_from_slice(&元素值.句柄.to_le_bytes());\n\
                     \x20       }}\n\
                     \x20       let 数组总长 = (16 + 元素扁平.len()) as u64;\n\
                     \x20       let mut 数组头 = Vec::new();\n\
                     \x20       数组头.extend_from_slice(&5u64.to_le_bytes());\n\
                     \x20       数组头.extend_from_slice(&数组总长.to_le_bytes());\n\
                     \x20       数组头.extend_from_slice(&元素扁平);\n\
                     \x20       let 数组泄漏 = Box::leak(数组头.into_boxed_slice());\n\
                     \x20       扁平.extend_from_slice(&10u64.to_le_bytes());\n\
                     \x20       扁平.extend_from_slice(&(数组泄漏.as_ptr() as i64).to_le_bytes());\n\
                     \x20       扁平.extend_from_slice(&0.0f64.to_le_bytes());\n\
                     \x20       扁平.extend_from_slice(&0u64.to_le_bytes());\n\
                     \x20   }}\n",
                    format!("值.字段_{}", i + 1),
                    字段访问,
                    Self::生成字段运行时值表达式("元素原始", 元素),
                ));
                continue;
            }
            // 构造该字段的 运行时值（32字节 = 标签/整数/小数/句柄 4×u64）
            代码.push_str(&format!("    let 临时值 = {};\n", Self::生成字段运行时值表达式(&字段访问, 字段类型)));
            // 直接按 32 字节内存布局写入（与 base 运行时值 布局一致，宿主 transmute 还原）
            代码.push_str("    let 临时字节 = unsafe { std::slice::from_raw_parts(&临时值 as *const 运行时值 as *const u8, 32) };\n");
            代码.push_str("    扁平.extend_from_slice(临时字节);\n");
        }
        代码.push_str("}\n\n");

        // 🆕 ① 递归生成嵌套结构体的序列化 helper（供本结构体的嵌套字段复用）
        for 字段类型 in 字段 {
            if let C类型::结构体(嵌套名, 嵌套字段) = 字段类型 {
                Self::生成结构体序列化辅助(嵌套名, 嵌套字段, 外部结构体序号, 代码, 诊断, 已生成);
            }
        }
    }

    /// 生成「把 C 原生字段值转为 运行时值 字面量」的 Rust 表达式
    fn 生成字段运行时值表达式(访问: &str, 字段类型: &C类型) -> String {
        match 字段类型 {
            t if t.是无符号整数类() =>
                format!("运行时值 {{ 标签: 1, 整数: ({} ) as u64 as i64, 小数: 0.0, 句柄: 0 }}", 访问),
            t if t.是整数类() =>
                format!("运行时值 {{ 标签: 1, 整数: ({} ) as i64, 小数: 0.0, 句柄: 0 }}", 访问),
            C类型::小数32 =>
                format!("运行时值 {{ 标签: 2, 整数: 0, 小数: ({} ) as f64, 句柄: 0 }}", 访问),
            C类型::小数64 =>
                format!("运行时值 {{ 标签: 2, 整数: 0, 小数: ({} ), 句柄: 0 }}", 访问),
            C类型::逻辑 =>
                format!("运行时值 {{ 标签: 3, 整数: if ({} ) != 0 {{ 1 }} else {{ 0 }}, 小数: 0.0, 句柄: 0 }}", 访问),
            C类型::指针 =>
                // 指针由整数承载（规范 §6.5.8）；不得标成堆句柄(4)，否则回传 C 时会被当成 0 号堆对象编组。
                format!("运行时值 {{ 标签: 1, 整数: ({} ) as i64, 小数: 0.0, 句柄: 0 }}", 访问),
            C类型::C字符 =>
                format!("运行时值 {{ 标签: 1, 整数: ({} ) as i8 as i64, 小数: 0.0, 句柄: 0 }}", 访问),
            C类型::C无符号字符 =>
                format!("运行时值 {{ 标签: 1, 整数: ({} ) as u8 as i64, 小数: 0.0, 句柄: 0 }}", 访问),
            // 上文已 panic 拦截复杂字段，此处不可达
            _ => "运行时值 { 标签: 1, 整数: 0, 小数: 0.0, 句柄: 0 }".to_string(),
        }
    }

    /// 由「库标识 + C 符号名」派生**确定性且 ASCII 安全**的 Rust extern 项名。
    ///
    /// 为何不能直接用 C 符号名作 Rust 名：C 符号可为中文（导出块 `关联` 缺省即中文函数名），
    /// 而 Rust 禁止 extern 块内非 ASCII 标识符。
    /// 为何不能用序号：`FFI项.键` 是**块内**序号，跨外部块文件会重复；而 `按库分组` 是
    /// HashMap，组内序号与全局序号既不一致、遍历顺序又随机。两者都会让 shim 重复定义
    /// 或调错符号（表现为 shim 编译失败回退 transmute、或间歇性 0xC0000005）。
    /// 以「库标识+符号」派生则天然全局唯一、与遍历顺序无关，且声明端与调用端可各自独立算出同名。
    /// 【G10】把库标识转换为 **ASCII 安全的链接库名**，用于 `#[link(name = ...)]`。
    ///
    /// 起因（实测最小复现确证，非推测）：`#[link(name = "样例库", kind = "raw-dylib")]`
    /// 会让 rustc 生成**损坏的**导入库 `样例库.dll_imports.lib`，链接器报
    /// `LNK1107: 文件无效或损坏: 无法在 0x588 处读取`；把同一个 .dll 改名为 ASCII
    /// （如 `sample`）后完全正常。即 rustc 在把非 ASCII 库名写入 COFF 归档成员名时编码有误。
    /// 智语言允许中文库代号，故必须在生成侧规避：链接名一律转义为 ASCII。
    ///
    /// 纯 ASCII 名原样返回（既有 sqlite3 等零影响，且不破坏用户手放的 `<名>.lib`）；
    /// 含非 ASCII 时转义为 `z_lib_<码点序列>`，并由部署侧按同名落一份库文件副本。
    pub fn 链接库安全名(库标识: &str) -> String {
        if 库标识.chars().all(|c| c.is_ascii_alphanumeric() || c == '_' || c == '-' || c == '.') {
            return 库标识.to_string();
        }
        let mut 名 = String::from("z_lib_");
        for c in 库标识.chars() {
            if c.is_ascii_alphanumeric() || c == '_' {
                名.push(c);
            } else {
                名.push_str(&format!("_u{:x}_", c as u32));
            }
        }
        名
    }

    fn 外部符号Rust名(库标识: &str, c符号: &str) -> String {
        let mut 名 = String::from("z_imp_");
        for c in 库标识.chars().chain(std::iter::once('\u{0}')).chain(c符号.chars()) {
            if c.is_ascii_alphanumeric() || c == '_' {
                名.push(c);
            } else {
                名.push_str(&format!("_u{:x}_", c as u32));
            }
        }
        名
    }

    fn 生成导入包装(f: &导入函数元数据, 外部结构体序号: &HashMap<String, usize>, 代码: &mut String) {
        let 函数名 = format!("z_shim_import_{}", f.键);

        代码.push_str(&format!("unsafe extern \"C\" fn {}(", 函数名));
        代码.push_str("输出: *mut 运行时值, ");
        代码.push_str("_数量: i64, ");
        代码.push_str("输入数组: *mut 运行时值) {\n");

        // 真实 C ABI（静态/动态链接统一）：在此做 运行时值 ↔ 原生C类型 编组。
        // 宿主 处理外部调用 已对每个参数先 编组为外部值（标签_外部值 扁平缓冲），
        // 本包装读取缓冲还原原生类型后调用真实 C 符号（动态库经 raw-dylib 运行期解析）。
        代码.push_str("    let 参数切片 = std::slice::from_raw_parts(输入数组, _数量 as usize);\n");
        // 非输出参数：读取值作为实参；输出参数（C 指针 out 参数）：生成本地临时变量传指针，调用后写回 输出 槽。
        let mut 调用实参: Vec<String> = Vec::new();
        let mut 输出写回: Vec<String> = Vec::new();
        let mut 输出序号 = 0usize;
        for (i, t) in f.形参类型.iter().enumerate() {
            let 是输出 = f.输出位图.get(i).copied().unwrap_or(false);
            if 是输出 {
                // 输出参数：本地临时变量 + 指针实参；调用后按 输出[1+序号] 写回
                let 临时名 = format!("out{}", i);
                let 指针名 = format!("pout{}", i);
                代码.push_str(&format!(
                    "    let mut {}: {} = {};\n", 临时名, t.取Rust类型名(), t.取默认值字面量()
                ));
                代码.push_str(&format!(
                    "    let {} = &mut {} as *mut {};\n", 指针名, 临时名, t.取Rust类型名()
                ));
                调用实参.push(指针名);
                输出写回.push(format!(
                    "    if 输出 != std::ptr::null_mut() {{ *输出.offset({} as isize) = {}; }}\n",
                    1 + 输出序号,
                    Self::生成运行时值写入(&临时名, t, 外部结构体序号)
                ));
                输出序号 += 1;
            } else {
                let 数组元素码 = f.形参数组元素C类型.get(i).copied().unwrap_or(0);
                let 表达式 = Self::生成参数读取代码(i, t, 数组元素码);
                let 变量名 = format!("a{}", i);
                if matches!(t, C类型::字符串) && 数组元素码 == 0 {
                    // 文本形参：局部持有 CString（调用后释放），向 C 传其指针；None → NULL
                    let 持有名 = format!("s{}", i);
                    代码.push_str(&format!("    let {}: Option<std::ffi::CString> = {};\n", 持有名, 表达式));
                    代码.push_str(&format!(
                        "    let {} = match &{} {{ Some(c) => c.as_ptr() as *const i8, None => std::ptr::null() }};\n",
                        变量名, 持有名
                    ));
                } else {
                    代码.push_str(&format!("    let {} = {};\n", 变量名, 表达式));
                }
                调用实参.push(变量名);
            }
        }
        let 调用实参串 = 调用实参.join(", ");
        // 🆕 FFI: 经 ASCII Rust 标识符调用真实 C 符号（由 #[link_name] 绑定）
        let rust_fn = Self::外部符号Rust名(&f.库标识, &f.c符号);
        if f.是否全局变量 {
            // 🆕 FFI（§6.5.2 第5项）：C 全局变量直接读 `static` 值（零参），不调用函数。
            if f.返回类型 == C类型::空值 {
                代码.push_str("    // void 全局变量，无操作\n");
            } else {
                代码.push_str(&format!("    let 结果 = {};\n", rust_fn));
                代码.push_str("    {\n");
                Self::生成返回值写入代码("结果".to_string(), &f.返回类型, 外部结构体序号, 代码);
                代码.push_str("    }\n");
            }
        } else if f.返回类型 == C类型::空值 {
            代码.push_str(&format!("    {}({});\n", rust_fn, 调用实参串));
        } else {
            代码.push_str(&format!("    let 结果 = {}({});\n", rust_fn, 调用实参串));
            代码.push_str("    {\n");
            Self::生成返回值写入代码("结果".to_string(), &f.返回类型, 外部结构体序号, 代码);
            代码.push_str("    }\n");
        }
        // 输出参数写回（C 指针参数 → 智语言 输出 槽）
        for 行 in &输出写回 {
            代码.push_str(行);
        }
        代码.push_str("}\n\n");
    }

    fn 生成参数读取代码(索引: usize, 类型: &C类型, 数组元素码: u8) -> String {
        // 🆕 2026-08-29 FFI 数组：宿主已把智语言数组序列化为 C ABI 紧凑缓冲（地址放参数槽
        // 标签_整数），此处还原为 *const <元素Rust类型> 传给 C。元素码 → C类型 → Rust 类型名
        // （如 3/短整数 → i32 → *const i32），与 精确类型码到C类型 严格对齐。
        if 数组元素码 != 0 {
            let 元素类型名 = super::外部调用注册::精确类型码到C类型(数组元素码).取Rust类型名();
            return format!("参数切片[{}].整数 as *const {}", 索引, 元素类型名);
        }
        match 类型 {
            // ★符号扩展安全：运行时值 整数 字段是 i64，按声明类型（含 c_long/c_char 等平台相关）
            // 截断/扩展后传给 C 侧；有符号类型 `as` 保持符号语义（负值正确）。
            t if t.是整数类() => {
                format!("参数切片[{}].整数 as {}", 索引, 类型.取Rust类型名())
            }
            C类型::小数32 => {
                format!("参数切片[{}].小数 as f32", 索引)
            }
            C类型::小数64 => {
                format!("参数切片[{}].小数", 索引)
            }
            C类型::逻辑 => {
                format!("参数切片[{}].整数 != 0", 索引)
            }
            C类型::指针 => {
                format!("参数切片[{}].整数 as *mut c_void", 索引)
            }
            C类型::字符串 => {
                // 宿主已把 文本 编组为 标签_外部值 扁平缓冲（[子标签u64][总长u64][UTF-8字节]），
                // 总长 = 16 + UTF-8字节数。此处从缓冲头读总长，跳过 16 字节头取 payload，
                // 重建 NUL 结尾的 CString 指针传给 C 侧（C 侧 const char* 需 NUL 结尾）。
                // 结果是 Option<CString>：由包装函数的局部变量持有，调用返回后自动释放（规范 §6.5.8「调用后释放」）；
                // 实参不是外部值缓冲（如传入空值）→ None → 传 NULL。内嵌 NUL 时按 C 语义截断到首个 NUL。
                format!(
                    "{{ let 缓冲 = 参数切片[{}]; \
                      if 缓冲.标签 != 10 || 缓冲.整数 == 0 {{ None }} else {{ \
                      let 头 = std::slice::from_raw_parts(缓冲.整数 as *const u8, 16); \
                      let 总长 = u64::from_le_bytes([头[8],头[9],头[10],头[11],头[12],头[13],头[14],头[15]]) as usize; \
                      if 总长 < 16 {{ None }} else {{ \
                      let 字节 = std::slice::from_raw_parts(缓冲.整数 as *const u8, 总长); \
                      let 负载 = &字节[16..]; \
                      let 截止 = 负载.iter().position(|&b| b == 0).unwrap_or(负载.len()); \
                      std::ffi::CString::new(&负载[..截止]).ok() }} }} }}",
                    索引
                )
            }
            C类型::结构体(名, _) => {
                // 结构体通过指针传递
                format!("std::ptr::read(参数切片[{}].整数 as *const {}_布局)", 索引, 名)
            }
            C类型::函数指针(形状) => {
                // ★transmute 目标签名必须与 取Rust类型名 完全一致（C 侧按此签名调用回调）。
                // 旧代码写死 `fn()` 与 trampoline 实际签名不符 → 调用即 UB；现按形状码映射。
                format!("std::mem::transmute::<u64, {}>(参数切片[{}].整数 as u64)", C类型::形状到函数签名(*形状), 索引)
            }
            C类型::空值 => {
                "0".to_string()
            }
            // 平台相关整数类已由 是整数类 guard 覆盖；此处兜底保证 match 穷尽（不可达）
            _ => "0".to_string(),
        }
    }

    /// 生成 Rust 代码，把原生类型临时变量转换为 运行时值（用于输出参数写回 输出 槽）
    /// ★符号扩展规则：有符号类型 `as i64` 符号扩展（C int=-1 → 智语言整数=-1）；
    /// 无符号类型先 `as u64` 再 `as i64` 无符号扩展（C uint32=4294967295 → 智语言整数=4294967295）。
    fn 生成运行时值写入(变量名: &str, 类型: &C类型, 外部结构体序号: &HashMap<String, usize>) -> String {
        if 类型.是无符号整数类() {
            return format!("运行时值::整数({} as u64 as i64)", 变量名);
        }
        match 类型 {
            t if t.是整数类() => {
                format!("运行时值::整数({} as i64)", 变量名)
            }
            C类型::指针 => {
                format!("运行时值::整数({} as i64)", 变量名)
            }
            C类型::小数32 => {
                format!("运行时值::小数({} as f64)", 变量名)
            }
            C类型::小数64 => {
                format!("运行时值::小数({})", 变量名)
            }
            C类型::逻辑 => {
                format!("运行时值::逻辑({})", 变量名)
            }
            C类型::字符串 => {
                // 【G3】字符串输出参数写回：C 侧把 char* 写进临时槽，此处拷贝成
                // 标签_外部值 扁平缓冲 [子标签=线_文本(4)][总长=16+字节数][UTF-8]，
                // 与 生成返回值写入代码 的字符串分支、宿主 从外部值还原/释放外部值 严格一致。
                // C 侧未写（保持空指针）时写回 空值，避免解引用 NULL。
                format!(
                    "{{ if ({} as *const i8).is_null() {{ 运行时值::空值() }} else {{ \
                       let s = std::ffi::CStr::from_ptr({} as *const i8).to_string_lossy().to_string(); \
                       let 字节 = s.as_bytes(); \
                       let mut 缓冲 = Vec::with_capacity(16 + 字节.len()); \
                       缓冲.extend_from_slice(&4u64.to_le_bytes()); \
                       缓冲.extend_from_slice(&((16 + 字节.len()) as u64).to_le_bytes()); \
                       缓冲.extend_from_slice(字节); \
                       let 泄漏 = Box::leak(缓冲.into_boxed_slice()); \
                       运行时值 {{ 标签: 10, 整数: 泄漏.as_ptr() as i64, 小数: 0.0, 句柄: 0 }} }} }}",
                    变量名, 变量名
                )
            }
            C类型::结构体(名, 字段) => {
                // 【G3】结构体输出参数写回：递归序列化到 线_结构体 扁平缓冲
                // [子标签=7][总长][类型索引][字段数][字段×32字节运行时值]，与返回值分支一致。
                let 类型索引 = 外部结构体序号.get(名).copied().unwrap_or(0);
                let 字段数 = 字段.len();
                format!(
                    "{{ let mut 扁平缓冲 = Vec::new(); \
                       序列化_{}(&{}, &mut 扁平缓冲); \
                       let 总长 = (16 + 扁平缓冲.len()) as u64; \
                       let mut 外部值缓冲 = Vec::new(); \
                       外部值缓冲.extend_from_slice(&7u64.to_le_bytes()); \
                       外部值缓冲.extend_from_slice(&总长.to_le_bytes()); \
                       外部值缓冲.extend_from_slice(&{}u64.to_le_bytes()); \
                       外部值缓冲.extend_from_slice(&{}u64.to_le_bytes()); \
                       外部值缓冲.extend_from_slice(&扁平缓冲); \
                       let 泄漏 = Box::leak(外部值缓冲.into_boxed_slice()); \
                       运行时值 {{ 标签: 10, 整数: 泄漏.as_ptr() as i64, 小数: 总长 as f64, 句柄: 0 }} }}",
                    名, 变量名, 类型索引, 字段数
                )
            }
            // 函数指针/空值等无写回语义，兜底空值
            _ => "运行时值::空值()".to_string(),
        }
    }

    fn 生成返回值写入代码(值表达式: String, 类型: &C类型, 外部结构体序号: &HashMap<String, usize>, 代码: &mut String) {
        match 类型 {
            C类型::空值 => {
                代码.push_str("    // void 返回，无操作\n");
            }
            // ★符号扩展（G 修正）：有符号 C 类型返回 `as i64` 符号扩展（int=-1 → 整数=-1）；
            // 无符号 C 类型先 `as u64` 再 `as i64` 无符号扩展（uint32=4294967295 → 整数=4294967295）。
            t if t.是无符号整数类() => {
                代码.push_str(&format!("    if !输出.is_null() {{ *输出 = 运行时值::整数({} as u64 as i64); }}\n", 值表达式));
            }
            t if t.是整数类() => {
                代码.push_str(&format!("    if !输出.is_null() {{ *输出 = 运行时值::整数({} as i64); }}\n", 值表达式));
            }
            C类型::小数32 | C类型::小数64 => {
                代码.push_str(&format!("    if !输出.is_null() {{ *输出 = 运行时值::小数({} as f64); }}\n", 值表达式));
            }
            C类型::逻辑 => {
                代码.push_str(&format!("    if !输出.is_null() {{ *输出 = 运行时值::逻辑({} != 0); }}\n", 值表达式));
            }
            C类型::指针 => {
                // 指针由整数承载（规范 §6.5.8）：与输出参数分支一致。旧实现标成堆句柄(4)且句柄字段为 0，
                // 该值再传回 C 时 编组为外部值 会去读 0 号堆对象，C 侧收到无关缓冲或 NULL。
                代码.push_str(&format!("    if !输出.is_null() {{ *输出 = 运行时值::整数({} as i64); }}\n", 值表达式));
            }
            C类型::字符串 => {
                // C字符串 → 智语言文本：需拷贝
                // C 返回 NULL（getenv 未命中等）→ 空值；对 NULL 调 CStr::from_ptr 是未定义行为。
                代码.push_str(&format!("    let 文本指针 = {};\n", 值表达式));
                代码.push_str("    if !输出.is_null() && 文本指针.is_null() {\n");
                代码.push_str("        *输出 = 运行时值 { 标签: 0, 整数: 0, 小数: 0.0, 句柄: 0 };\n");
                代码.push_str("    } else if !输出.is_null() {\n");
                代码.push_str("        let c_str = std::ffi::CStr::from_ptr(文本指针);\n");
                代码.push_str("        let s = c_str.to_string_lossy().to_string();\n");
                // ★文本通过外部值传递，缓冲格式与宿主 编组为外部值/释放外部值 严格一致：
                //   [子标签 u64=4(线_文本)][总长 u64=16+字节数][UTF-8 字节]
                //   否则 释放外部值 读 总长(offset 8) 得到裸字节内容 → from_raw_parts_mut 越界 panic。
                代码.push_str("        let 字节 = s.as_bytes();\n");
                代码.push_str("        let mut 缓冲 = Vec::with_capacity(16 + 字节.len());\n");
                代码.push_str("        缓冲.extend_from_slice(&4u64.to_le_bytes());\n");
                代码.push_str("        缓冲.extend_from_slice(&((16 + 字节.len()) as u64).to_le_bytes());\n");
                代码.push_str("        缓冲.extend_from_slice(字节);\n");
                代码.push_str("        let 泄漏 = Box::leak(缓冲.into_boxed_slice());\n");
                代码.push_str("        *输出 = 运行时值 { 标签: 10, 整数: 泄漏.as_ptr() as i64, 小数: 0.0, 句柄: 0 };\n");
                代码.push_str("    }\n");
            }
            C类型::结构体(名, 字段) => {
                // 【缺口 D 修复】外部结构体返回值：递归序列化到 线_结构体 扁平缓冲
                // [子标签=线_结构体(7)][总长][类型索引][字段数][字段数×32字节运行时值...]，
                // 与宿主 从外部值还原 的 线_结构体 分支严格对齐（子标签=7、字段数=真实字段个数）。
                // 原实现把裸指针写入 标签_外部值 缺头缓冲且布局不符 → 还原越界 panic。
                let 类型索引 = 外部结构体序号.get(名).copied().unwrap_or(0);
                let 字段数 = 字段.len();
                代码.push_str(&format!("    if !输出.is_null() {{\n"));
                代码.push_str(&format!("        let 结构体值 = {};\n", 值表达式));
                代码.push_str("        let mut 扁平缓冲 = Vec::new();\n");
                代码.push_str(&format!("        序列化_{}(&结构体值, &mut 扁平缓冲);\n", 名));
                代码.push_str("        let 总长 = (16 + 扁平缓冲.len()) as u64;\n");
                代码.push_str("        let mut 外部值缓冲 = Vec::new();\n");
                代码.push_str("        外部值缓冲.extend_from_slice(&7u64.to_le_bytes()); // 子标签 = 线_结构体(=7, 与宿主 从外部值还原 对齐)\n");
                代码.push_str("        外部值缓冲.extend_from_slice(&总长.to_le_bytes());\n");
                代码.push_str(&format!("        外部值缓冲.extend_from_slice(&{}u64.to_le_bytes()); // 类型索引\n", 类型索引));
                代码.push_str(&format!("        外部值缓冲.extend_from_slice(&{}u64.to_le_bytes()); // 字段数 = 真实字段个数\n", 字段数));
                代码.push_str("        外部值缓冲.extend_from_slice(&扁平缓冲);\n");
                // ★`Box::into_raw(boxed_slice) as u64` 是胖指针转整数（E0606），shim 直接编译失败；
                //   必须先经 Box::leak 取切片再 as_ptr() 取瘦指针。
                代码.push_str("        let 泄漏 = Box::leak(外部值缓冲.into_boxed_slice());\n");
                代码.push_str("        *输出 = 运行时值 { 标签: 10, 整数: 泄漏.as_ptr() as i64, 小数: 总长 as f64, 句柄: 0 };\n");
                代码.push_str("    }\n");
            }
            C类型::函数指针(_) => {
                代码.push_str(&format!("    if !输出.is_null() {{ *输出 = 运行时值 {{ 标签: 6, 整数: ({}).map(|f| f as *const () as usize).unwrap_or(0) as i64, 小数: 0.0, 句柄: 0 }}; }}\n", 值表达式));
            }
            // 平台相关整数类已由 guard 覆盖；兜底保证 match 穷尽（不可达）
            _ => {}
        }
    }

    fn 生成导出thunk(f: &导出函数元数据, 代码: &mut String) {
        // 【G5】导出 thunk 以真实 C 符号名（c符号）导出，与编译模式 Cranelift 生成的导出符号对齐，
        // 使 C 侧在双模下都能按同一符号名（如 math_add）调用。
        let 函数名 = f.c符号.clone();
        let 返回类型 = f.返回类型.取Rust类型名();

        // 构建 C ABI 签名
        let 形参声明: Vec<String> = f.形参类型.iter().enumerate()
            .map(|(i, t)| format!("p{}: {}", i, t.取Rust类型名()))
            .collect();

        代码.push_str(&format!("#[export_name = \"{}\"]\n", 函数名));
        代码.push_str(&format!("pub unsafe extern \"C\" fn {}({}) -> {} {{\n",
            函数名, 形参声明.join(", "), 返回类型));

        // 编组为运行时值
        代码.push_str("    let mut 参数列表 = vec![\n");
        for (i, _类型) in f.形参类型.iter().enumerate() {
            let 标签 = _类型.取智语言类型标签();
            match _类型 {
                // 无符号类先 as u64 再 as i64（无符号扩展，避免负值化）；有符号类直接 as i64
                t if t.是无符号整数类() => {
                    代码.push_str(&format!("        运行时值 {{ 标签: {}, 整数: p{} as u64 as i64, 小数: 0.0, 句柄: 0 }},\n", 标签, i));
                }
                t if t.是整数类() => {
                    代码.push_str(&format!("        运行时值 {{ 标签: {}, 整数: p{} as i64, 小数: 0.0, 句柄: 0 }},\n", 标签, i));
                }
                C类型::小数32 | C类型::小数64 => {
                    代码.push_str(&format!("        运行时值 {{ 标签: {}, 整数: 0, 小数: p{} as f64, 句柄: 0 }},\n", 标签, i));
                }
                C类型::逻辑 => {
                    代码.push_str(&format!("        运行时值 {{ 标签: {}, 整数: if p{} {{ 1 }} else {{ 0 }}, 小数: 0.0, 句柄: 0 }},\n", 标签, i));
                }
                C类型::指针 => {
                    代码.push_str(&format!("        运行时值 {{ 标签: {}, 整数: p{} as i64, 小数: 0.0, 句柄: 0 }},\n", 标签, i));
                }
                C类型::字符串 => {
                    代码.push_str(&format!("        // 字符串参数 #{} 按标签_外部值传递\n", i));
                    代码.push_str(&format!("        {{ let c_str = CString::from_raw(p{} as *mut i8);\n", i));
                    代码.push_str("             let s = c_str.to_string_lossy().to_string();\n");
                    代码.push_str("             let 字节 = s.into_bytes();\n");
                    代码.push_str("             let 泄漏 = Box::leak(字节.into_boxed_slice());\n");
                    代码.push_str("             std::mem::forget(c_str); // 不释放原始 C 内存\n");
                    代码.push_str("             运行时值 { 标签: 10, 整数: 泄漏.as_ptr() as i64, 小数: 0.0, 句柄: 0 }\n");
                    代码.push_str("        },\n");
                }
                C类型::结构体(名, 字段) => {
                    // 结构体通过指针传递
                    代码.push_str(&format!("        // 结构体参数 #{}: {}\n", i, 名));
                    代码.push_str(&format!("        {{ let 结构体指针 = &p{} as *const {}_布局 as usize;\n", i, 名));
                    代码.push_str("             运行时值 { 标签: 10, 整数: 结构体指针 as i64, 小数: 0.0, 句柄: 0 }\n");
                    代码.push_str("        },\n");
                    let _ = 字段;
                }
                C类型::函数指针(_) => {
                    代码.push_str(&format!("        运行时值 {{ 标签: 6, 整数: p{} as i64, 小数: 0.0, 句柄: 0 }},\n", i));
                }
                C类型::空值 => {
                    代码.push_str("        运行时值::空值(),\n");
                }
                // 平台相关整数类已由 guard 覆盖；兜底保证 match 穷尽（不可达）
                _ => {
                    代码.push_str(&format!("        运行时值 {{ 标签: 1, 整数: p{} as i64, 小数: 0.0, 句柄: 0 }},\n", i));
                }
            }
        }
        代码.push_str("    ];\n");

        // 调用智语言函数（经 智语言_系统调用回调 C ABI 入口，按导出函数索引执行真实用户函数）
        // 注：不能用系统调用 119（那是外部调用入口，会把函数索引当 FFI 命令码误路由）。
        代码.push_str(&format!("    let 函数索引: u64 = {};\n", f.导出序号));
        代码.push_str("    let mut 输出 = 运行时值::空值();\n");
        代码.push_str("    z_sysc_cb(函数索引, 参数列表.as_ptr(), 参数列表.len() as u64, &mut 输出);\n");

        // 返回值编组
        if 返回类型 != "()" {
            代码.push_str(&format!("    // 编组返回值 → {}\n", 返回类型));
            match &f.返回类型 {
                // 有符号/无符号统一 `as` 截断到目标位宽（Rust 整型 as 为按位截断，符合 C 语义）
                t if t.是整数类() => {
                    代码.push_str(&format!("    return 输出.整数 as {};\n", 返回类型));
                }
                C类型::小数32 => {
                    代码.push_str("    return 输出.小数 as f32;\n");
                }
                C类型::小数64 => {
                    代码.push_str("    return 输出.小数;\n");
                }
                C类型::逻辑 => {
                    代码.push_str("    return 输出.整数 != 0;\n");
                }
                C类型::指针 => {
                    代码.push_str("    return 输出.整数 as *mut c_void;\n");
                }
                C类型::字符串 => {
                    代码.push_str("    // 文本返回：从运行时值 标签_外部值 提取\n");
                    代码.push_str("    if 输出.标签 == 10 {\n");
                    代码.push_str("        return 输出.整数 as *const i8;\n");
                    代码.push_str("    }\n");
                    代码.push_str("    return std::ptr::null();\n");
                }
                C类型::结构体(_, _) => {
                    代码.push_str("    // 结构体返回：通过缓冲区\n");
                    代码.push_str("    return std::mem::zeroed();\n");
                }
                C类型::函数指针(_) => {
                    代码.push_str("    return 输出.整数 as usize as unsafe extern \"C\" fn();\n");
                }
                C类型::空值 => {}
                // 平台相关整数类已由 是整数类 guard 覆盖；此处兜底保证 match 穷尽
                _ => {}
            }
        }

        代码.push_str("}\n\n");
    }
}

#[cfg(test)]
mod 测试 {
    use super::*;

    /// 回调槽位按函数复用：同一函数重复注册必须先查已有槽位，再找空槽。
    /// 设置环境变量 ZHI_DUMP_TEST_SHIM=<路径> 时把生成的源码写到该路径，便于单独用 rustc 编译后做行为验证。
    #[test]
    fn 生成shim_回调槽位按函数复用() {
        let 代码 = FFI代码生成器::生成shim(&[], &[]);
        let 复用 = 代码.find("回调槽位表[形状usize][槽位] == 函数索引 && 函数索引 != 0").expect("缺少按函数复用槽位的查找");
        let 占用 = 代码.find("回调槽位表[形状usize][槽位] = 函数索引;").expect("缺少占用空槽");
        assert!(复用 < 占用, "必须先查已有槽位再占用空槽");
        if let Ok(路径) = std::env::var("ZHI_DUMP_TEST_SHIM") {
            std::fs::write(路径, &代码).unwrap();
        }
    }

    #[test]
    fn 生成shim_导入() {
        let 导入 = vec![
            导入函数元数据 {
                键: 200,
                c符号: "add".to_string(),
                库标识: "mylib".to_string(),
                形参类型: vec![C类型::整数64, C类型::整数64],
                返回类型: C类型::整数64,
                输出位图: vec![false, false],
                是否静态: false,
                原生约定: true,
                是否全局变量: false,
                形参数组元素C类型: vec![],
            },
        ];
        let 代码 = FFI代码生成器::生成shim(&导入, &[]);
        assert!(代码.contains("extern \"C\""));
        // extern 项用「库标识+C符号」派生的确定性 ASCII 名（Rust 禁止 extern 块非 ASCII 标识符），
        // 真实 C 符号经 #[link_name] 绑定；两者必须同时出现。
        assert!(代码.contains("#[link_name = \"add\"]"), "缺少 link_name 绑定:\n{}", 代码);
        assert!(
            代码.contains(&FFI代码生成器::外部符号Rust名("mylib", "add")),
            "缺少确定性 extern 项名:\n{}",
            代码
        );
        assert!(代码.contains("z_shim_import_200"));
        // 导入 shim 采用 flat 运行时值约定（flat C ABI），非真实 C 签名
        assert!(代码.contains("*mut 运行时值"));
    }

    /// 🆕 FFI（§6.5.2 第5项）：C 全局变量必须走 `extern "C" { static 符号: T }` 读取，
    /// 绝不能当 `fn` 调用（否则调用变量地址处的机器码 → UB/崩溃/垃圾值）。
    #[test]
    fn 生成shim_外部全局变量() {
        let 导入 = vec![
            导入函数元数据 {
                键: 201,
                c符号: "sqlite3_version".to_string(),
                库标识: "数据库".to_string(),
                形参类型: vec![],
                返回类型: C类型::字符串,
                输出位图: vec![],
                是否静态: false,
                原生约定: true,
                是否全局变量: true,
                形参数组元素C类型: vec![],
            },
            导入函数元数据 {
                键: 202,
                c符号: "counter".to_string(),
                库标识: "库".to_string(),
                形参类型: vec![],
                返回类型: C类型::整数64,
                输出位图: vec![],
                是否静态: false,
                原生约定: true,
                是否全局变量: true,
                形参数组元素C类型: vec![],
            },
        ];
        let 代码 = FFI代码生成器::生成shim(&导入, &[]);
        // 全局变量：extern 块里必须是 `static 符号`，不允许出现 `fn 符号(`
        let 版本名 = FFI代码生成器::外部符号Rust名("数据库", "sqlite3_version");
        let 计数名 = FFI代码生成器::外部符号Rust名("库", "counter");
        assert!(代码.contains(&format!("static {}: *const i8;", 版本名)),
            "字符串全局变量应生成 static 指针:\n{}", 代码);
        assert!(代码.contains(&format!("static {}: i64;", 计数名)),
            "整数全局变量应生成 static i64:\n{}", 代码);
        assert!(!代码.contains(&format!("fn {}(", 版本名)), "全局变量不得生成 fn 调用");
        assert!(!代码.contains(&format!("fn {}(", 计数名)), "全局变量不得生成 fn 调用");
        // 包装函数应直接读 `static` 值（零参），不调用函数
        assert!(代码.contains("let 结果 = "), "包装函数应读取 static 值:\n{}", 代码);
    }

    #[test]
    fn 生成shim_导出示() {
        let 导出 = vec![
            导出函数元数据 {
                导出序号: 0,
                智语言函数名: "加法".to_string(),
                c符号: "math_add".to_string(),
                形参类型: vec![C类型::整数64, C类型::整数64],
                返回类型: C类型::整数64,
            },
        ];
        let 代码 = FFI代码生成器::生成shim(&[], &导出);
        // 【G5】导出符号用真实 c符号（math_add），与编译模式 Cranelift 导出符号对齐，非 z_export_N 桩名。
        assert!(代码.contains("math_add"), "导出符号应为真实 c符号:\n{}", 代码);
        assert!(代码.contains("#[export_name = \"math_add\"]"), "缺少 export_name:\n{}", 代码);
        assert!(代码.contains("unsafe extern \"C\" fn"), "缺少 extern 函数:\n{}", 代码);
        // 【G5】解释模式 thunk 经 智语言_系统调用回调 C ABI 入口调真实智语言导出函数（非系统调用 119）。
        assert!(代码.contains("z_sysc_cb("), "thunk 应经 智语言_系统调用回调 调用:\n{}", 代码);
        assert!(代码.contains("#[link_name = \"智语言_系统调用回调\"]"), "缺少回调符号绑定:\n{}", 代码);
    }

    #[test]
    fn 类型映射() {
        assert_eq!(C类型::整数8.取Rust类型名(), "i8");
        assert_eq!(C类型::整数64.取Rust类型名(), "i64");
        assert_eq!(C类型::小数64.取Rust类型名(), "f64");
        assert_eq!(C类型::逻辑.取Rust类型名(), "bool");
        assert_eq!(C类型::指针.取Rust类型名(), "*mut std::ffi::c_void");
        assert_eq!(C类型::字符串.取Rust类型名(), "*const i8");
        // ★平台相关变体 → 长整型/字符映射 std::os::raw，字节数量 = usize（size_t 语义）
        assert_eq!(C类型::C长整数.取Rust类型名(), "std::os::raw::c_long");
        assert_eq!(C类型::C字节数量.取Rust类型名(), "usize");
        assert_eq!(C类型::C有符号字节数量.取Rust类型名(), "isize");
        assert_eq!(C类型::C字符.取Rust类型名(), "std::os::raw::c_char");
        assert_eq!(C类型::C无符号字符.取Rust类型名(), "std::os::raw::c_uchar");
    }

    #[test]
    fn 类型标签() {
        assert_eq!(C类型::整数64.取智语言类型标签(), 1);
        assert_eq!(C类型::小数64.取智语言类型标签(), 2);
        assert_eq!(C类型::逻辑.取智语言类型标签(), 3);
        assert_eq!(C类型::指针.取智语言类型标签(), 4);
        assert_eq!(C类型::字符串.取智语言类型标签(), 10);
        // 平台相关整数类 → 标签 1（整数）
        assert_eq!(C类型::C长整数.取智语言类型标签(), 1);
        assert_eq!(C类型::C字节数量.取智语言类型标签(), 1);
        assert_eq!(C类型::C无符号字符.取智语言类型标签(), 1);
    }

    #[test]
    fn 符号扩展分类() {
        assert!(C类型::整数32.是整数类());
        assert!(C类型::C长整数.是整数类());
        assert!(!C类型::C长整数.是无符号整数类());
        assert!(C类型::无符号32.是无符号整数类());
        assert!(C类型::C字节数量.是无符号整数类());
        assert!(C类型::C有符号字节数量.是整数类());
        assert!(!C类型::C有符号字节数量.是无符号整数类());
        assert!(!C类型::小数64.是整数类());
    }

    #[test]
    fn 参数读取代码() {
        let 读取 = FFI代码生成器::生成参数读取代码(0, &C类型::整数64, 0);
        assert_eq!(读取, "参数切片[0].整数 as i64");
        let 读取 = FFI代码生成器::生成参数读取代码(1, &C类型::小数64, 0);
        assert_eq!(读取, "参数切片[1].小数");
        let 读取 = FFI代码生成器::生成参数读取代码(2, &C类型::逻辑, 0);
        assert_eq!(读取, "参数切片[2].整数 != 0");
    }
}
