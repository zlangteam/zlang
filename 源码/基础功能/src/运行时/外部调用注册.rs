// 文件: 基础功能/src/运行时/外部调用注册.rs
// 职责: FFI 外部函数动态库加载与调用（双模式共用真相）
//
// 设计: 解释模式在模块加载时经 Rust API 注册；编译模式在 启动程序 prologue
// 经 C ABI 注册。两者都写入同一张全局注册表，调用时统一由 处理外部调用
// （系统调用索引 119）落点，保证双模式语义一致。
//
// 动态库加载使用裸平台 API（Windows LoadLibraryW/GetProcAddress，
// POSIX dlopen/dlsym），零额外依赖，适配离线 vendored 构建环境。

use crate::值类型::*;
use once_cell::sync::Lazy;
use std::collections::HashMap;
use std::sync::Mutex;

/// W4 FFI 导出块：跨 C ABI 边界的「序列化堆对象」根标记（见 值类型.rs 标签_外部值）。
/// 旧 `标签_外部文本=6` 已废弃——它与 值类型.rs 的 标签_函数指针=6 撞车，
/// 现统一由「统一 FFI 编组层」以 标签_外部值 表示任意堆类型（文本/数组/映射/结构体）。

// ============================================================
//  平台相关：动态库加载（零依赖）
// ============================================================
#[cfg(windows)]
mod 平台 {
    use std::os::raw::c_void;
    extern "system" {
        fn LoadLibraryExW(lpFileName: *const u16, hFile: *mut c_void, dwFlags: u32) -> *mut c_void;
        fn GetProcAddress(hModule: *mut c_void, lpProcName: *const u8) -> *mut c_void;
        fn FreeLibrary(hModule: *mut c_void) -> i32;
        fn AddDllDirectory(NewDirectory: *const u16) -> *mut c_void;
        fn RemoveDllDirectory(Cookie: *mut c_void) -> i32;
        fn GetLastError() -> u32;
    }

    // 加载标志：强制优先搜索 dll 自身所在目录（解析其内部导入的外部库 dll），
    // 再回退系统默认搜索顺序（System32/KnownDlls）。缺省 LoadLibraryW 不把
    // "调用方目录" 纳入搜索路径（SafeDllSearchMode 安全策略），导致 shim cdylib
    // 导入的外部库 dll（如 全局变量库.dll 的安全名副本）解析失败 → 126。
    const LOAD_LIBRARY_SEARCH_DLL_LOAD_DIR: u32 = 0x100;
    const LOAD_LIBRARY_SEARCH_DEFAULT_DIRS: u32 = 0x1000;
    // ★2026-08-10：把 上层注册的外部语言运行时目录（如 GTK4 的 bin/）纳入搜索。
    // AddDllDirectory 注册的目录属于「用户目录集」，必须配合此标志才被显式搜索；
    // 而 SetDllDirectoryW 仅影响 dwFlags=0 的默认搜索序，与显式 SEARCH_* 标志互斥，故用 AddDllDirectory。
    const LOAD_LIBRARY_SEARCH_USER_DIRS: u32 = 0x400;

    /// 外部库运行时搜索目录（由上层在加载 shim/dll 前设置，加载后清除）。
    /// 用 全局静态 Mutex<Vec<String>> 收集，先于系统搜索路径尝试。
    static 外部库搜索目录列表: std::sync::Mutex<Vec<String>> = std::sync::Mutex::new(Vec::new());

    /// 临时把某外部语言运行时目录（如 GTK4 的 bin/）加入加载搜索路径。
    /// AddDllDirectory 不接受 `\\?\` verbatim 前缀，故此处存「普通规整绝对路径」（反斜杠归一）。
    pub fn 设置外部库搜索目录(目录: &std::path::Path) {
        let 规整 = 目录.to_string_lossy().replace('/', "\\");
        let mut 表 = 外部库搜索目录列表.lock().unwrap();
        if !表.iter().any(|p| *p == 规整) {
            表.push(规整);
        }
    }

    /// 清空外部库搜索目录（如 切换项目 时）。
    pub fn 清空外部库搜索目录() {
        let mut 表 = 外部库搜索目录列表.lock().unwrap();
        表.clear();
    }

    /// 已加载的动态库句柄（drop 时自动释放）
    pub struct 库(*mut c_void);

    impl 库 {
        pub fn 加载(路径: &str) -> Option<库> {
            // ★支持中文长路径（规范铁律：绝不挪移文件到其它路径）。
            // 嵌套中文目录（如 全局/产出/外部语言文件/执行文件/…/样例库.dll）
            // 常超过 Windows MAX_PATH(260)，对超长路径需加 `\\?\` verbatim
            // 前缀才能正确解析，否则返回 126（ERROR_MOD_NOT_FOUND）。
            // 自动补齐前缀可突破 32767 字符限制，彻底支持任意长度中文路径。
            // 注意：`\\?\` 前缀仅对绝对路径有效；相对路径保持原样交还系统按当前目录解析
            // （相对路径通常较短，不会触发 MAX_PATH 限制）。
            // 跨平台：verbatim 逻辑收敛进 平台::加载器路径（POSIX 原样返回，无 MAX_PATH 问题）。
            let 加前缀路径 = crate::平台::加载器路径(&路径);
            // ★2026-08-10 传递依赖 DLL 加载：shim 经 raw-dylib 静态引用 gtk4.dll 等，
            // 这些 dll 在 库/.../GTK4/bin/，既不在 shim 目录也不在系统目录，
            // 且 LOAD_LIBRARY_SEARCH_DEFAULT_DIRS 排除 PATH/调用方目录。
            // 经 AddDllDirectory 把各外部语言 bin/ 注册为「用户目录」，配合
            // LOAD_LIBRARY_SEARCH_USER_DIRS 标志，在加载 shim 时一并搜索（加载后移除 cookie）。
            let mut 注册的cookie: Vec<*mut c_void> = Vec::new();
            {
                let 表 = 外部库搜索目录列表.lock().unwrap();
                for 目录 in 表.iter() {
                    let 宽: Vec<u16> = 目录.encode_utf16().chain(std::iter::once(0)).collect();
                    let cookie = unsafe { AddDllDirectory(宽.as_ptr()) };
                    if !cookie.is_null() {
                        注册的cookie.push(cookie);
                    }
                }
            }
            let 宽: Vec<u16> = 加前缀路径.encode_utf16().chain(std::iter::once(0)).collect();
            let h = unsafe {
                LoadLibraryExW(
                    宽.as_ptr(),
                    std::ptr::null_mut(),
                    LOAD_LIBRARY_SEARCH_DLL_LOAD_DIR
                        | LOAD_LIBRARY_SEARCH_DEFAULT_DIRS
                        | LOAD_LIBRARY_SEARCH_USER_DIRS,
                )
            };
            // 移除本次注册的 cookie（避免污染后续加载）
            for cookie in 注册的cookie {
                unsafe { RemoveDllDirectory(cookie); }
            }
            if h.is_null() {
                // ★诊断：打印 LoadLibraryExW 失败码，便于定位（如 126=模块未找到、193=非法的 Win32 映像、1114=DLL 初始化失败）
                let 码 = unsafe { GetLastError() };
                eprintln!("[FFI] 诊断：LoadLibraryExW 失败 os_err={} 路径={}", 码, 加前缀路径);
                None
            } else {
                Some(库(h))
            }
        }
        pub fn 取符号(&self, 名: &str) -> Option<*mut c_void> {
            let 名字节: Vec<u8> = 名.as_bytes().iter().cloned().chain(std::iter::once(0)).collect();
            unsafe {
                let p = GetProcAddress(self.0, 名字节.as_ptr());
                if p.is_null() { None } else { Some(p) }
            }
        }
    }

    impl Drop for 库 {
        fn drop(&mut self) { unsafe { FreeLibrary(self.0); } }
    }

    // 裸句柄始终经 Mutex 独占访问，断定 Send+Sync 是安全的
    unsafe impl Send for 库 {}
    unsafe impl Sync for 库 {}
}

#[cfg(not(windows))]
mod 平台 {
    use std::os::raw::{c_void, c_char};
    extern "C" {
        fn dlopen(filename: *const c_char, flag: i32) -> *mut c_void;
        fn dlsym(handle: *mut c_void, symbol: *const c_char) -> *mut c_void;
        fn dlclose(handle: *mut c_void) -> i32;
    }
    // RTLD_NOW（glibc/macOS 均为 0x2）
    const RTLD_NOW: i32 = 0x2;

    /// 已加载的动态库句柄（drop 时自动释放）
    pub struct 库(*mut c_void);

    impl 库 {
        pub fn 加载(路径: &str) -> Option<库> {
            let c = std::ffi::CString::new(路径).ok()?;
            let h = unsafe { dlopen(c.as_ptr(), RTLD_NOW) };
            if h.is_null() { None } else { Some(库(h)) }
        }
        pub fn 取符号(&self, 名: &str) -> Option<*mut c_void> {
            let c = std::ffi::CString::new(名).ok()?;
            unsafe {
                let p = dlsym(self.0, c.as_ptr());
                if p.is_null() { None } else { Some(p) }
            }
        }
    }

    impl Drop for 库 {
        fn drop(&mut self) { unsafe { dlclose(self.0); } }
    }

    // 裸句柄始终经 Mutex 独占访问，断定 Send+Sync 是安全的
    unsafe impl Send for 库 {}
    unsafe impl Sync for 库 {}

    // POSIX 下 dll 搜索目录由 dlopen 的 rpath/LD_LIBRARY_PATH 机制处理，此处为 no-op
    pub fn 设置外部库搜索目录(_目录: &std::path::Path) {}
    pub fn 清空外部库搜索目录() {}
}

// ============================================================
//  注册表与调用
// ============================================================
#[derive(Clone)]
pub struct FFI项 {
    pub 键: usize,
    /// C 符号名（动态库导出符号）
    pub c符号: String,
    /// 库标识：绝对/相对路径，或经 外部块 ["别名"] 解析后的库名
    pub 库标识: String,
    /// 每个形参的底层类型标签（整数/小数/逻辑/外部文本）
    pub 形参标签: Vec<u8>,
    /// 返回类型的底层类型标签
    pub 返回标签: u8,
    /// 🆕 FFI: 每形参是否为输出参数（输出参数）
    pub 输出位图: Vec<bool>,
    /// 🆕 FFI: 是否为静态链接（true=静态链接，false=运行时 dlopen）
    pub 是否静态: bool,
    /// FFI: 是否原生 C ABI 约定（true=原生 C 类型，需 shim 包装编组；
    /// false=扁平 运行时值 约定，由处理外部调用 的 transmute 回退路径直接调用）
    pub 原生约定: bool,
    /// 🆕 P3: 每形参的精确 C 类型码（0=缺省，回退 标签到C类型 默认位宽）。
    /// 由 生成关联 从 .h 自动推导（int→3/i32、float→9/f32、size_t→16/c_size_t 等），
    /// 手写外部块缺省 0（整数=i64、小数=f64），现有用例零影响。
    pub 形参C类型: Vec<u8>,
    /// 🆕 P3: 返回类型的精确 C 类型码（0=缺省）
    pub 返回C类型: u8,
    /// 🆕 回调：每个函数指针形参对应的 trampoline 形状码（0-4，255=动态）。
    /// 与 shim 预生成的 5 种 trampoline（void fn(i64)/i64 fn()/i64 fn(i64)/i64 fn(i64,*void*)/void fn(*void*)）对齐；
    /// 处理外部调用 据此把「函数索引」替换为真正的 trampoline 地址。
    pub 函数指针形状: Vec<u8>,
    /// 🆕 G1：形参为外部结构体（精确码 21）时的结构体名；非结构体形参为空串。
    /// 这是「外部结构体布局元数据通道」的形参侧：仅凭精确码 21 无法还原布局，
    /// 必须携带名字去 外部结构体布局表 查字段类型，才能生成 #[repr(C)] 布局与序列化 helper。
    pub 形参结构体名: Vec<String>,
    /// 🆕 G1：返回值为外部结构体时的结构体名；非结构体返回为空串。
    pub 返回结构体名: String,
    /// 🆕 FFI（§6.5.2 第5项）：是否外部全局变量（true=C 全局变量，shim 走 `static` 读取而非 `fn` 调用）
    pub 是否全局变量: bool,
    /// 🆕 2026-08-29 FFI 数组：每形参的数组元素外部类型码（非数组形参=0；数组形参=元素精确码，
    /// 宿主 处理外部调用 据此把智语言数组序列化为 C ABI 紧凑缓冲（地址进参数槽），shim 按元素码还原指针）
    pub 形参数组元素C类型: Vec<u8>,
    /// 🆕 2026-08-29 FFI 数组：数组形参的紧邻长度参数索引（经典 `T*, len` 模式；无 = usize::MAX）。
    /// 宿主据此校验「长度 ≤ 数组实际长度」并限定序列化/回写范围。
    pub 形参数组长度索引: Vec<usize>,
}

// 按 键 索引的外部函数表（键 = 外部函数基址 + 序号）
static 外部函数表: Lazy<Mutex<Vec<FFI项>>> = Lazy::new(|| Mutex::new(Vec::new()));

/// 🆕 G1：外部结构体布局表（结构体名 → 各字段的精确 C 类型码）。
///
/// 【为何需要独立的一张表】`注册外部函数` 的 C ABI 出口已有 17 个形参，
/// 再塞布局数组会加剧「跨层签名漂移」（base `#[export_name]` 形参表与编译器
/// `declare_function` 签名必须逐一对齐，历史上漏参直接导致编译模式整体
/// `mismatched argument count`）。故布局走**旁路注册**：结构体布局与函数注册解耦，
/// 两者用「结构体名」这一稳定字符串键关联，双模式共用同一张表。
static 外部结构体布局表: Lazy<Mutex<HashMap<String, Vec<u8>>>> =
    Lazy::new(|| Mutex::new(HashMap::new()));

/// 注册一个外部结构体的字段布局（字段的精确 C 类型码序列，顺序 = 声明顺序）。
/// 解释模式由降级层直接调用；编译模式由 Cranelift 生成的 prologue 经 C ABI 调用。
// ★2026-08-10 重新导出：外部库运行时搜索目录设置（解释模式 shim 加载 GTK 传递依赖 DLL 用）
pub use 平台::设置外部库搜索目录;
pub use 平台::清空外部库搜索目录;

pub fn 注册外部结构体布局(名称: String, 字段类型码: Vec<u8>) {    if 名称.is_empty() {
        return;
    }
    let mut 表 = 外部结构体布局表.lock().unwrap();
    表.insert(名称, 字段类型码);
}

/// 查询外部结构体字段布局；未注册返回 None（调用方据此回退为不透明指针）。
pub fn 取外部结构体布局(名称: &str) -> Option<Vec<u8>> {
    let 表 = 外部结构体布局表.lock().unwrap();
    表.get(名称).cloned()
}
// 动态库句柄缓存（按 库标识 缓存，保持存活以避免重复加载）
static 外部库缓存: Lazy<Mutex<HashMap<String, Option<平台::库>>>> = Lazy::new(|| Mutex::new(HashMap::new()));

/// 返回当前注册表中所有外部库的 库标识 集合（去重），
/// 供构建期将对应动态库（.dll/.so/.dylib）部署到可执行文件旁。
pub fn 外部库标识集合() -> Vec<String> {
    let 表 = 外部函数表.lock().unwrap();
    let mut 集合: std::collections::HashSet<String> = std::collections::HashSet::new();
    for 项 in 表.iter() {
        if !项.库标识.is_empty() {
            集合.insert(项.库标识.clone());
        }
    }
    集合.into_iter().collect()
}

/// 清空注册表（每次运行前调用，避免上一次程序的残留守留）
pub fn 清空外部函数表() {
    let mut 表 = 外部函数表.lock().unwrap();
    表.clear();
    let mut 库 = 外部库缓存.lock().unwrap();
    库.clear();
    let mut 包装表 = 包装调度函数表.lock().unwrap();
    *包装表 = None;
    let mut 包装库 = 包装库句柄.lock().unwrap();
    *包装库 = None;
}

// ============================================================
//  【缺口 C 修复：FFI 回调打通 — 依赖倒置注册点】
//  设计：base 不知道"如何执行一个智语言用户函数"（解释器有 VM、编译模式有
//  原生函数表），故由 base 定义一个注册点，各后端在启动时把自己的"用户函数
//  调用器"注入。shim trampoline 经 智语言_系统调用回调 进来后，base 查此注册点
//  直接执行用户函数，而不再误走系统调用 119（把用户函数索引当命令码路由）。
//  调用器原型采用 裸指针 + fn 指针 的 C 友好签名，避免 base↔解释器 循环依赖：
//    适配器: unsafe fn(*mut c_void, 函数索引, &[运行时值]) -> 运行时值
//  后端注册时把自己的 VM/函数表指针作为 第一参数 透传，回调执行器再转回具体类型。
// ============================================================

/// 用户函数调用器：第一参数为后端私有上下文（如 VM 指针地址，存为 usize），第二为用户函数索引（≥200），
/// 第三为实参裸指针，第四为实参长度，第五为结果写回指针（32 字节 运行时值）。
/// 注意：使用 extern "C" fn 指针（FFI 需 C ABI 调用），函数指针本身是 Sync/Send；
/// 全部参数类型均为 Sync 兼容（usize + 裸指针），保证 全局注册表 满足 Sync；
/// 指针解引用等不安全操作在调用器实现内部用 unsafe 块封装。
/// 结果经「显式输出指针」回写（与编译模式 thunk 既有约定一致，规避 Cranelift 返回 32 字节结构体
/// 的 sret 歧义——解释/编译双模式共用同一 Rust 注册点，仅桥函数内部实现不同）。
pub type 用户函数调用器 = extern "C" fn(usize, usize, *const 运行时值, usize, *mut 运行时值);

/// 依赖倒置注册点：后端启动时注册，base 回调 dispatch 时调用。
static 用户函数调用器注册表: Lazy<Mutex<Option<(用户函数调用器, usize)>>> =
    Lazy::new(|| Mutex::new(None));

/// 寄存器后端用户函数调用器（解释器传 VM 指针地址 + 桥函数；编译模式传本机函数表指针地址 + 桥函数）。
pub fn 注册用户函数调用器(调用器: 用户函数调用器, 上下文: usize) {
    let mut 表 = 用户函数调用器注册表.lock().unwrap();
    *表 = Some((调用器, 上下文));
}

/// 注册用户函数调用器并返回此前的注册项（供嵌套守卫在退出时原样恢复）。
/// 解释器的 `执行()` 会被 FFI 回调重入：内层退出时必须恢复外层注册，而不是清空，
/// 否则同一次外部调用里的第二次回调起将找不到调用器、静默返回空值。
pub fn 替换用户函数调用器(新项: Option<(用户函数调用器, usize)>) -> Option<(用户函数调用器, usize)> {
    let mut 表 = 用户函数调用器注册表.lock().unwrap_or_else(|e| e.into_inner());
    std::mem::replace(&mut *表, 新项)
}

/// 注销用户函数调用器（VM 销毁/程序退出时调用，避免悬垂指针）。
pub fn 注销用户函数调用器() {
    let mut 表 = 用户函数调用器注册表.lock().unwrap();
    *表 = None;
}

/// 是否已有后端注册了用户函数调用器（供 shim 调度判断）。
pub fn 有用户函数调用器() -> bool {
    let 表 = 用户函数调用器注册表.lock().unwrap();
    表.is_some()
}

/// 经注册点执行一个用户函数（不透明 dispatch，双模式共用）。
/// 结果经 输出 指针回写（32 字节 运行时值）；未注册时写回空值（安全降级，不 panic）。
/// 参数以裸指针+长度传入（避免依赖 运行时值: Sync，保全局注册表 Sync）。
pub fn 执行用户函数回调(函数索引: usize, 参数指针: *const 运行时值, 参数数量: usize, 输出: *mut 运行时值) {
    // ★死锁修复（2026-08-07）：不能持锁调用 调用器 —— 解释器回调桥 会重入 VM（调用函数带参数 → self.执行()），
    //   VM 执行开头 回调桥守卫::启用 再调 注册用户函数调用器（lock 同一把锁）→ 单线程重入死锁（FFI 回调 应用 卡死）。
    //   正确做法：锁内仅复制 (调用器, 上下文)（二者均为 Copy：extern"C" fn + usize），锁外调用，锁临界区最短。
    let (调用器, 上下文) = {
        let 注册 = 用户函数调用器注册表.lock().unwrap();
        match *注册 {
            Some((调用器, 上下文)) => (调用器, 上下文),
            None => {
                if !输出.is_null() {
                    写运行时值到(输出, &运行时值::空值());
                }
                return;
            }
        }
    };
    调用器(上下文, 函数索引, 参数指针, 参数数量, 输出);
}

// ═══════════════════════════════════════════════════════
//  HTTP 服务端后台执行器（依赖倒置注册点，2026-08-19 阶段二）
//  设计：HTTP 服务端（网络系统函数 147）在后台 accept 线程同步调用处理器，该线程
//  可能是「用户函数调用器」注册者的非持锁线程——而 解释器回调桥 是「同线程重入主 VM」
//  语义（仅 FFI 回调安全，跨线程重入主 VM 非线程安全）。故单独设一「后台用户函数
//  执行器」槽位：
//    · 解释器模式：注册「子VM执行器」（每次调用新建独立子 VM 执行目标函数，跨线程安全）；
//    · 编译模式：不注册（原生 回调桥 thunk 天然线程安全），未注册时回退 用户函数调用器。
//  双模式共用 执行后台用户函数 入口 → 网络系统函数 147 双分支收敛为单实现。
// ═══════════════════════════════════════════════════════
static 后台用户函数执行器注册表: Lazy<Mutex<Option<(用户函数调用器, usize)>>> =
    Lazy::new(|| Mutex::new(None));

/// 注册后台用户函数执行器（解释器：子VM执行器 + 字节码 Arc 指针；编译模式无需注册）。
pub fn 注册后台用户函数执行器(调用器: 用户函数调用器, 上下文: usize) {
    let mut 表 = 后台用户函数执行器注册表.lock().unwrap();
    *表 = Some((调用器, 上下文));
}

/// 注销后台用户函数执行器（VM 销毁/程序退出时调用，避免悬垂指针）。
pub fn 注销后台用户函数执行器() {
    let mut 表 = 后台用户函数执行器注册表.lock().unwrap();
    *表 = None;
}

/// 经后台执行器执行一个用户函数（HTTP 服务端处理器专用，双模式共用）。
/// 优先 后台用户函数执行器（跨线程安全叶子）；未注册则回退 用户函数调用器
/// （编译模式=回调桥 thunk，原生函数天然线程安全）。两者均未注册时写回空值（安全降级）。
/// 锁内仅复制 (调用器, 上下文)（二者均为 Copy），锁外调用（同 执行用户函数回调 死锁修复约定）。
pub fn 执行后台用户函数(函数索引: usize, 参数指针: *const 运行时值, 参数数量: usize, 输出: *mut 运行时值) {
    let (调用器, 上下文) = {
        let 后台 = 后台用户函数执行器注册表.lock().unwrap();
        match *后台 {
            Some((调用器, 上下文)) => (调用器, 上下文),
            None => {
                drop(后台);
                let 注册 = 用户函数调用器注册表.lock().unwrap();
                match *注册 {
                    Some((调用器, 上下文)) => (调用器, 上下文),
                    None => {
                        if !输出.is_null() {
                            写运行时值到(输出, &运行时值::空值());
                        }
                        return;
                    }
                }
            }
        }
    };
    调用器(上下文, 函数索引, 参数指针, 参数数量, 输出);
}

/// 注册一条外部函数声明。键须与降级层 函数索引映射 中的外部函数基址对齐。
pub fn 注册外部函数(项: FFI项) {
    eprintln!("[调试FFI] 注册 键={} c符号={} 库标识={} 原生约定={} 静态={}",
        项.键, 项.c符号, 项.库标识, 项.原生约定, 项.是否静态);
    let mut 表 = 外部函数表.lock().unwrap();
    let 键 = 项.键;
    if 键 >= 表.len() {
        表.resize(
            键 + 1,
            FFI项 {
                键: 0,
                c符号: String::new(),
                库标识: String::new(),
                形参标签: vec![],
                返回标签: 0,
                输出位图: vec![],
                是否静态: false,
                原生约定: false,
                形参C类型: vec![],
                返回C类型: 0,
                函数指针形状: vec![],
                形参结构体名: vec![],
                返回结构体名: String::new(),
                是否全局变量: false,
                形参数组元素C类型: vec![],
                形参数组长度索引: vec![],
            },
        );
    }
    表[键] = 项;
}

/// 外部 C 函数统一签名：
///   void fn(参数: *mut 运行时值, 数量: i64, 输出: *mut 运行时值)
/// - 参数[0..数量]：入参。堆对象（文本/数组/映射/结构体/闭包）经「统一 FFI 编组层」序列化为
///   扁平缓冲，以 `标签_外部值(10)` + 整数指针表示；标量透传。
/// - 输出：返回值，堆对象同样以 `标签_外部值` + 整数指针表示。
/// ⚠ 2026-09-25 校正：原文写「文本以 **标签_外部文本** + 整数指针 表示」——该 ad-hoc 标签
///   早已废弃（与 `标签_函数指针=6` 撞车，见 值类型.rs 注释），现行统一为 `标签_外部值`，
///   且不只承载文本，而是**任意堆类型**。勿再按旧文推理。
type 外部函数签名 = unsafe extern "C" fn(*mut 运行时值, i64, *mut 运行时值);
/// 导出 DLL 的自有释放函数签名：void fn(标签: u64, 载荷: i64)。
/// 输出扁平缓冲由 DLL 分配，必须由 DLL 自己释放，跨二进制释放会触发堆损坏。
type 释放函数签名 = unsafe extern "C" fn(u64, i64);
/// FFI shim 调度函数签名：void fn(u64 函数索引, *mut 运行时值 输出, i64 数量, *mut 运行时值 输入)
type 包装调度函数 = unsafe extern "C" fn(u64, *mut 运行时值, i64, *mut 运行时值);

// 🆕 包装函数表：键=外部函数基址+序号，值=对应 shim 包装函数
// 优先于旧 transmute 路径，消除 C ABI 不匹配 UB
static 包装调度函数表: Lazy<Mutex<Option<包装调度函数>>> = Lazy::new(|| Mutex::new(None));
// 外层 cdylib 句柄缓存（保持存活，避免函数指针悬垂）
static 包装库句柄: Lazy<Mutex<Option<平台::库>>> = Lazy::new(|| Mutex::new(None));

/// 注册外部包装调度函数：由 shim 代码生成器生成并编译后调用。
/// 注册成功后，处理外部调用 优先走 shim 包装路径，彻底消除旧 transmute UB。
pub fn 注册包装调度函数(调度函数: 包装调度函数) {
    let mut 包装表 = 包装调度函数表.lock().unwrap();
    *包装表 = Some(调度函数);
}

/// 系统调用索引 119 落点：按 键 查找注册项并调用动态库符号。
///
/// 【双模式统一】按外部块声明的边界契约二选一，两条路径都是正式契约，无 UB 逃生舱：
/// - 真实 C ABI（原生约定=true，默认）：必须经 shim 包装函数调度，
///   内部做 运行时值↔C原生类型 编组。shim 未加载时**抛异常快速失败**（G4），
///   绝不跌落扁平签名 transmute（那是确定的 ABI 违约）。
/// - 智语言扁平契约（原生约定=false，`["库名" 智语言]`）：被调符号真实签名即
///   `void f(输出, 数量, 输入)`，直接 transmute 到 外部函数签名 与实际定义一致。

pub fn 处理外部调用(索引: usize, 参数: &[运行时值]) -> 运行时值 {

    // 索引 由 核心系统函数::处理核心(119,..) 在调用前已剥离：
    //   处理外部调用(参数[0].取整数(), &参数[1..])
    // 此处 参数 已是全部实际参数（不含索引）。
    let 项 = {
        let 表 = 外部函数表.lock().unwrap();
        表.get(索引).cloned()
    };
    let 项 = match 项 {
        Some(x) => x,
        None => {
            eprintln!("[调试FFI] 调用 索引={} 未注册", 索引);
            return 运行时值::空值();
        }
    };
    // 回调接线：把「函数指针」形参（标签=6、整数=-(函数索引+1)）替换为真实 trampoline 地址，
    // 供 shim 把该地址 transmute 成 C 函数指针后传给 C 侧回调智语言函数。无回调形参时为无操作。
    // ── 优先路径：shim 包装调度函数（仅原生 C ABI 约定） ──
    // 仅当外部函数声明为原生 C 约定（原生约定=true）时走此路径，
    // shim 包装函数内部做正确的 C ABI 编组（运行时值↔原生类型）。
    // 扁平 运行时值 约定的库（如测试样例库）走下方 transmute 回退路径。
    if 项.原生约定 == false {
        eprintln!("[调试FFI] 走到扁平路径 索引={} c符号=[{}] 库标识=[{}] 参数数={} 参数0标签={} 参数0整数={}",
            索引, 项.c符号, 项.库标识, 参数.len(),
            参数.first().map(|v| v.标签 as i64).unwrap_or(-1),
            参数.first().map(|v| v.整数).unwrap_or(0));
    }
    if 项.原生约定 {
        eprintln!("[调试FFI] 调用 索引={} c符号={} 库标识={}", 索引, 项.c符号, 项.库标识);
        let 包装表 = 包装调度函数表.lock().unwrap();
        if let Some(调度) = *包装表 {
            drop(包装表);
            let 输出参数数 = 项.输出位图.iter().filter(|&&b| b).count();
            // 🆕 G1: 输出槽布局与 shim 侧约定严格一致（生成导入包装）：
            //   slot 0 = C 返回值（无返回值时空置占位），slot 1.. = 输出参数。
            // 故只要存在输出参数，返回槽 = 输出参数数 + 1（slot 0 恒预留）；
            // 无输出参数时，返回槽 = 1（仅 C 返回值）或 0（无返回值）。
            let 有C返回值 = 项.返回标签 as u64 != 0;
            let 返回槽数 = if 输出参数数 > 0 {
                输出参数数 + 1
            } else if 有C返回值 {
                1
            } else {
                0
            };
            // 输入参数：宿主侧先把每个参数 编组为外部值（标签_外部值 扁平缓冲），
            // 文本/数组/映射/结构体等堆对象在此完成序列化，shim 内再还原为 C 原生类型。
            // 🆕 2026-08-29 FFI 数组：数组形参（元素码≠0）不编组为外部值，而是由宿主直接
            // 序列化为 C ABI 紧凑缓冲（地址进参数槽，shim 按元素码还原指针）；调用后回写。
            let mut 调用参数: Vec<运行时值> = Vec::new();
            let mut 数组回写: Vec<(运行时值, Vec<u8>, u8, usize)> = Vec::new(); // (数组句柄, C缓冲, 元素码, 回写长度)
            for (i, p) in 参数.iter().enumerate() {
                let 元素码 = 项.形参数组元素C类型.get(i).copied().unwrap_or(0);
                if 元素码 != 0 {
                    // 数组形参：长度校验 + 序列化 C 紧凑缓冲（元素按声明位宽转 C ABI 布局）
                    let 实际长度 = crate::堆操作::堆操作(crate::堆操作::堆_数组长度, &[*p]).整数.max(0) as usize;
                    let 声明长度 = if let Some(&长度索引) = 项.形参数组长度索引.get(i) {
                        if 长度索引 < 参数.len() {
                            let 长度值 = 参数[长度索引].整数;
                            if 长度值 < 0 || 长度值 as usize > 实际长度 {
                                let 错误信息 = format!(
                                    "外部函数【{}】参数 {} 数组长度超限：声明长度 {} 超出数组实际长度 {}",
                                    项.c符号, i + 1, 长度值, 实际长度
                                );
                                crate::异常运行时::智语言_抛出异常(
                                    "运行时异常".as_ptr(), "运行时异常".len() as i64,
                                    错误信息.as_ptr(), 错误信息.len() as i64,
                                );
                                return 运行时值::空值();
                            }
                            长度值 as usize
                        } else {
                            实际长度
                        }
                    } else {
                        实际长度
                    };
                    let 缓冲 = 序列化数组为C紧凑缓冲(*p, 元素码, 声明长度);
                    let 地址 = 缓冲.as_ptr() as i64;
                    数组回写.push((*p, 缓冲, 元素码, 声明长度));
                    调用参数.push(运行时值 { 标签: 标签_整数, 整数: 地址, 小数: 0.0, 句柄: 0 });
                } else if p.标签 == 标签_外部值 || p.标签 == 标签_函数指针 {
                    // 【缺口 C 修复】函数指针实参不编组（shim 需要的是 trampoline 地址，
                    // 而非 标签_外部值 缓冲）；下方循环会将其替换为 trampoline 地址。
                    调用参数.push(*p);
                } else {
                    调用参数.push(编组为外部值(*p));
                }
            }

            // 【缺口 C 修复】函数指针实参 → trampoline 地址：
            // 当形参声明为函数指针（形状码≠255）且实参是函数引用时，把智语言函数
            // 注册到 shim 槽位得到 trampoline 地址，替换参数整数字段，使 C 侧能经
            // trampoline → 智语言_系统调用回调 → 用户函数调用器 回调回智语言。
            for (i, &形状) in 项.函数指针形状.iter().enumerate() {
                if 形状 != 255 && i < 调用参数.len() {
                    let 实参 = 调用参数[i];
                    if 实参.标签 == 标签_函数指针 {
                        // ★运行时值::函数指针 采用负编码 `-(函数索引)-1`（见 值类型::函数指针），
                        // 须先解码为正索引再注册，否则以极大负值当槽位 → trampoline 错配 → C 侧崩溃。
                        let 正索引 = (-(实参.整数) - 1) as usize;
                        if let Some((_槽位, 地址)) = 注册ffi回调(正索引, 形状 as u64) {
                            调用参数[i] = 运行时值 { 标签: 标签_整数, 整数: 地址 as i64, 小数: 0.0, 句柄: 0 };
                        }
                    }
                }
            }

            let mut 输出槽: Vec<运行时值> = vec![运行时值::空值(); 返回槽数.max(1)];
            let 数量 = 调用参数.len() as i64;
            unsafe {
                let 输入指针 = 调用参数.as_mut_ptr();
                let 输出指针 = 输出槽.as_mut_ptr();
                // shim 包装函数契约：void fn(u64 函数索引, *mut 运行时值 输出, i64 数量, *mut 运行时值 输入)
                调度(索引 as u64, 输出指针, 数量, 输入指针);
            }
            // 释放宿主侧编组的输入缓冲（标签_外部值 扁平缓冲），避免内存泄漏
            for p in &调用参数 {
                释放外部值(*p);
            }

            // 🆕 2026-08-29 FFI 数组 In/Out 回写：读回 C 紧凑缓冲写回智语言数组
            // （原地 堆_数组存储，1-based），然后 C 缓冲随 Vec drop 自动释放。
            // 🆕 2026-08-29 FFI 数组 In/Out 回写：读回 C 紧凑缓冲写回智语言数组（堆_数组存储）。
            // ⚠️ 已知限制：智语言数组为值语义（传参深拷贝），回写改的是宿主收到的副本；
            // 要反映到调用方变量需调用点写回实参（后续增强）。In 只读/长度校验已完整可用。
            for (数组值, 缓冲, 元素码, 回写长度) in &数组回写 {
                for 索引 in 0..*回写长度 {
                    let 元素 = 从C紧凑缓冲读元素(缓冲, *元素码, 索引);
                    crate::堆操作::堆操作(
                        crate::堆操作::堆_数组存储,
                        &[数组值.clone(), 运行时值::整数(索引 as i64 + 1), 元素],
                    );
                }
            }

            // 🆕 G1: C 函数返回非零自动抛异常
            // 仅当: 有输出参数 + 有 C 返回值 + 返回值是整数且非零
            if 输出参数数 > 0 && 有C返回值 {
                let c返回值 = 输出槽[0];  // slot 0 = C return value (written by shim wrapper)
                if c返回值.标签 == 标签_整数 && c返回值.整数 != 0 {
                    let 错误信息 = format!("外部函数【{}】返回非零值: {}", 项.c符号, c返回值.整数);
                    crate::异常运行时::智语言_抛出异常(
                        "运行时异常".as_ptr(), "运行时异常".len() as i64,
                        错误信息.as_ptr(), 错误信息.len() as i64
                    );
                    return 运行时值::空值();
                }
            }

            // 处理返回/释放（与 shim 输出槽约定一致：slot 0 = C 返回值/空置，slot 1.. = 输出参数）
            if 输出参数数 == 0 {
                // 无输出参数：slot 0 = C 返回值（若有），否则无返回值
                if 有C返回值 {
                    let 还原值 = 从外部值还原(输出槽[0]);
                    释放外部值(输出槽[0]);
                    return 还原值;
                }
                return 运行时值::空值();
            } else if 输出参数数 == 1 {
                // 单输出参数：直接返回输出值（slot 1；slot 0 = C 返回值，仅供异常判断已在上方处理）
                let 还原值 = 从外部值还原(输出槽[1]);
                释放外部值(输出槽[1]);
                return 还原值;
            } else {
                // 多输出参数：还原从 slot 1 开始的输出参数数组
                let 还原数组: Vec<运行时值> = 输出槽[1..1+输出参数数]
                    .iter()
                    .map(|v| {
                        let r = 从外部值还原(v.clone());
                        释放外部值(*v);
                        r
                    })
                    .collect();
                return 分配数组(还原数组);
            }
        } else {
            // 【G4 消除逃生舱】原生 C ABI 约定的外部函数**必须**经 shim 包装调用。
            // 旧实现在 shim 不可用时静默跌落到下方 transmute 路径，把真实 C 符号
            // 强转成扁平签名 `void(*mut 运行时值, i64, *mut 运行时值)` 调用——这是
            // 确定的 ABI 违约（UB）：轻则参数错位读到垃圾值，重则栈破坏/访问违例，
            // 且现象随平台与调用约定漂移，极难定位。此处改为明确抛异常快速失败。
            drop(包装表);
            let 错误信息 = format!(
                "外部函数【{}】声明为真实 C ABI，但 FFI 包装（shim）未加载，无法安全调用。\
                 请确认 shim 已生成并编译成功（解释模式 cdylib / 编译模式 staticlib），\
                 或在外部块中以 [\"{}\" 智语言] 声明为智语言扁平契约。",
                项.c符号, 项.库标识
            );
            crate::异常运行时::智语言_抛出异常(
                "运行时异常".as_ptr(), "运行时异常".len() as i64,
                错误信息.as_ptr(), 错误信息.len() as i64
            );
            return 运行时值::空值();
        }
    }

    // ── 智语言扁平契约路径（非逃生舱，是 §6.5 第二种正式边界契约） ──
    // 仅当外部块以 ["库名" 智语言] 声明（原生约定=false）时到达此处。
    // 此时被调符号的真实签名**就是** `void f(输出, 数量, 输入)`（外部函数签名），
    // transmute 到该签名与 C 侧实际定义一致，不构成 ABI 违约。
    // 真实 C ABI 的函数永远走上方 shim 路径，不会落到这里。


    // 解析并缓存动态库
    let mut 库锁 = 外部库缓存.lock().unwrap();
    if !库锁.contains_key(&项.库标识) {
        // ★2026-08-05 学生点名系统：动态库（dlopen）库标识可能为纯库名（如 "sqlite3"），
        // LoadLibrary("sqlite3") 会去系统路径找（不存在）。回退解析：
        // 1. 库标识原样（含路径的情况）
        // 2. 当前可执行文件所在目录/{库名}.dll（部署逻辑把 sqlite3.dll 放到 exe 旁）
        // 3. 当前目录/{库名}.dll
        let 平台后缀 = if cfg!(windows) { ".dll" } else if cfg!(target_os = "macos") { ".dylib" } else { ".so" };
        let 库名 = 项.库标识.split(['/', '\\']).next_back().unwrap_or(&项.库标识).to_string();
        let mut 候选路径列表: Vec<String> = vec![项.库标识.clone()];
        if let Ok(当前exe) = std::env::current_exe() {
            if let Some(目录) = 当前exe.parent() {
                候选路径列表.push(目录.join(format!("{}{}", 库名, 平台后缀)).to_string_lossy().to_string());
            }
        }
        候选路径列表.push(format!("{}{}", 库名, 平台后缀));
        let 结果 = 候选路径列表.into_iter()
            .find_map(|p| 平台::库::加载(&p));
        库锁.insert(项.库标识.clone(), 结果);
    }
    let 库 = match 库锁.get(&项.库标识).unwrap() {
        Some(l) => l,
        None => {
            return 运行时值::空值();
        }
    };

    // 解析符号
    let 符号指针 = match 库.取符号(&项.c符号) {
        Some(p) => p,
        None => {
            return 运行时值::空值();
        }
    };
    let 符号: 外部函数签名 = unsafe { std::mem::transmute(符号指针) };

    // 取得导出 DLL 的 智语言_释放外部值：输出缓冲由 DLL 分配，必须由 DLL 释放
    // （跨二进制释放会触发堆损坏 0xC0000374）。输入缓冲由本侧分配，仍用本侧 释放外部值。
    let 释放函数指针 = 库.取符号("智语言_释放外部值");
    let 释放函数: Option<释放函数签名> = 释放函数指针.map(|p| unsafe { std::mem::transmute(p) });
    let 释放输出缓冲 = |v: 运行时值| {
        if let Some(f) = 释放函数 {
            unsafe { f(v.标签 as u64, v.整数) };
        } else {
            释放外部值(v);
        }
    };

    // 构造调用参数（统一 FFI 编组层）：堆对象 编组为 标签_外部值 扁平缓冲；标量透传。
    // 智语言 侧创建的输入缓冲在调用结束后由本函数统一释放（所有权不转移给 C 侧）。
    let mut 输入缓冲: Vec<运行时值> = Vec::new();
    let mut 调用参数: Vec<运行时值> = Vec::with_capacity(参数.len());
    for p in 参数.iter() {
        let v = *p;
        if v.标签 == 标签_句柄 {
            let 缓冲 = 编组为外部值(v);
            输入缓冲.push(缓冲);
            调用参数.push(缓冲);
        } else {
            调用参数.push(v);
        }
    }

    // ★回调接线：把「函数指针」形参（标签=6、整数=-(函数索引+1)）替换为真实 trampoline 地址，
    // 供 shim 把该地址 transmute 成 C 函数指针后传给 C 侧回调智语言函数。无回调形参时为无操作。
    处理外部调用的回调(&项, &mut 调用参数);

    // 输出槽数：有输出参数 → 输出参数数；否则有返回类型 → 1；否则 0（void）
    let 输出参数数 = 项.输出位图.iter().filter(|&&b| b).count();
    let 返回槽数 = if 输出参数数 > 0 {
        输出参数数
    } else if 项.返回标签 as u64 != 0 {
        1
    } else {
        0
    };

    let mut 输出槽: Vec<运行时值> = vec![运行时值::空值(); 返回槽数.max(1)];
    let 数量 = 调用参数.len() as i64;
    unsafe {
        let 输入指针 = 调用参数.as_mut_ptr();
        let 输出指针 = 输出槽.as_mut_ptr();
        // 统一契约：void fn(输出槽, 数量, 输入) —— 多输出时输出槽为数组

        符号(输出指针, 数量, 输入指针);
    }
    drop(库锁);

    // 释放输入缓冲：所有权约定（双模式统一）
    // - 智语言 编译的 DLL 由 thunk 在调用期间统一释放入参（任务执行器.rs 入参释放循环），
    //   跨模块释放同一进程堆在 Windows 安全，故本侧不可重复释放，否则双重释放导致堆损坏。
    // - 纯 C 库无 thunk、不识别 标签_外部值，必须由本侧释放以避免泄漏。
    //   输入缓冲本侧分配（编组为外部值 → Box::leak），本侧释放（释放外部值 → Box::from_raw），
    //   同模块分配/释放，安全。
    //
    // 🔎 2026-09-25 复核（消除一处「两路径策略不一致」的误判）：**本路径与 shim 路径的策略不同，是
    //   **有意且必要的**，因为**被调方种类不同**：
    //     · shim 路径（本文件 :573-576）：被调方是**本地 shim cdylib**（真实 C ABI 的编组层），
    //       它只**借用**宿主传入的扁平缓冲，从不释放 ⇒ 必须由宿主**无条件**释放；
    //     · 本路径（扁平契约）：被调方是**远端 DLL**——若是智语言编译的 DLL，其导出 thunk
    //       自己释放入参 ⇒ 宿主**必须不释放**；若是纯 C 库则无 thunk ⇒ 宿主**必须释放**。
    //   判据「DLL 是否导出 `智语言_释放外部值`」即"是否为智语言编译产物"的代理（该符号名是本项目
    //   专有 ABI 名，纯 C 库不会导出）⇒ 判据可靠。**勿把两条路径"统一"为同一策略**（会造成双重释放或泄漏）。
    if 释放函数.is_none() {

        for b in 输入缓冲.iter() {
            释放外部值(*b);
        }

    }

    if 返回槽数 == 0 {
        运行时值::空值()
    } else if 返回槽数 == 1 {
        // C 侧返回的 标签_外部值 缓冲：还原为堆对象后由本侧释放

        let 还原值 = 从外部值还原(输出槽[0]);

        释放输出缓冲(输出槽[0]);

        还原值
    } else {
        let 还原数组: Vec<运行时值> = 输出槽[0..返回槽数]
            .iter()
            .map(|v| {
                let r = 从外部值还原(v.clone());
                释放输出缓冲(*v);
                r
            })
            .collect();
        分配数组(还原数组)
    }
}

// ============================================================
//  W4 FFI 导出块：统一 FFI 编组层（双模式共用真相）
//  跨 C ABI 传递堆对象时，把堆对象序列化为扁平缓冲（线格式 = 递归 运行时值），
//  边界两侧（导出 thunk / 外部调用）共用本层，彻底解决全局堆跨 DLL 边界分裂问题。
//
//  线格式（每个缓冲 = 一块 Box<[u8]>，由 标签_外部值 运行时值 经 整数 字段持有指针）：
//    [0..8]   子标签: u64   (线_文本=4 / 线_数组=5 / 线_映射=6 / 线_结构体=7 / 线_闭包=8)
//    [8..16]  字节长度: u64 (整块长度，含本头)
//    [16..]   payload，嵌套堆对象以 运行时值{标签_外部值, 整数=子缓冲指针} 递归铺开
// ============================================================

const 线_文本: u64 = 4;
const 线_数组: u64 = 5;
const 线_映射: u64 = 6;
const 线_结构体: u64 = 7;
const 线_闭包: u64 = 8;

/// 构造 [子标签][字节长度][payload] 缓冲，返回泄漏所有权交由 释放外部值 回收的指针。
fn 构造缓冲(子标签: u64, 载荷: &[u8]) -> Box<[u8]> {
    let mut 缓冲 = Vec::with_capacity(16 + 载荷.len());
    缓冲.extend_from_slice(&子标签.to_le_bytes());
    缓冲.extend_from_slice(&((16 + 载荷.len()) as u64).to_le_bytes());
    缓冲.extend_from_slice(载荷);

    缓冲.into_boxed_slice()
}

/// 从 Box<[u8]> 生成 标签_外部值 运行时值（整数字段 = 泄漏后的缓冲指针）。
fn 外部值运行时值(缓冲: Box<[u8]>) -> 运行时值 {
    let 泄漏 = Box::leak(缓冲);
    运行时值 {
        标签: 标签_外部值,
        整数: 泄漏.as_ptr() as *const u8 as i64,
        小数: 0.0,
        句柄: 0,
    }
}

/// 把 运行时值 写入扁平缓冲（32 字节，含可能嵌套的 标签_外部值 指针）。
fn 写运行时值(扁平: &mut Vec<u8>, v: &运行时值) {
    扁平.extend_from_slice(&v.标签.to_le_bytes());
    扁平.extend_from_slice(&v.整数.to_le_bytes());
    扁平.extend_from_slice(&v.小数.to_le_bytes());
    扁平.extend_from_slice(&v.句柄.to_le_bytes());
}

/// 文本 → 标签_外部值（不分配堆句柄，直接序列化字节）。
fn 文本到外部值(s: &str) -> 运行时值 {
    let 字节 = s.as_bytes();
    let 缓冲 = 构造缓冲(线_文本, 字节);
    外部值运行时值(缓冲)
}

// ── 编组：堆对象 → 扁平缓冲 ──
/// 把一个 运行时值 编组为 标签_外部值 扁平缓冲；标量（整数/小数/逻辑/空值）原地透传。
/// 文本与复合对象在智语言中均以 标签_句柄 引用全局堆中的 堆数据（文本/数组/映射/结构体），
/// 统一序列化为 [子标签][长度][UTF-8] 缓冲，确保 shim（独立二进制）能正确还原 C 字符串指针。
pub fn 编组为外部值(v: 运行时值) -> 运行时值 {
    if v.标签 != 标签_句柄 {
        return v;
    }
    let 数据 = match 堆读取(v.句柄) {
        Some(d) => d,
        None => return 运行时值::空值(),
    };
    match 数据 {
        堆数据::文本(s) => 文本到外部值(&s),
        堆数据::文本_静态(p, n) => {
            let 切片 = unsafe { std::slice::from_raw_parts(p as *const u8, n) };
            文本到外部值(&String::from_utf8_lossy(切片).into_owned())
        }
        堆数据::数组(arr) => {
            let mut 扁平 = Vec::new();
            扁平.extend_from_slice(&(arr.len() as u64).to_le_bytes());
            for e in arr {
                let e序列化 = if e.标签 == 标签_句柄 { 编组为外部值(e) } else { e };
                写运行时值(&mut 扁平, &e序列化);
            }
            外部值运行时值(构造缓冲(线_数组, &扁平))
        }
        堆数据::映射(m) => {
            let mut 扁平 = Vec::new();
            扁平.extend_from_slice(&(m.len() as u64).to_le_bytes());
            for (k, val) in m {
                let 键值 = 堆键到运行时值(k);
                写运行时值(&mut 扁平, &键值);
                let val序列化 = if val.标签 == 标签_句柄 { 编组为外部值(val) } else { val };
                写运行时值(&mut 扁平, &val序列化);
            }
            外部值运行时值(构造缓冲(线_映射, &扁平))
        }
        堆数据::结构体 { 类型索引, 字段值 } => {
            let mut 扁平 = Vec::new();
            扁平.extend_from_slice(&(类型索引 as u64).to_le_bytes());
            扁平.extend_from_slice(&(字段值.len() as u64).to_le_bytes());
            for f in 字段值 {
                let f序列化 = if f.标签 == 标签_句柄 { 编组为外部值(f) } else { f };
                写运行时值(&mut 扁平, &f序列化);
            }
            外部值运行时值(构造缓冲(线_结构体, &扁平))
        }
        堆数据::闭包环境 { 函数索引, 捕获变量 } => {
            let mut 扁平 = Vec::new();
            扁平.extend_from_slice(&(函数索引 as u64).to_le_bytes());
            扁平.extend_from_slice(&(捕获变量.len() as u64).to_le_bytes());
            for c in 捕获变量 {
                let c序列化 = if c.标签 == 标签_句柄 { 编组为外部值(c) } else { c };
                写运行时值(&mut 扁平, &c序列化);
            }
            外部值运行时值(构造缓冲(线_闭包, &扁平))
        }
    }
}

/// 堆键 → 运行时值（映射键序列化）：整数/逻辑内联；文本 → 标签_外部值 嵌套。
fn 堆键到运行时值(k: 堆键) -> 运行时值 {
    match &k {
        堆键::整数(i) => 运行时值::整数(*i),
        堆键::逻辑(b) => 运行时值::逻辑(*b),
        堆键::文本(s) => 文本到外部值(s),
        // 2026-10-12：新增键变体（外部值编组/还原往返）
        堆键::空值 => 运行时值::空值(),
        堆键::小数(位) => 运行时值::小数(f64::from_bits(*位)),
        // 结构体键 ⇒ 还原为结构体实例（键→值），交由外部值编组按结构体序列化
        堆键::结构体 { .. } => 键转值(&k),
        // 枚举键 ⇒ 还原为**载体数组**（键→值）；还原侧由 `值转键` 依注册表重新识别为枚举键 ✓
        堆键::枚举 { .. } => 键转值(&k),
    }
}

// ── 还原：扁平缓冲 → 堆对象 ──
/// 把 标签_外部值 扁平缓冲还原为堆对象（标签_句柄）；标量透传。
pub fn 从外部值还原(v: 运行时值) -> 运行时值 {
    if v.标签 != 标签_外部值 {
        return v;
    }
    let 指针 = v.整数 as u64 as *const u8;
    if 指针.is_null() {
        return 运行时值::空值();
    }
    // 缓冲来自外部代码（shim / 用户写的原生库），头部里的长度与元素个数都不可信：
    // 旧实现直接按它们切片、预分配、循环读取，个数被写坏时会越界读或因容量溢出直接中止进程。
    // 现在逐项校验「头部 + 个数 × 每项字节 ≤ 总长」，不满足一律按损坏处理并还原为空值。
    let 子标签 = 读u64(指针, 0);
    let 总长 = 读u64(指针, 8) as usize;
    if 总长 < 16 {
        return 运行时值::空值();
    }
    match 子标签 {
        线_文本 => {
            let 文本字节 = unsafe { std::slice::from_raw_parts(指针.add(16), 总长 - 16) };
            let s = String::from_utf8_lossy(文本字节).to_string();
            分配文本(s)
        }
        线_数组 => {
            if 总长 < 24 { return 运行时值::空值(); }
            let 数量 = 读u64(指针, 16) as usize;
            if !容器数量合法(总长, 24, 数量, 32) { return 运行时值::空值(); }
            let mut 偏移 = 24usize;
            let mut 元素 = Vec::with_capacity(数量);
            for _ in 0..数量 {
                let (rv, 步进) = 读运行时值(指针, 偏移);
                偏移 += 步进;
                元素.push(从外部值还原(rv));
            }
            分配数组(元素)
        }
        线_映射 => {
            if 总长 < 24 { return 运行时值::空值(); }
            let 数量 = 读u64(指针, 16) as usize;
            if !容器数量合法(总长, 24, 数量, 64) { return 运行时值::空值(); }
            let mut 偏移 = 24usize;
            let mut 项 = Vec::with_capacity(数量);
            for _ in 0..数量 {
                let (k, s1) = 读运行时值(指针, 偏移);
                偏移 += s1;
                let (val, s2) = 读运行时值(指针, 偏移);
                偏移 += s2;
                let 键 = 运行时值到堆键(从外部值还原(k));
                let 值 = 从外部值还原(val);
                项.push((键, 值));
            }
            分配映射(项)
        }
        线_结构体 => {
            if 总长 < 32 { return 运行时值::空值(); }
            let 类型索引 = 读u64(指针, 16) as usize;
            let 数量 = 读u64(指针, 24) as usize;
            if !容器数量合法(总长, 32, 数量, 32) { return 运行时值::空值(); }
            let mut 偏移 = 32usize;
            let mut 字段 = Vec::with_capacity(数量);
            for _ in 0..数量 {
                let (f, 步进) = 读运行时值(指针, 偏移);
                偏移 += 步进;
                字段.push(从外部值还原(f));
            }
            分配结构体(类型索引, 字段)
        }
        线_闭包 => {
            if 总长 < 32 { return 运行时值::空值(); }
            let 函数索引 = 读u64(指针, 16) as usize;
            let 数量 = 读u64(指针, 24) as usize;
            if !容器数量合法(总长, 32, 数量, 32) { return 运行时值::空值(); }
            let mut 偏移 = 32usize;
            let mut 捕获 = Vec::with_capacity(数量);
            for _ in 0..数量 {
                let (c, 步进) = 读运行时值(指针, 偏移);
                偏移 += 步进;
                捕获.push(从外部值还原(c));
            }
            分配闭包(函数索引, 捕获)
        }
        _ => 运行时值::空值(),
    }
}

/// 容器缓冲的元素个数是否与总长相容：头部字节 + 个数 × 每项字节 ≤ 总长（含溢出检查）。
fn 容器数量合法(总长: usize, 头部字节: usize, 数量: usize, 每项字节: usize) -> bool {
    数量.checked_mul(每项字节)
        .and_then(|n| n.checked_add(头部字节))
        .map(|n| n <= 总长)
        .unwrap_or(false)
}

/// 运行时值（还原后）→ 堆键：文本来自 标签_句柄(文本) 或 标签_外部值(文本)。
fn 运行时值到堆键(v: 运行时值) -> 堆键 {
    // 与 堆键到运行时值 对称：统一走 值转键（整数/逻辑/空值/小数/文本/结构体/枚举）。
    // 旧实现只认文本、整数、逻辑，其余一律落到 整数键 0：小数键、空值键的映射
    // 经外部调用往返后所有这类键都塌缩成同一个键 0，条目互相覆盖。
    值转键(&v).unwrap_or(堆键::整数(0))
}

// ── 释放：递归回收扁平缓冲 ──
/// 释放 标签_外部值 扁平缓冲（递归释放嵌套缓冲）；标量无操作。JNI 式所有权模型。
pub fn 释放外部值(v: 运行时值) {
    if v.标签 != 标签_外部值 {
        return;
    }
    let 指针 = v.整数 as u64 as *mut u8;
    if 指针.is_null() {
        return;
    }
    let 总长 = 读u64(指针, 8) as usize;
    // 头部损坏（总长不足一个头部）：无法得知原始分配大小，宁可泄漏也不按错误大小释放
    if 总长 < 16 {
        return;
    }
    let 子标签 = 读u64(指针, 0);
    释放嵌套(子标签, 指针, 总长);
    unsafe {
        let _ = Box::from_raw(std::slice::from_raw_parts_mut(指针, 总长));
    }
}

/// 递归释放 payload 中嵌套的 标签_外部值 缓冲。
fn 释放嵌套(子标签: u64, 指针: *mut u8, 总长: usize) {
    match 子标签 {
        线_数组 => {
            if 总长 < 24 { return; }
            let 数量 = 读u64(指针, 16) as usize;
            if !容器数量合法(总长, 24, 数量, 32) { return; }
            let mut 偏移 = 24usize;
            for _ in 0..数量 {
                let (rv, 步进) = 读运行时值(指针, 偏移);
                偏移 += 步进;
                if rv.标签 == 标签_外部值 {
                    释放外部值(rv);
                }
            }
        }
        线_映射 => {
            if 总长 < 24 { return; }
            let 数量 = 读u64(指针, 16) as usize;
            if !容器数量合法(总长, 24, 数量, 64) { return; }
            let mut 偏移 = 24usize;
            for _ in 0..数量 {
                let (k, s1) = 读运行时值(指针, 偏移);
                偏移 += s1;
                let (val, s2) = 读运行时值(指针, 偏移);
                偏移 += s2;
                if k.标签 == 标签_外部值 {
                    释放外部值(k);
                }
                if val.标签 == 标签_外部值 {
                    释放外部值(val);
                }
            }
        }
        线_结构体 | 线_闭包 => {
            if 总长 < 32 { return; }
            let 数量 = 读u64(指针, 24) as usize;
            if !容器数量合法(总长, 32, 数量, 32) { return; }
            let mut 偏移 = 32usize;
            for _ in 0..数量 {
                let (f, 步进) = 读运行时值(指针, 偏移);
                偏移 += 步进;
                if f.标签 == 标签_外部值 {
                    释放外部值(f);
                }
            }
        }
        _ => {}
    }
}

/// 从缓冲偏移处非对齐读取 u64（小端）。
fn 读u64(指针: *const u8, 偏移: usize) -> u64 {
    unsafe { std::ptr::read_unaligned::<u64>(指针.add(偏移) as *const u64) }
}

/// 从缓冲偏移处非对齐读取 32 字节 运行时值。
fn 读运行时值(指针: *const u8, 偏移: usize) -> (运行时值, usize) {
    let rv = unsafe { std::ptr::read_unaligned::<运行时值>(指针.add(偏移) as *const 运行时值) };
    (rv, 32)
}

// ============================================================
//  回调注册与查找（供 处理外部调用 和 FFI代码生成器 使用）
// ============================================================

/// 注册一个智语言函数为 C 回调，返回槽位 ID 和 trampoline 地址
/// 由 shim 代码生成器调用
pub fn 注册ffi回调(函数索引: usize, 签名形状: u64) -> Option<(usize, u64)> {
    let 签名 = match 签名形状 {
        0 => crate::回调管理器::回调签名::V空值I64,
        1 => crate::回调管理器::回调签名::I64V空值,
        2 => crate::回调管理器::回调签名::I64I64,
        3 => crate::回调管理器::回调签名::I64I64指针,
        4 => crate::回调管理器::回调签名::V空值指针,
        _ => crate::回调管理器::回调签名::动态(签名形状),
    };
    crate::回调管理器::注册回调(函数索引, 签名)
}

/// 释放回调槽位
pub fn 释放ffi回调(函数索引: usize) {
    crate::回调管理器::释放回调(函数索引)
}

/// 在外部调用中检查是否有回调类型参数，有则注册并把「函数索引」替换为 trampoline 地址。
/// 调用参数 为 shim 契约的扁平 运行时值 切片：函数指针参数编码为 标签=6、整数=-(函数索引+1)。
/// ★修复：原实现直接 `参数[i].整数 as usize`（负数 as usize 得极大值）且从不替换地址，
/// 导致回调形参传给 C 侧的是「函数索引」而非真实 trampoline 地址 → C 调用该地址即崩溃。
pub fn 处理外部调用的回调(项: &FFI项, 调用参数: &mut [运行时值]) -> usize {
    let mut 回调计数 = 0;
    for (i, &tag) in 项.形参标签.iter().enumerate() {
        if tag == 6 && i < 调用参数.len() { // 标签 6 = 函数指针
            if 调用参数[i].标签 == 6 && 调用参数[i].整数 != 0 {
                // ★解码 运行时值::函数指针 的负编码 `-1 - 函数索引`（见 值类型::函数指针）
                let 函数索引 = (-(调用参数[i].整数) - 1) as usize;
                // ★trampoline 形状直接来自 FFI项 元数据（语义层由函数指针声明映射），
                // 不再用基于「位置后参数数」的启发式（与实际签名不符 → trampoline 错配）。
                let 形状 = 项.函数指针形状.get(i).copied().unwrap_or(0);
                if let Some((_, 地址)) = 注册ffi回调(函数索引, 形状 as u64) {
                    // ★把「函数索引」替换为真实 trampoline 地址，C 侧才能正确回调智语言函数
                    调用参数[i].整数 = 地址 as i64;
                    回调计数 += 1;
                }
            }
        }
    }
    回调计数
}

/// 推断回调签名形状：基于参数列表和位置（保留作兜底，新路径直接用 FFI项.函数指针形状）
#[allow(dead_code)]
fn 回调签名推断(形参标签: &[u8], 回调位置: usize) -> u64 {
    let 后面参数数 = 形参标签.len().saturating_sub(回调位置 + 1);
    match 后面参数数 {
        0 => 1,
        1 => 2,
        _ => 3,
    }
}

/// 【缺口 C 修复】FFI 回调 dispatch 的 C ABI 入口（替代原 `系统调用(119)` 误路由）。
///
/// shim trampoline 经 `z_shim_set_callback_dispatch` 注册的回调分发器，原调用
/// `系统调用(119, 函数索引, 参数...)`，而 119 把函数索引当 FFI 命令码查询外部函数表
/// 失败 → 回调永不执行。此处改为独立入口，直接经「用户函数调用器」注册点执行用户函数，
/// 语义清晰且与外部调用（119）解耦，双模式自动 parity（都走 base 注册点）。
///
/// 参数：
///   函数索引: 智语言用户函数索引（≥200，编码规则见 值类型::函数指针）
///   参数指针: 扁平 运行时值 数组指针
///   数量: 数组长度
///   输出: 结果写回指针（32 字节 运行时值）
#[export_name = "智语言_系统调用回调"]
pub extern "C" fn 智语言_系统调用回调(
    函数索引: u64,
    参数指针: *const 运行时值,
    数量: u64,
    输出: *mut 运行时值,
) {
    // 结果交由 执行用户函数回调 经 输出 指针回写（双模式 用户函数调用器 统一写回）。
    执行用户函数回调(函数索引 as usize, 参数指针, 数量 as usize, 输出);
}

// ============================================================
//  C ABI 出口（供编译模式导出 thunk 调用 统一 FFI 编组层）
// ============================================================

/// 生成并编译 Rust FFI shim，返回生成的 .o/.lib 路径
/// 由构建管线在链接阶段前调用
///
/// 【双模式统一】无论解释/编译模式，始终：
/// 1. 生成 shim.rs 代码
/// 2. 编译为 cdylib → dlopen 注册包装调度函数（消除旧 transmute UB）
/// 3. 编译为 staticlib → 返回路径供链接器使用（仅编译模式需要，解释模式忽略）
/// 判断两个库文件是否可视为同一份（大小 + 修改时间一致）。用于跳过重复的大文件复制。
fn 文件内容相同(源: &std::path::Path, 目标: &std::path::Path) -> bool {
    let (Ok(源信息), Ok(目标信息)) = (std::fs::metadata(源), std::fs::metadata(目标)) else {
        return false;
    };
    if 源信息.len() != 目标信息.len() {
        return false;
    }
    match (源信息.modified(), 目标信息.modified()) {
        (Ok(甲), Ok(乙)) => 甲 == 乙,
        _ => false,
    }
}

pub fn 生成并编译shim(输出目录: &str, 外部库搜索路径: &[std::path::PathBuf]) -> Result<String, String> {
    // 剔除 Windows verbatim 前缀 \\?\，否则 rustc/链接器对带前缀路径处理异常
    // （项目模式下 输出目录 常带此前缀，独立文件模式不带；统一剔除保证一致）。
    // 跨平台：收敛进 平台::子进程路径（POSIX 原样返回）。
    // 剔除 Windows verbatim 前缀（子进程工具才能解析）+ 防御性清理回车/换行（路径不得含 CR/LF）
    let 输出目录 = crate::平台::子进程路径(&输出目录).replace('\r', "").replace('\n', "");

    // 1. 收集导入函数元数据（仅原生约定=true 的真实C ABI 函数走 shim；
    //    扁平约定（原生约定=false，如样例库）由 处理外部调用 的 transmute 回退路径直接调用，不生成 shim 包装）
    let 外部函数表锁 = 外部函数表.lock().unwrap();
    let 导入函数列表: Vec<super::FFI代码生成器::导入函数元数据> = 外部函数表锁.iter()
        .filter(|项| 项.原生约定)
        .map(|项| {
        // 🆕 P3: 优先使用 精确 C 类型码（生成关联 从 .h 推导的位宽），
        // 缺省（码 0）回退 标签到C类型 默认位宽（整数64/小数64），现有用例零影响。
        let 形参类型: Vec<super::FFI代码生成器::C类型> = 项.形参标签.iter()
            .enumerate()
            .map(|(i, &tag)| {
                // ★函数指针形参：用 函数指针形状[i] 携带 trampoline 形状码，
                // 否则 shim 会把地址 transmute 成 `fn()`（与 trampoline 实际签名不符 → UB）。
                if tag == 6 {
                    let 形状 = 项.函数指针形状.get(i).copied().unwrap_or(0);
                    return super::FFI代码生成器::C类型::函数指针(形状);
                }
                let 精确码 = 项.形参C类型.get(i).copied().unwrap_or(0);
                // 🆕 G1：结构体形参携带名字去布局表还原 #[repr(C)] 布局
                let 结构体名 = 项.形参结构体名.get(i).map(|s| s.as_str()).unwrap_or("");
                if 精确码 == 0 { 标签到C类型(tag) } else { 精确类型码到C类型带名(精确码, 结构体名) }
            })
            .collect();
        let 返回类型 = if 项.返回C类型 == 0 {
            标签到C类型(项.返回标签)
        } else {
            精确类型码到C类型带名(项.返回C类型, &项.返回结构体名)
        };
        super::FFI代码生成器::导入函数元数据 {
            键: 项.键,
            c符号: 项.c符号.clone(),
            库标识: 项.库标识.clone(),
            形参类型,
            返回类型,
            输出位图: 项.输出位图.clone(),
            是否静态: 项.是否静态,
            原生约定: 项.原生约定,
            是否全局变量: 项.是否全局变量,
            形参数组元素C类型: 项.形参数组元素C类型.clone(),
        }
    }).collect();
    drop(外部函数表锁);

    // 2. 生成 shim.rs 内容
    let 导出列表 = vec![]; // 导入方向不生成导出 thunk
    // 🆕 2026-09-25 修复既有缺陷：原调用 `生成shim` 会**丢弃诊断**（该函数注释自陈"诊断被忽略"），
    //   导致「外部结构体含不支持字段（函数指针 / 数组字段）」等**语义错误被静默吞掉**，
    //   继续编译出一个语义已错的 shim。现改用带诊断版本，按该 API 的设计意图：
    //   **诊断非空 ⇒ 明确报错退出**，绝不静默生成错误 shim。
    let (shim内容, 诊断列表) =
        super::FFI代码生成器::FFI代码生成器::生成shim带诊断(&导入函数列表, &导出列表);
    if !诊断列表.is_empty() {
        return Err(format!(
            "FFI shim 生成失败（语义错误，已拒绝生成语义错误的 shim）：\n{}",
            诊断列表.join("\n")
        ));
    }

    // 3. 写入文件
    // 注意：输出目录可能是带 \\?\ 前缀的 verbatim 路径，必须用 Path::join
    // （而非 "/") 拼接，否则 verbatim 路径下 '/' 非法（os error 123）。
    let shim目录 = std::path::Path::new(&输出目录)
        .join("外部语言文件")
        .join("执行文件");
    // 二次清洗：个别 Windows 环境下 Path::join 可能保留路径组件中混入的 CR/LF
    let shim目录 = std::path::PathBuf::from(
        shim目录.to_string_lossy().replace('\r', "").replace('\n', ""),
    );
    std::fs::create_dir_all(&shim目录).map_err(|e| format!("创建 shim 目录失败: {}", e))?;
    // ★规范 §6.5.3：把外部库搜索路径 + 全局 shim 目录中的所有 .dll 复制到 shim.dll 同目录，
    //   确保 shim.dll 运行时 LoadLibrary 能在同目录找到外部库 dll（避免 PATH/搜索路径问题）。
    let mut 候选搜索目录: Vec<std::path::PathBuf> = 外部库搜索路径.to_vec();
    候选搜索目录.push(std::path::PathBuf::from(输出目录.clone())); // 全局 shim 目录（单嵌套，持久部署点）
    // ★「先到先得」：搜索目录顺序为 外部库搜索路径(首项=静态库目录，即 cargo 产物，
    //   与 .lib 导入库同源，符号必然匹配) → 全局 shim 目录。首个含某 dll 的目录优先复制，
    //   后续目录同名 dll 不再覆盖，避免陈旧构建产物(项目 产出/执行文件)覆盖新鲜的 cargo 产物导致
    //   导入符号缺失(os_err=127)。规范 §6.5.3：shim.dll 运行时在同目录解析外部库 dll。
    let mut 已复制: std::collections::HashSet<String> = Default::default();
    for 搜索目录 in &候选搜索目录 {
        if let Ok(条目列表) = std::fs::read_dir(搜索目录) {
            for 条目 in 条目列表.flatten() {
                let 路径 = 条目.path();
                if 路径.extension().map_or(false, |e| e == "dll" || e == "so" || e == "dylib") {
                    let 文件名 = 路径.file_name().unwrap().to_string_lossy().to_string();
                    if 已复制.contains(&文件名) {
                        continue;
                    }
                    已复制.insert(文件名.clone());
                    let 目标 = shim目录.join(&文件名);
                    // ★性能：GTK4 这类桌面库一次要搬几十个、合计数百 MB 的 dll。
                    //   目标已存在且「大小 + 修改时间」都与源一致时视为同一份，跳过复制，
                    //   否则每次运行都要重搬一遍，表现为启动前长时间无响应。
                    if 文件内容相同(&路径, &目标) {
                        continue;
                    }
                    let _ = std::fs::copy(&路径, &目标);
                }
            }
        }
    }
    // ★【G10】非 ASCII 库名的安全别名副本。
    //   实测确证：`#[link(name = "样例库", kind = "raw-dylib")]` 会让 rustc 生成损坏的
    //   `样例库.dll_imports.lib`，链接器报 LNK1107；同一 .dll 改名 ASCII 后完全正常
    //   （rustc 把非 ASCII 库名写入 COFF 归档成员名时编码有误）。
    //   故 shim 里 `#[link(name=..)]` 一律用 链接库安全名，此处按该名在 shim 同目录
    //   落一份库文件副本（dll/so/dylib 供加载与 raw-dylib，lib/a 供静态链接），
    //   使链接器与运行时加载器都能按安全名解析。纯 ASCII 库名不产生副本，零影响。
    {
        let 库标识集合: std::collections::HashSet<String> =
            导入函数列表.iter().map(|f| f.库标识.clone()).collect();
        for 库标识 in &库标识集合 {
            let 安全名 = super::FFI代码生成器::FFI代码生成器::链接库安全名(库标识);
            if &安全名 == 库标识 {
                continue; // 已是 ASCII，无需别名
            }
            for 后缀 in ["dll", "so", "dylib", "lib", "a"] {
                let 源文件名 = format!("{}.{}", 库标识, 后缀);
                // 静态库/动态库在不同平台还可能带 lib 前缀（libX.so / libX.a）
                let 候选名 = [源文件名.clone(), format!("lib{}.{}", 库标识, 后缀)];
                let mut 已处理 = false;
                for 名 in &候选名 {
                    if 已处理 {
                        break;
                    }
                    for 目录 in std::iter::once(&shim目录).chain(候选搜索目录.iter()) {
                        let 源 = 目录.join(名);
                        if !源.exists() {
                            continue;
                        }
                        let 目标名 = if 名.starts_with("lib") && 名 != &源文件名 {
                            format!("lib{}.{}", 安全名, 后缀)
                        } else {
                            format!("{}.{}", 安全名, 后缀)
                        };
                        let _ = std::fs::copy(&源, shim目录.join(&目标名));
                        已处理 = true;
                        break;
                    }
                }
            }
        }
    }

    let shim路径 = std::path::PathBuf::from(
        shim目录
            .join("shim.rs")
            .to_string_lossy()
            .replace('\r', "")
            .replace('\n', ""),
    );
    std::fs::write(&shim路径, &shim内容).map_err(|e| format!("写入 shim.rs 失败: {}", e))?;

    // 4a. 编译 shim.rs 为 cdylib（用于 dlopen 注册包装函数）
    #[cfg(target_os = "windows")]
    let cdylib输出 = shim目录.join("z_shim_cdylib.dll");
    #[cfg(target_os = "macos")]
    let cdylib输出 = shim目录.join("libz_shim_cdylib.dylib");
    #[cfg(not(any(target_os = "windows", target_os = "macos")))]
    let cdylib输出 = shim目录.join("libz_shim_cdylib.so");
    // 清洗可能的 CR/LF 污染
    let cdylib输出 = std::path::PathBuf::from(
        cdylib输出.to_string_lossy().replace('\r', "").replace('\n', ""),
    );

    // 直接用 rustc（不经 cmd /C，避免 Windows 下 cmd 对含中文/CRLF 路径的解析损坏）
    let cdylib输出路径 = cdylib输出.to_string_lossy().to_string();
    let shim路径字符串 = shim路径.to_string_lossy().to_string();
    let mut cdylib命令 = std::process::Command::new("rustc");
    cdylib命令.args([
        "--crate-type",
        "cdylib",
        "--edition",
        "2021",
        "-o",
        &cdylib输出路径,
        &shim路径字符串,
    ]);
    // ★静态链接 C 运行时（仅 MSVC 目标）：样例库已用 +crt-static 消除 UCRT 动态依赖；
    //   shim 亦须同步，否则 rustc 默认动态链接 VCRUNTIME140.dll / api-ms-win-crt-*，
    //   而宿主（智语言开发套件.exe）的 EXE 目录不含这些运行库，LoadLibrary 解析到错误/缺失版本 → os_err=127。
    //   静态链接后 shim 仅依赖系统 KERNEL32/ntdll，可在任意路径（含中文路径）下加载。规范 §6.5.3。
    #[cfg(target_env = "msvc")]
    cdylib命令.arg("-C").arg("target-feature=+crt-static");
    // 静态链接外部库（真实C ABI）的导入库搜索目录
    // 剔除 Windows verbatim 前缀 \\?\（与 输出目录 一致处理），否则 rustc/链接器对带前缀路径处理异常。
    // ★顺序：输出目录（全局 shim/编译文件，staticlib 部署点）放【最前】，再外部库搜索路径。
    // 原因：外部库搜索路径常含 项目 产出/执行文件/ 的 dll【导入库】（小 .lib，如样例库.dll 的 2.58KB 导入表），
    // 若它先于 staticlib 被链接器找到，`#[link(kind="static")]` 会被解析为动态依赖 → shim dll 产生运行时 dll 依赖
    // （LoadLibrary 绝对路径加载不搜同目录 → 加载失败）。staticlib（通常 >1MB）置于 输出目录/全局 shim 优先命中。
    // 【G10】shim 目录放最前：非 ASCII 库名的「安全名副本」落在这里，
    // 必须先于原目录命中，否则链接器仍按原中文名去找（找不到或命中损坏导入库）。
    cdylib命令
        .arg("-L")
        .arg(crate::平台::子进程路径(&shim目录.to_string_lossy()));
    cdylib命令.arg("-L").arg(&输出目录);
    for 路径 in 外部库搜索路径 {
        let 路径字符串 = crate::平台::子进程路径(&路径.to_string_lossy());
        cdylib命令.arg("-L").arg(路径字符串);
    }
    let cdylib结果 = cdylib命令
        .output()
        .map_err(|e| format!("执行 rustc 编译 cdylib 失败: {}", e))?;

    if !cdylib结果.status.success() {
        let stderr = String::from_utf8_lossy(&cdylib结果.stderr);
        return Err(format!("shim cdylib 编译失败: {}", stderr));
    }

    // 4b. dlopen cdylib 并注册回调 trampoline 提供者
    //    shim 导入包装调度函数（z_shim_import_dispatch）现为真实C ABI 统一入口：
    //    凡「原生约定=true」（即外部库以 静态 关键字声明、走真实C类型）的函数，
    //    处理外部调用 一律经此调度到 shim 做运行时值↔原生C类型 编组；
    //    扁平 运行时值 约定函数（动态库）仍走旧 transmute 回退分支。双模式复用同一逻辑。
    {
        let 库句柄 = 平台::库::加载(cdylib输出.to_str().unwrap_or(""));
        if let Some(ref 库) = 库句柄 {
            // 设置回调调度函数指针（使 shim trampoline 可回调智语言运行时）
            if let Some(设置指针) = 库.取符号("z_shim_set_callback_dispatch") {
                type 调度设置函数 = unsafe extern "C" fn(unsafe extern "C" fn(u64, *const 运行时值, u64, *mut 运行时值));
                let 设置函数: 调度设置函数 = unsafe { std::mem::transmute(设置指针) };
                // 【缺口 C 修复】传递 智语言_系统调用回调（而非 智语言_系统调用）。
                // 原传 智语言_系统调用 导致 trampoline 调回系统函数分发器，把用户函数索引(≥200)
                // 当命令码路由到 119 默认分支 → 回调永不执行。现改走独立的回调 dispatch 入口。
                unsafe { 设置函数(智语言_系统调用回调 as unsafe extern "C" fn(u64, *const 运行时值, u64, *mut 运行时值)) };
            }
            // 注册回调 trampoline 提供者
            if let Some(回调指针) = 库.取符号("z_shim_register_callback") {
                let 回调注册器: super::回调管理器::外部注册函数 = unsafe { std::mem::transmute(回调指针) };
                super::回调管理器::设置外部注册器(回调注册器);
            }
            // 🆕 注册导入包装调度函数：原生约定=true 时 处理外部调用 路由到 shim 真实C ABI 路径（双模式统一）。
            if let Some(符号指针) = 库.取符号("z_shim_import_dispatch") {
                type 导入调度原型 = unsafe extern "C" fn(u64, *mut 运行时值, i64, *mut 运行时值);
                let 导入调度: 导入调度原型 = unsafe { std::mem::transmute(符号指针) };
                注册包装调度函数(导入调度);
            }
            // 缓存库句柄保持存活（函数指针引用其代码段）
            let mut 句柄缓存 = 包装库句柄.lock().unwrap();
            *句柄缓存 = 库句柄;
        } else {
            eprintln!("[FFI] 警告：无法加载 shim cdylib: {}", cdylib输出.display());
        }
    }

    // 4c. 编译 shim.rs 为静态库（供链接器使用）
    #[cfg(target_os = "windows")]
    let 库路径 = shim目录.join("z_shim.lib");
    #[cfg(not(target_os = "windows"))]
    let 库路径 = shim目录.join("libz_shim.a");
    // 清洗可能的 CR/LF 污染
    let 库路径 = std::path::PathBuf::from(
        库路径.to_string_lossy().replace('\r', "").replace('\n', ""),
    );

    let 库路径字符串 = 库路径.to_string_lossy().to_string();
    let mut staticlib命令 = std::process::Command::new("rustc");
    staticlib命令.args([
        "--crate-type",
        "staticlib",
        "--edition",
        "2021",
        "-o",
        &库路径字符串,
        &shim路径字符串,
    ]);
    // 静态链接外部库搜索目录（与 cdylib 编译共享相同 -L 路径）
    // 【G10】shim 目录同样放最前：非 ASCII 库名的安全名副本落在这里。
    staticlib命令
        .arg("-L")
        .arg(crate::平台::子进程路径(&shim目录.to_string_lossy()));
    for 路径 in 外部库搜索路径 {
        staticlib命令.arg("-L").arg(路径);
    }
    staticlib命令.arg("-L").arg(&输出目录);
    let 输出 = staticlib命令.output()
        .map_err(|e| format!("执行 rustc 编译 shim 失败: {}", e))?;

    if !输出.status.success() {
        let stderr = String::from_utf8_lossy(&输出.stderr);
        return Err(format!("shim.rs 编译失败: {}", stderr));
    }

    // 5. 生成 .h 头文件（置于 shim 目录，避免 verbatim 路径下 '..' 失效）
    {
        let shim头文件路径 = shim目录.join("z_shim.h");
        let 项目名 = "智语言FFI";
        let _ = std::fs::write(&shim头文件路径, format!(
            "// 自动生成: 智语言 FFI 导入函数类型描述\n// 项目: {}\n// 请参考 shim.rs 获取完整 extern \"C\" 声明\n\n", 项目名
        ));
    }

    // ★返回路径须剔除 Windows verbatim 前缀（\\?\），否则链接器（rust-lld/link.exe）
    // 无法打开该 .lib，静态库被惰性跳过，exe 缺失全部 shim/外部符号
    // （2026-08-05 学生点名系统发现：exe 无 z_shim_import_dispatch 致 FFI 调用静默失败）。
    // 跨平台：收敛进 平台::子进程路径（POSIX 原样返回）。
    Ok(crate::平台::子进程路径(&库路径.to_string_lossy()))
}

/// u8 标签 → C类型（兼容现有外部调用注册）
fn 标签到C类型(标签: u8) -> super::FFI代码生成器::C类型 {
    // 注：u8标签来自 FFI项.形参标签，只区分大类（整数/小数/文本等），
    // 不区分位宽（8/16/32/64）。当前 shim 生成统一使用 整数64/小数64。
    // 若要精确位宽，需在 FFI元数据 中存储原生 C 类型信息（见 精确类型码到C类型）。
    match 标签 {
        0 => super::FFI代码生成器::C类型::空值,
        1 => super::FFI代码生成器::C类型::整数64,
        2 => super::FFI代码生成器::C类型::小数64,
        3 => super::FFI代码生成器::C类型::逻辑,
        4 | 10 => super::FFI代码生成器::C类型::指针,
        5 => super::FFI代码生成器::C类型::字符串,
        6 => super::FFI代码生成器::C类型::函数指针(0),
        _ => super::FFI代码生成器::C类型::指针,
    }
}

/// 🆕 P3: 精确 C 类型码 → C类型（跨层契约，与 前端 公共降级.rs 的 模块到FFI项 对齐）。
///
/// 精确码由 生成关联 从 .h 自动推导（int→3、float→9、size_t→16 等）写入
/// FFI项.形参C类型/返回C类型；0=缺省（调用方回退 标签到C类型 默认位宽）。
/// 平台相关码（14..=19）直接映射 Rust std::os::raw，平台差异由 Rust 编译器消化。
///
/// | 码 | 智语言 FFI 类型名 | Rust 类型 | C 类型 |
/// |:--|:--|:--|:--|
/// | 0  | 缺省（整数/小数/文本/逻辑/空值/指针） | 标签到C类型 默认 | — |
/// | 1  | 单整数 | i8 | int8_t |
/// | 2  | 微整数 | i16 | short |
/// | 3  | 短整数 | i32 | int |
/// | 4  | 长整数 | i64 | long long |
/// | 5  | 无符号单整数 | u8 | uint8_t |
/// | 6  | 无符号微整数 | u16 | unsigned short |
/// | 7  | 无符号短整数 | u32 | unsigned int |
/// | 8  | 无符号长整数 | u64 | unsigned long long |
/// | 9  | 短小数 | f32 | float |
/// | 10 | 长小数 | f64 | double |
/// | 11 | 逻辑 | bool | _Bool |
/// | 12 | 文本 | *const i8 | const char* |
/// | 13 | 指针 | *mut c_void | void* |
/// | 14 | 平台整数 | c_long | long（平台相关）|
/// | 15 | 无符号平台整数 | c_ulong | unsigned long（平台相关）|
/// | 16 | 无符号位宽整数 | usize | size_t（usize 恒等于指针宽度）|
/// | 17 | 位宽整数 | isize | ssize_t |
/// | 18 | 字符 | c_char | char |
///
/// ⚠ 类型名以 前端 名称身份.rs 的 外部块类型名表 为唯一真相（2026-08-29 位宽前缀改版）：
/// 单(8)/微(16)/短(32)/长(64)/平台(随ABI)/位宽(恒等于指针宽)；无符号前置。
/// 此前本表误把码 4 写作「整数64」、码 14 写作「长整数」，与前端不一致，已更正。
/// | 19 | 无符号字符 | c_uchar | unsigned char |
/// | 20 | 函数指针 | extern "C" fn | 函数指针 |
/// | 21 | 结构体（外部） | 布局 | struct |
pub fn 精确类型码到C类型(码: u8) -> super::FFI代码生成器::C类型 {
    精确类型码到C类型带名(码, "")
}

/// 🆕 G1：带结构体名的精确类型码解析。
///
/// 码 21（外部结构体）**必须**配合结构体名才能还原布局：先查 外部结构体布局表，
/// 拿到字段的精确类型码序列后递归构造 `C类型::结构体(名, 字段)`，shim 生成器据此
/// 产出 `#[repr(C)]` 布局 + 序列化 helper（这两段代码本就完整，此前缺的只是这条通道，
/// 因而码 21 被无奈降级成 `C类型::指针` → shim 布局段为空 → 宿主还原越界崩溃）。
/// 名字为空或布局未注册时保持旧行为（不透明指针），保证渐进兼容。
pub fn 精确类型码到C类型带名(码: u8, 结构体名: &str) -> super::FFI代码生成器::C类型 {
    use super::FFI代码生成器::C类型;
    if 码 == 21 {
        if !结构体名.is_empty() {
            if let Some(字段码列表) = 取外部结构体布局(结构体名) {
                // 字段递归解析（**步进式**）：数组字段占两字节 `[布局码_外部数组字段][元素码]`，
                // 其余字段占一字节。数组字段按规范数组映射表解析为 `C类型::数组(元素)` ⇒ C 指针 `X*`。
                let mut 字段: Vec<C类型> = Vec::with_capacity(字段码列表.len());
                let mut i = 0usize;
                while i < 字段码列表.len() {
                    let 字段码 = 字段码列表[i];
                    // 数组字段：[22][元素码] ⇒ C 指针 `X*`（元素类型参与按宽度编组）
                    if 字段码 == super::FFI代码生成器::布局码_外部数组字段 && i + 1 < 字段码列表.len() {
                        let 元素码 = 字段码列表[i + 1];
                        字段.push(C类型::数组(Box::new(精确类型码到C类型带名(元素码, ""))));
                        i += 2;
                        continue;
                    }
                    // 嵌套结构体字段：[21][名字长 u8][名字 UTF-8 字节…] ⇒ 按名字递归还原 #[repr(C)] 布局
                    //   （旧编码无名字 ⇒ 会退化为不透明指针，嵌套字段即不可用）。
                    if 字段码 == 21 && i + 1 < 字段码列表.len() {
                        let 名长 = 字段码列表[i + 1] as usize;
                        if i + 2 + 名长 <= 字段码列表.len() {
                            let 名 = String::from_utf8_lossy(&字段码列表[i + 2..i + 2 + 名长]).to_string();
                            字段.push(精确类型码到C类型带名(21, &名));
                            i += 2 + 名长;
                            continue;
                        }
                    }
                    字段.push(精确类型码到C类型带名(字段码, ""));
                    i += 1;
                }
                return C类型::结构体(结构体名.to_string(), 字段);
            }
        }
        return C类型::指针;
    }
    match 码 {
        1 => C类型::整数8,
        2 => C类型::整数16,
        3 => C类型::整数32,
        4 => C类型::整数64,
        5 => C类型::无符号8,
        6 => C类型::无符号16,
        7 => C类型::无符号32,
        8 => C类型::无符号64,
        9 => C类型::小数32,
        10 => C类型::小数64,
        11 => C类型::逻辑,
        12 => C类型::字符串,
        13 => C类型::指针,
        14 => C类型::C长整数,
        15 => C类型::C无符号长整数,
        16 => C类型::C字节数量,
        17 => C类型::C有符号字节数量,
        18 => C类型::C字符,
        19 => C类型::C无符号字符,
        20 => C类型::函数指针(0),
        // 码 21（结构体）已在函数开头按名解析，此处不再出现
        _ => C类型::指针,
    }
}

#[export_name = "智语言_从外部值还原"]
pub extern "C" fn 智语言_从外部值还原(输出: *mut 运行时值, 输入标签: u64, 输入载荷: i64) {
    let v = 运行时值 { 标签: 输入标签, 整数: 输入载荷, 小数: 0.0, 句柄: 0 };
    let 还原值 = 从外部值还原(v);
    写运行时值到(输出, &还原值);
}

#[export_name = "智语言_编组为外部值"]
pub extern "C" fn 智语言_编组为外部值(输出: *mut 运行时值, 输入标签: u64, 输入载荷: i64) {
    let v = crate::堆操作::重建运行时值(输入标签, 输入载荷);
    写运行时值到(输出, &编组为外部值(v));
}

#[export_name = "智语言_释放外部值"]
pub extern "C" fn 智语言_释放外部值(输入标签: u64, 输入载荷: i64) {
    let v = 运行时值 { 标签: 输入标签, 整数: 输入载荷, 小数: 0.0, 句柄: 0 };
    释放外部值(v);
}

// ============================================================
//  导出方向（智语言 → C）的文本编组
//  编译模式「导出 thunk」按规范 §6.5.3 使用真实 C 签名，文本在边界上是
//  C 原生字符串（`const char*`，NUL 结尾），不是 `[子标签][总长]` 的外部值缓冲。
//  ⇒ 这一组是 C 字符串 ↔ 堆文本 的唯一真相入口，thunk 只负责调用，不做字节搬运。
// ============================================================

/// 导出 thunk 入参：C 字符串 → 堆文本，结果写入 32 字节 运行时值 槽。
/// 空指针按「空值」传递（C 侧未提供该参数时的安全退化）。
#[export_name = "智语言_文本入参"]
pub unsafe extern "C" fn 智语言_文本入参(输入: *const std::os::raw::c_char, 输出槽: *mut 运行时值) {
    if 输出槽.is_null() {
        return;
    }
    if 输入.is_null() {
        写运行时值到(输出槽, &运行时值::空值());
        return;
    }
    let 原始 = std::ffi::CStr::from_ptr(输入);
    let 文本 = match 原始.to_str() {
        Ok(有效) => 有效.to_string(),
        Err(_) => String::from_utf8_lossy(原始.to_bytes()).to_string(),
    };
    写运行时值到(输出槽, &分配文本(文本));
}

/// 导出 thunk 返回：堆文本 → C 字符串（NUL 结尾副本，调用方用 `智语言_释放C字符串` 回收）。
/// 非文本值返回空指针（C 侧对 `空值` 不做解引用即可）。
#[export_name = "智语言_文本出参"]
pub extern "C" fn 智语言_文本出参(标签: u64, 载荷: i64) -> *const std::os::raw::c_char {
    if 标签 != 标签_句柄 {
        return std::ptr::null();
    }
    match 堆读取文本字符串(载荷 as u64) {
        Some(文本) => {
            let mut 字节 = 文本.into_bytes();
            字节.push(0);
            Box::into_raw(字节.into_boxed_slice()) as *const u8 as *const std::os::raw::c_char
        }
        None => std::ptr::null(),
    }
}

/// 回收 `智语言_文本出参` 返回的 C 字符串（长度按 NUL 终止符还原）。
#[export_name = "智语言_释放C字符串"]
pub unsafe extern "C" fn 智语言_释放C字符串(指针: *const std::os::raw::c_char) {
    if 指针.is_null() {
        return;
    }
    let 长度 = std::ffi::CStr::from_ptr(指针).to_bytes().len() + 1;
    let 切片 = std::ptr::slice_from_raw_parts_mut(指针 as *mut u8, 长度);
    drop(Box::from_raw(切片));
}

/// 把 运行时值 写入输出指针（输出指针由 thunk 提供，已对齐 16 字节）。
fn 写运行时值到(输出: *mut 运行时值, v: &运行时值) {
    if 输出.is_null() {
        return;
    }
    unsafe {
        (*输出).标签 = v.标签;
        (*输出).整数 = v.整数;
        (*输出).小数 = v.小数;
        (*输出).句柄 = v.句柄;
    }
}

#[cfg(test)]
mod 编组层测试 {
    use super::*;

    fn 取文本(v: 运行时值) -> String {
        if v.标签 == 标签_句柄 {
            堆读取文本字符串(v.句柄).unwrap_or_default()
        } else {
            String::new()
        }
    }

    #[test]
    fn 损坏的元素个数不越界() {
        let 编组值 = 编组为外部值(分配数组(vec![运行时值::整数(1), 运行时值::整数(2)]));
        let 指针 = 编组值.整数 as u64 as *mut u8;
        // 把元素个数改成一个巨大的值：旧实现会按它预分配并越界读取
        unsafe { std::ptr::write_unaligned(指针.add(16) as *mut u64, u64::MAX / 2) };
        assert_eq!(从外部值还原(编组值).标签, 标签_空值);
        // 个数只多 1 同样必须拒绝（刚好越过缓冲末尾）
        unsafe { std::ptr::write_unaligned(指针.add(16) as *mut u64, 3) };
        assert_eq!(从外部值还原(编组值).标签, 标签_空值);
        // 恢复后正常还原并释放
        unsafe { std::ptr::write_unaligned(指针.add(16) as *mut u64, 2) };
        assert_eq!(从外部值还原(编组值).标签, 标签_句柄);
        释放外部值(编组值);
    }

    #[test]
    fn 损坏的总长不下溢() {
        let 编组值 = 编组为外部值(分配文本("甲".to_string()));
        let 指针 = 编组值.整数 as u64 as *mut u8;
        let 原总长 = 读u64(指针, 8);
        unsafe { std::ptr::write_unaligned(指针.add(8) as *mut u64, 8) };
        assert_eq!(从外部值还原(编组值).标签, 标签_空值);
        unsafe { std::ptr::write_unaligned(指针.add(8) as *mut u64, 原总长) };
        释放外部值(编组值);
    }

    #[test]
    fn 小数键与空值键往返不塌缩() {
        let 映射值 = 分配映射(vec![
            (堆键::小数(1.5f64.to_bits()), 运行时值::整数(1)),
            (堆键::小数(2.5f64.to_bits()), 运行时值::整数(2)),
            (堆键::空值, 运行时值::整数(3)),
        ]);
        let 编组值 = 编组为外部值(映射值);
        let 还原 = 从外部值还原(编组值);
        if let Some(堆数据::映射(m)) = 堆读取(还原.句柄) {
            assert_eq!(m.len(), 3, "三个键不应互相覆盖");
            assert!(m.iter().any(|(k, v)| matches!(k, 堆键::小数(位) if *位 == 1.5f64.to_bits()) && v.整数 == 1));
            assert!(m.iter().any(|(k, v)| matches!(k, 堆键::小数(位) if *位 == 2.5f64.to_bits()) && v.整数 == 2));
            assert!(m.iter().any(|(k, v)| matches!(k, 堆键::空值) && v.整数 == 3));
        } else {
            panic!("应为映射");
        }
        释放外部值(编组值);
    }

    #[test]
    fn 文本往返() {
        let 原文 = "你好，智语言";
        let 编组值 = 编组为外部值(分配文本(原文.to_string()));
        assert_eq!(编组值.标签, 标签_外部值);
        let 还原 = 从外部值还原(编组值);
        assert_eq!(取文本(还原), 原文);
        释放外部值(编组值);
    }

    #[test]
    fn 导出文本往返() {
        use std::ffi::{CStr, CString};
        let 原文 = "你好，导出";
        let c字符串 = CString::new(原文).unwrap();
        let mut 槽 = 运行时值::空值();
        unsafe { 智语言_文本入参(c字符串.as_ptr(), &mut 槽) };
        assert_eq!(槽.标签, 标签_句柄, "文本入参应产出堆文本句柄");

        let 指针 = 智语言_文本出参(槽.标签, 槽.句柄 as i64);
        assert!(!指针.is_null(), "文本出参应返回 C 字符串");
        let 还原 = unsafe { CStr::from_ptr(指针) }.to_str().unwrap().to_string();
        assert_eq!(还原, 原文);
        unsafe { 智语言_释放C字符串(指针) };

        // 非文本返回 → 空指针（C 侧对 空值 不解引用）
        assert!(智语言_文本出参(标签_空值, 0).is_null());
    }

    #[test]
    fn 数组往返() {
        let 数组值 = 分配数组(vec![
            运行时值::整数(1),
            运行时值::整数(2),
            分配文本("x".into()),
        ]);
        let 编组值 = 编组为外部值(数组值);
        let 还原 = 从外部值还原(编组值);
        if let Some(堆数据::数组(a)) = 堆读取(还原.句柄) {
            assert_eq!(a.len(), 3);
            assert_eq!(a[0].整数, 1);
            assert_eq!(a[2].标签, 标签_句柄);
        } else {
            panic!("应为数组");
        }
        释放外部值(编组值);
    }

    #[test]
    fn 映射往返() {
        let 映射值 = 分配映射(vec![
            (堆键::文本("k".into()), 运行时值::整数(9)),
            (堆键::整数(2), 分配文本("v".into())),
        ]);
        let 编组值 = 编组为外部值(映射值);
        let 还原 = 从外部值还原(编组值);
        if let Some(堆数据::映射(m)) = 堆读取(还原.句柄) {
            assert_eq!(m.len(), 2);
        } else {
            panic!("应为映射");
        }
        释放外部值(编组值);
    }

    #[test]
    fn 结构体往返() {
        let 结构体值 = 分配结构体(5, vec![运行时值::整数(7), 分配文本("字段".into())]);
        let 编组值 = 编组为外部值(结构体值);
        let 还原 = 从外部值还原(编组值);
        if let Some(堆数据::结构体 { 类型索引, 字段值 }) = 堆读取(还原.句柄) {
            assert_eq!(类型索引, 5);
            assert_eq!(字段值[0].整数, 7);
        } else {
            panic!("应为结构体");
        }
        释放外部值(编组值);
    }

    #[test]
    fn 标量透传() {
        let 编组值 = 编组为外部值(运行时值::整数(42));
        assert_eq!(编组值.标签, 标签_整数);
        assert_eq!(编组值.整数, 42);
        let 还原 = 从外部值还原(编组值);
        assert_eq!(还原.整数, 42);
        释放外部值(编组值);
    }
}

// ============================================================
//  C ABI 出口（供编译模式 prologue 注册外部函数）
// ============================================================
#[export_name = "智语言_注册外部函数"]
pub extern "C" fn 注册外部函数_导出(
    键: u64,
    c符号: *const u8,
    c符号长: u64,
    库标识: *const u8,
    库标识长: u64,
    返回标签: u64,
    形参标签数组: *const u8,
    形参标签长: u64,
    输出位图数组: *const u8,
    输出位图长: u64,
    是否静态: u64,
    原生约定: u64,
    // 🆕 P3: 精确 C 类型码（0=缺省回退 标签到C类型 默认位宽）
    形参C类型数组: *const u8,
    形参C类型长: u64,
    返回C类型: u64,
    // 🆕 回调：函数指针形参的 trampoline 形状码（255=非函数指针），编译模式回调必需
    函数指针形状数组: *const u8,
    函数指针形状长: u64,
) {
    let (c符号, 库标识, 形参标签, 输出位图, 形参C类型, 函数指针形状) = unsafe {
        (
            读取文本(c符号, c符号长),
            读取文本(库标识, 库标识长),
            读取字节数组(形参标签数组, 形参标签长),
            读取字节数组(输出位图数组, 输出位图长),
            读取字节数组(形参C类型数组, 形参C类型长),
            读取字节数组(函数指针形状数组, 函数指针形状长),
        )
    };
    注册外部函数(FFI项 {
        键: 键 as usize,
        c符号,
        库标识,
        形参标签,
        返回标签: 返回标签 as u8,
        输出位图: 输出位图.into_iter().map(|b| b != 0).collect(),
        是否静态: 是否静态 != 0,
        原生约定: 原生约定 != 0,
        形参C类型,
        返回C类型: 返回C类型 as u8,
        函数指针形状,
        是否全局变量: false, // 是否全局变量走旁路补充（智语言_设置FFI全局变量标记），不扩充本函数的 17 参签名
        // 结构体名走旁路补充（智语言_设置FFI结构体名），不扩充本函数的 17 参签名——
        // 见 FFI项.形参结构体名 的注释：跨层签名漂移是 FFI 最隐蔽的 bug 源。
        形参结构体名: vec![],
        返回结构体名: String::new(),
        // 数组元数据走旁路补充（智语言_设置FFI数组元数据），不扩充本函数的 17 参签名。
        形参数组元素C类型: vec![],
        形参数组长度索引: vec![],
    });
}

/// 🆕 G1 C ABI：注册外部结构体的字段布局（旁路通道，与 注册外部函数 解耦）。
/// 字段类型码数组 = 各字段的精确 C 类型码，顺序即声明顺序。
#[export_name = "智语言_注册外部结构体布局"]
pub extern "C" fn 注册外部结构体布局_导出(
    名称: *const u8,
    名称长: u64,
    字段类型码数组: *const u8,
    字段类型码长: u64,
) {
    let (名称, 字段类型码) = unsafe {
        (
            读取文本(名称, 名称长),
            读取字节数组(字段类型码数组, 字段类型码长),
        )
    };
    注册外部结构体布局(名称, 字段类型码);
}

/// 🆕 G1 C ABI：为已注册的外部函数补充「结构体形参/返回值的结构体名」。
/// 形参结构体名以 `\u{1}` 分隔打包为单个字符串（形参数 = 分段数，非结构体形参为空段），
/// 避免再引入 `*const *const u8` 二维数组带来的跨层签名复杂度。
#[export_name = "智语言_设置FFI结构体名"]
pub extern "C" fn 设置FFI结构体名_导出(
    键: u64,
    形参结构体名打包: *const u8,
    形参结构体名打包长: u64,
    返回结构体名: *const u8,
    返回结构体名长: u64,
) {
    let (打包, 返回名) = unsafe {
        (
            读取文本(形参结构体名打包, 形参结构体名打包长),
            读取文本(返回结构体名, 返回结构体名长),
        )
    };
    let 形参名列表: Vec<String> = if 打包.is_empty() {
        vec![]
    } else {
        打包.split('\u{1}').map(|s| s.to_string()).collect()
    };
    设置FFI结构体名(键 as usize, 形参名列表, 返回名);
}

/// 为已注册的外部函数补充结构体名（解释模式直接调用本 Rust API）。
pub fn 设置FFI结构体名(键: usize, 形参结构体名: Vec<String>, 返回结构体名: String) {
    let mut 表 = 外部函数表.lock().unwrap();
    if let Some(项) = 表.get_mut(键) {
        项.形参结构体名 = 形参结构体名;
        项.返回结构体名 = 返回结构体名;
    }
}

/// 🆕 FFI（§6.5.2 第5项）：标记某外部函数为「C 全局变量」。
/// shim 据此把 `extern "C" { fn 符号() -> T }` 改为 `extern "C" { static 符号: T }` 读取，
/// 否则调用变量地址处的机器码属于未定义行为。
/// 使用旁路注册（与 设置FFI结构体名 同模式），不扩充 `智语言_注册外部函数` 的 17 参签名，
/// 避免跨层 C ABI 漂移。
#[export_name = "智语言_设置FFI全局变量标记"]
pub extern "C" fn 设置FFI全局变量标记_导出(键: u64) {
    设置FFI是否全局变量(键 as usize, true);
}

/// 标记某外部函数为「C 全局变量」（解释模式直接调用本 Rust API）。
pub fn 设置FFI是否全局变量(键: usize, 是否: bool) {
    let mut 表 = 外部函数表.lock().unwrap();
    if let Some(项) = 表.get_mut(键) {
        项.是否全局变量 = 是否;
    }
}

/// 🆕 2026-08-29 FFI 数组：为已注册的外部函数补充「数组元数据」
/// （每形参数组元素外部类型码 + 数组形参的紧邻长度参数索引）。
/// 走旁路注册（与 设置FFI结构体名 同模式），不扩充 `智语言_注册外部函数` 的 17 参签名，
/// 避免跨层 C ABI 漂移。
#[export_name = "智语言_设置FFI数组元数据"]
pub extern "C" fn 设置FFI数组元数据_导出(
    键: u64,
    元素码数组: *const u8,
    元素码长: u64,
    长度索引数组: *const u8,
    长度索引长: u64,
) {
    // 长度索引数组按 u8 字节传入（编译模式经 嵌入字节 写入数据段，非 8 字节对齐，
    // 若按 *const u64 读会触发 slice::from_raw_parts 对齐断言 panic）。
    // 每元素 8 字节（u64 LE），chunks_exact(8) 还原为 usize。
    let (元素码, 长度索引) = unsafe {
        (
            读取字节数组(元素码数组, 元素码长),
            读取字节数组(长度索引数组, 长度索引长)
                .chunks_exact(8)
                .map(|c| u64::from_le_bytes(c.try_into().unwrap_or([0u8; 8])) as usize)
                .collect(),
        )
    };
    设置FFI数组元数据(键 as usize, 元素码, 长度索引);
}

/// 为已注册的外部函数补充数组元数据（解释模式直接调用本 Rust API）。
pub fn 设置FFI数组元数据(键: usize, 元素码: Vec<u8>, 长度索引: Vec<usize>) {
    let mut 表 = 外部函数表.lock().unwrap();
    if let Some(项) = 表.get_mut(键) {
        项.形参数组元素C类型 = 元素码;
        项.形参数组长度索引 = 长度索引;
    }
}

/// 🆕 2026-08-29 FFI 数组：把智语言数组序列化为 C ABI 紧凑缓冲（元素按精确码转 C 布局）。
/// 元素码与 精确类型码到C类型 严格对齐：1=i8,2=i16,3=i32,4=i64,5=u8,6=u16,7=u32,8=u64,
/// 9=f32,10=f64,14=c_long,15=c_ulong,16=usize,17=isize,18=c_char。
/// 平台相关码（14..=17）按 std::os::raw 目标平台实际位宽写（to_le_bytes 自动取类型大小）。
fn 序列化数组为C紧凑缓冲(数组: 运行时值, 元素码: u8, 长度: usize) -> Vec<u8> {
    let mut 缓冲 = Vec::with_capacity(长度 * 8);
    for i in 1..=长度 {
        let 元素 = crate::堆操作::堆操作(crate::堆操作::堆_数组加载, &[数组.clone(), 运行时值::整数(i as i64)]);
        match 元素码 {
            1 => 缓冲.extend_from_slice(&(元素.整数 as i8).to_le_bytes()),
            2 => 缓冲.extend_from_slice(&(元素.整数 as i16).to_le_bytes()),
            3 => 缓冲.extend_from_slice(&(元素.整数 as i32).to_le_bytes()),
            4 => 缓冲.extend_from_slice(&元素.整数.to_le_bytes()),
            5 => 缓冲.push(元素.整数 as u8),
            6 => 缓冲.extend_from_slice(&(元素.整数 as u16).to_le_bytes()),
            7 => 缓冲.extend_from_slice(&(元素.整数 as u32).to_le_bytes()),
            8 => 缓冲.extend_from_slice(&(元素.整数 as u64).to_le_bytes()),
            9 => 缓冲.extend_from_slice(&(元素.小数 as f32).to_le_bytes()),
            10 => 缓冲.extend_from_slice(&元素.小数.to_le_bytes()),
            14 => 缓冲.extend_from_slice(&(元素.整数 as std::os::raw::c_long).to_le_bytes()),
            15 => 缓冲.extend_from_slice(&(元素.整数 as std::os::raw::c_ulong).to_le_bytes()),
            16 => 缓冲.extend_from_slice(&(元素.整数 as usize).to_le_bytes()),
            17 => 缓冲.extend_from_slice(&(元素.整数 as isize).to_le_bytes()),
            18 => 缓冲.push(元素.整数 as u8), // c_char 恒 1 字节
            _ => {}
        }
    }
    缓冲
}

/// 🆕 2026-08-29 FFI 数组：从 C 紧凑缓冲读回第 index 个元素（按元素码反序列化为 运行时值）。
/// 平台相关码（14..=17）按目标平台实际位宽读（4/8 字节分支），有符号码符号扩展为 i64。
fn 从C紧凑缓冲读元素(缓冲: &[u8], 元素码: u8, index: usize) -> 运行时值 {
    let 读 = |偏移: usize, 宽度: usize| -> &[u8] { &缓冲[偏移..偏移 + 宽度] };
    match 元素码 {
        1 => 运行时值::整数(i8::from_le_bytes(读(index * 1, 1).try_into().unwrap()) as i64),
        2 => 运行时值::整数(i16::from_le_bytes(读(index * 2, 2).try_into().unwrap()) as i64),
        3 => 运行时值::整数(i32::from_le_bytes(读(index * 4, 4).try_into().unwrap()) as i64),
        4 => 运行时值::整数(i64::from_le_bytes(读(index * 8, 8).try_into().unwrap())),
        5 => 运行时值::整数(u8::from_le_bytes(读(index * 1, 1).try_into().unwrap()) as i64),
        6 => 运行时值::整数(u16::from_le_bytes(读(index * 2, 2).try_into().unwrap()) as i64),
        7 => 运行时值::整数(u32::from_le_bytes(读(index * 4, 4).try_into().unwrap()) as i64),
        8 => 运行时值::整数(u64::from_le_bytes(读(index * 8, 8).try_into().unwrap()) as i64),
        9 => 运行时值::小数(f32::from_le_bytes(读(index * 4, 4).try_into().unwrap()) as f64),
        10 => 运行时值::小数(f64::from_le_bytes(读(index * 8, 8).try_into().unwrap())),
        14 => {
            let 宽 = std::mem::size_of::<std::os::raw::c_long>();
            if 宽 == 4 {
                运行时值::整数(i32::from_le_bytes(读(index * 4, 4).try_into().unwrap()) as i64)
            } else {
                运行时值::整数(i64::from_le_bytes(读(index * 8, 8).try_into().unwrap()))
            }
        }
        15 => {
            let 宽 = std::mem::size_of::<std::os::raw::c_ulong>();
            if 宽 == 4 {
                运行时值::整数(u32::from_le_bytes(读(index * 4, 4).try_into().unwrap()) as i64)
            } else {
                运行时值::整数(u64::from_le_bytes(读(index * 8, 8).try_into().unwrap()) as i64)
            }
        }
        16 => {
            let 宽 = std::mem::size_of::<usize>();
            if 宽 == 4 {
                运行时值::整数(u32::from_le_bytes(读(index * 4, 4).try_into().unwrap()) as i64)
            } else {
                运行时值::整数(u64::from_le_bytes(读(index * 8, 8).try_into().unwrap()) as i64)
            }
        }
        17 => {
            let 宽 = std::mem::size_of::<isize>();
            if 宽 == 4 {
                运行时值::整数(i32::from_le_bytes(读(index * 4, 4).try_into().unwrap()) as i64)
            } else {
                运行时值::整数(i64::from_le_bytes(读(index * 8, 8).try_into().unwrap()))
            }
        }
        18 => 运行时值::整数(std::os::raw::c_char::from_le_bytes([读(index * 1, 1)[0]]) as i64),
        _ => 运行时值::空值(),
    }
}

/// C ABI：注册 shim 包装调度函数（编译模式 exe 启动时由 Cranelift+生成登记 shim 分发函数）
#[export_name = "智语言_注册包装调度函数"]
pub extern "C" fn 注册包装调度函数_导出(调度地址: u64) {
    if 调度地址 != 0 {
        let 调度: 包装调度函数 = unsafe { std::mem::transmute(调度地址) };
        注册包装调度函数(调度);
    }
}

/// C ABI：注册 shim 回调 trampoline 提供者（编译模式 exe 启动时登记 z_shim_register_callback）。
/// 解释模式经 dlopen 取符号注册；编译模式 shim 静态链接进 exe，须经此 C ABI 显式登记，
/// 否则 回调管理器.外部注册器 为 None → 注册ffi回调 回退自管槽位表（trampoline 地址=0）→
/// C 侧调用 0 地址崩溃（0xC0000005）。与 智语言_系统调用回调 配套，双模式 parity。
#[export_name = "智语言_注册回调提供者"]
pub extern "C" fn 注册回调提供者_导出(注册器地址: u64) {
    if 注册器地址 != 0 {
        let 注册器: super::回调管理器::外部注册函数 = unsafe { std::mem::transmute(注册器地址) };
        super::回调管理器::设置外部注册器(注册器);
    }
}

/// C ABI：设置 shim 回调调度函数（编译模式 exe 启动时登记 z_shim_set_callback_dispatch，
/// 传 智语言_系统调用回调，使 shim trampoline 回调回智语言用户函数）。
#[export_name = "智语言_设置回调调度"]
pub extern "C" fn 设置回调调度_导出(设置函数地址: u64) {
    if 设置函数地址 != 0 {
        type 调度设置函数 = unsafe extern "C" fn(unsafe extern "C" fn(u64, *const 运行时值, u64, *mut 运行时值));
        let 设置函数: 调度设置函数 = unsafe { std::mem::transmute(设置函数地址) };
        unsafe { 设置函数(智语言_系统调用回调 as unsafe extern "C" fn(u64, *const 运行时值, u64, *mut 运行时值)) };
    }
}

/// C ABI：注册编译模式回调桥函数（exe 启动时由 Cranelift 生成并登记「按索引调用」thunk）。
/// 解释模式经 RAII 守卫直接调 Rust `注册用户函数调用器(解释器回调桥, vm_ptr)`；
/// 编译模式无 VM，须经此 C ABI 把生成的 thunk 地址登记为 用户函数调用器
/// （上下文=0，thunk 内部已编译进本机函数表，自含按索引分派能力）。
/// 此为 智语言_系统调用回调 → 执行用户函数回调 → 用户函数调用器 链路的最后一环，
/// 缺则编译模式 FFI 回调 trampoline 调回后无桥可达用户函数 → 返回空值/崩溃。
#[export_name = "智语言_注册用户函数调用器"]
pub extern "C" fn 注册用户函数调用器_导出(调用器地址: u64) {
    if 调用器地址 != 0 {
        let 调用器: 用户函数调用器 = unsafe { std::mem::transmute(调用器地址) };
        注册用户函数调用器(调用器, 0);
    }
}

unsafe fn 读取文本(指针: *const u8, 长: u64) -> String {
    if 指针.is_null() || 长 == 0 {
        return String::new();
    }
    let 切片 = std::slice::from_raw_parts(指针, 长 as usize);
    String::from_utf8_lossy(切片).to_string()
}

unsafe fn 读取字节数组(指针: *const u8, 长: u64) -> Vec<u8> {
    if 指针.is_null() || 长 == 0 {
        return vec![];
    }
    std::slice::from_raw_parts(指针, 长 as usize).to_vec()
}
