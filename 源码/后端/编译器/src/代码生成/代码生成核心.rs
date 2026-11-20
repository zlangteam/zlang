// 文件: 后端/编译器/src/代码生成/代码生成核心.rs
// 职责: 底层表示 → Cranelift IR → 目标文件，完整支持结构体/数组/函数调用（含参数传递）和DWARF调试信息

use cranelift_codegen::ir::{types, AbiParam, GlobalValue, InstBuilder, Signature, StackSlotData, StackSlotKind};
use cranelift_codegen::isa::{self, CallConv};
use cranelift_codegen::settings::{self, Configurable};
use cranelift_codegen::Context;
use cranelift_frontend::{FunctionBuilder, FunctionBuilderContext, Variable};
use cranelift_module::{DataId, Linkage, Module, FuncId};
use cranelift_object::{ObjectBuilder, ObjectModule};
use target_lexicon::Triple;
use std::collections::{HashMap, HashSet};
use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};

/// 对象文件唯一序号：每次编译自增，配合进程号生成唯一临时对象文件名，
/// 杜绝多文件顺序编译时复用同一固定临时对象文件导致的「串台」（A 的 exe 跑出 B 的代码）。
static 对象文件计数器: AtomicU64 = AtomicU64::new(0);

use frontend::底层表示::{
    指令集::{操作码, 操作数},
    模块结构::{异常目标, 底层表示模块, 常量值},
};
use super::优化管线::{优化管线 as 优化管线器, 优化级别};
use super::调试信息::调试信息生成器;

/// 统一值表示：每个局部 = (标签, 载荷) 双槽 i64，与运行时 运行时值 / 智语言_堆操作 / 智语言_系统调用 同一套语言。
pub struct 编译器 {
    pub 函数构建上下文: FunctionBuilderContext,
    /// 局部索引 → (标签变量, 载荷变量)。惰性声明，彻底修复非参数局部未声明问题。
    pub 变量映射: HashMap<usize, (Variable, Variable)>,
    /// 惰性声明局部时使用的下一个 Cranelift Variable 序号（从形参数目之后开始，避免与形参冲突）。
    pub 下一变量序号: usize,
    pub 基本块映射: HashMap<usize, cranelift_codegen::ir::Block>,
    pub 函数名映射: HashMap<usize, FuncId>,
    pub 入口函数标识: Option<FuncId>,
    pub 优化器: Option<优化管线器>,
    pub 当前实参列表: Vec<cranelift_codegen::ir::Value>,
    /// 智语言_系统调用 FuncId 缓存（避免跨函数重复 declare 签名冲突）
    pub 系统调用函数标识: Option<FuncId>,
    /// 智语言_注册外部函数 FuncId 缓存（FFI prologue 注册用）
    pub 注册外部函数标识: Option<FuncId>,
    /// 智语言_注册包装调度函数 FuncId 缓存（FFI prologue 设置 shim 分发用）
    pub 注册包装调度函数标识: Option<FuncId>,
    /// 🆕 G1 智语言_注册外部结构体布局 FuncId 缓存（外部结构体布局旁路注册）
    pub 注册外部结构体布局标识: Option<FuncId>,
    /// 🆕 G1 智语言_设置FFI结构体名 FuncId 缓存（为已注册外部函数补结构体名）
    pub 设置FFI结构体名标识: Option<FuncId>,
    /// 🆕 FFI（§6.5.2 第5项）智语言_设置FFI全局变量标记 FuncId 缓存（为已注册外部函数标记 C 全局变量）
    pub 设置FFI全局变量标记标识: Option<FuncId>,
    /// 🆕 2026-08-29 FFI 数组：智语言_设置FFI数组元数据 FuncId 缓存（为已注册外部函数补数组元数据）
    pub 设置FFI数组元数据标识: Option<FuncId>,
    /// 🆕 智语言_注册回调提供者 FuncId 缓存（FFI prologue 注册 shim 回调 trampoline 提供者）
    pub 注册回调提供者标识: Option<FuncId>,
    /// 🆕 智语言_设置回调调度 FuncId 缓存（FFI prologue 设置 shim 回调系统调用）
    pub 设置回调调度标识: Option<FuncId>,
    /// 🆕 智语言_注册用户函数调用器 FuncId 缓存（FFI prologue 登记编译模式回调桥 thunk）
    pub 注册用户函数调用器标识: Option<FuncId>,
    /// 🆕 回调桥 thunk FuncId（编译模式 FFI 回调的「按索引调用」桥，仅在存在回调候选时生成）
    pub 回调桥函数标识: Option<FuncId>,
    /// 🆕 回调候选函数索引集合（被 FFI 函数指针实参引用的智语言用户函数，≥200），
    /// 由预遍扫描收集，供生成「按索引调用」回调桥 thunk 分派。
    pub 回调候选函数集: Vec<usize>,
    /// 入口函数索引(200+i)，用于仅在入口函数 prologue 注入 FFI 注册调用
    pub 入口函数索引: Option<usize>,
    /// 当前正在编译的函数索引（运行时异常处理器栈以「函数索引」标记每条记录，实现跨函数正确传播）
    pub 当前函数索引: Option<usize>,
    pub 常量池字节: HashMap<usize, Vec<u8>>, // 常量池索引 → 实际字节
    // DWARF调试信息
    pub 调试信息: 调试信息生成器,
    pub 当前函数调试信息: Vec<(u32, u32, u32)>, // (指令偏移, 行号, 列号)
    /// 函数索引(200+i) → 返回类型（任务 thunk 结果打包约束用）
    pub 函数返回类型: HashMap<usize, frontend::底层表示::类型定义::底层表示类型>,
    /// ⚡双模式⚡ 函数索引(200+i) → 真实形参数（C1 函数一等公民间接调用 dispatch 按真实参数数生成实参，避免参数数不匹配）
    pub 函数参数数映射: HashMap<usize, usize>,
    /// 任务体函数索引 → 生成的 thunk FuncId（缓存，避免重复生成）
    pub thunk映射: HashMap<usize, FuncId>,
    /// HTTP 处理器函数索引集合（由 CALL 147 处理器实参预遍收集）：
    /// 处理器源码声明返回 空值，但运行时经 sret 返回 网络响应（对齐解释器 调用函数带参数 返回结构），
    /// 编译模式须强制前置 sret 指针，否则 thunk 调用参数数不匹配（got 3, expected 2）。
    pub http处理器集合: HashSet<usize>,
    /// 当前正在翻译的基本块标识符（用于在块入口检测 catch 块）。
    pub 当前块标识符: usize,
    /// 🆕 方案B S3：当前基本块的静态异常目标（前端 S1 标注）。注入异常检查据此直跳，
    /// 替代历史 base 运行时异常处理器栈 + 智语言_派发异常 状态机。
    pub 当前异常目标: 异常目标,
    /// 当前函数的局部变量类型表（Vec索引对应局部变量索引，值=底层表示类型）。
    /// 供 取操作数类型与载荷 在Rust层面判断局部变量是整数还是小数。
    pub 当前局部变量表: Vec<frontend::底层表示::类型定义::底层表示类型>,
    /// 当前函数的“异常提前返回块”：待处理异常未被任何 try 捕获时由此块提前返回。
    pub 异常返回块: Option<cranelift_codegen::ir::Block>,
    /// 运行时异常函数标识缓存（智语言_异常待处理 / 智语言_清除当前异常）
    pub 异常函数缓存: HashMap<String, FuncId>,
    /// 异常位置数据段唯一命名计数（编译模式经 嵌入字节 注入「路径:行:列」字符串）
    pub 异常位置计数: usize,
    /// 当前生效的异常源码位置文本（`路径:行:列`）——「最近一次已知位置」。
    ///
    /// 为什么需要它：前端多数指令的 `调试位置` 是 `None`（只有少数语句显式填），
    /// 而 base 构造异常时消费 thread-local「当前源码位置」；若可抛指令前从未设置过，
    /// `异常错误().位置` 就是**空串** ⇒ 与解释模式（回退「指令偏移 N」）分歧。
    /// 实测：`转整数("abc")` 编译模式位置为空（289 全局函数矩阵 1 条失败）。
    /// 策略：函数入口先置「模块名:0:0」兜底，遇到带位置的指令即更新；可抛指令前注入。
    pub 当前异常位置文本: Option<String>,
    // 🆕 方案B S4：catch块集合/finally块集合 已删除（注入异常检查改静态直跳，不再需要集合匹配）。
    /// 当前编译函数的 sret 指针（非空返回函数把 (标签,载荷) 写入此 16 字节栈槽后返回 void）。
    /// ⚡双模式⚡ D5 修复：Win64 下 i128/双 i64 返回 ABI 不受 Cranelift 0.110.3 支持，
    /// 改用 sret(i64 指针)——干净返回于 RAX，调用方再从栈槽读回 (标签,载荷)。
    pub 当前sret指针: Option<cranelift_codegen::ir::Value>,
    /// 🆕 统一帧契约（UFC）2026-09-22：当前函数的**影子块基址**（引用型槽镜像块；`None` = 本函数无引用型槽）。
    /// 编译模式 LIR 局部槽由 `Variable`(SSA) 承载 ⇒ 句柄可能只活在物理寄存器 ⇒ 运行时**无法枚举其根**；
    /// 影子块把引用型槽镜像到本函数栈帧内、prologue 压入**编译帧链** ⇒ 与解释模式产出**同构根集**。
    pub 当前影子块基址: Option<cranelift_codegen::ir::Value>,
    /// 🆕 局部槽索引 → 影子块内「对下标」（每对 16 字节 = (标签, 载荷)）。
    pub 当前影子槽位映射: HashMap<usize, usize>,
    /// ⚡双模式⚡ 偏差A（已废弃·方案C2）：原「全局变量(0) 内存格副本」通道已废除，重抛标志改由专用 opcode 承载——
    ///   两模均直调 base 同一函数（查询/复位/置位重抛标志），真相源唯一 = base 线程局部「待处理」；
    ///   （旧注释记录的「栈槽被 SSA 提升」顾虑，现由直调 base 外部调用天然免疫，无需内存副本）。
    ///   取全局基址() 仍保留供通用全局变量寻址（偏移 0 现为空闲普通槽）。
    /// 闭包函数索引列表（编译模式 间接调用 dispatch 用）
    pub 闭包索引列表: Vec<usize>,
    /// 每个闭包自身的捕获数（按 闭包索引 记录）。解释器按闭包数组长度取实际捕获数，
    /// 编译模式 dispatch 也须按每个闭包自身的捕获数加载，而不能用模块级统一最大值，
    /// 否则闭包数组长度与函数签名捕获参数数不一致导致 ABI 错配。
    pub 闭包捕获数映射: HashMap<usize, usize>,
    /// ⚡双模式⚡ 每个闭包自身的"真实参数数"= 闭包函数 形参列表.len() - 捕获数。
    /// 间接调用 dispatch 为每个分支按真实参数数生成实参（从当前调用 当前参数 取前 N 对、不足补 0），
    /// 解决异构签名闭包（如 增量1参/乘积2参）共用同一 当前参数 造成的 参数数不匹配。
    pub 闭包参数数映射: HashMap<usize, usize>,
    /// ⚡双模式⚡: 模块级全局变量持久区（Cranelift 静态数据区）。
    /// 每个全局变量占 16 字节（标签 i64 + 载荷 i64），索引 = 前端下发的 全局变量(gi+1)。
    /// 与解释器独立 全局变量表 语义对齐：跨函数可见、可读写。无全局变量的模块为 None。
    /// 全局数据标识 为模块级 DataId（编译模块时声明）；全局全局值 为当前函数内的 GlobalValue 句柄。
    pub 全局数据标识: Option<DataId>,
    /// 当前编译函数内对 全局数据标识 的 GlobalValue 句柄（编译函数入口处经 declare_data_in_func 生成）。
    pub 全局全局值: Option<GlobalValue>,
    /// 🆕 方案A：字符串常量静态化区（COFF 数据段）。键=常量池索引，值=(段内字节偏移, 长度)。
    /// 所有字符串字面量在编译模块第 1.2 遍拼入单一只读数据段，运行时零拷贝引用（见 智语言分配文本自静态）。
    pub 字符串常量偏移: HashMap<usize, (usize, usize)>,
    pub 字符串常量数据标识: Option<DataId>,
    pub 字符串常量全局值: Option<GlobalValue>,
    /// 闭包/匿名任务 thunk 缓存（键=任务参数数，值=闭包任务 thunk 的 FuncId）
    /// 编译模式 任务启动 遇闭包操作数时据此生成 CALL 160 参数字段。
    pub 闭包任务thunk映射: HashMap<usize, FuncId>,
}

impl 编译器 {
    pub fn 新建() -> Self {
        编译器 {
            函数构建上下文: FunctionBuilderContext::new(),
            变量映射: HashMap::new(),
            下一变量序号: 0,
            基本块映射: HashMap::new(),
            函数名映射: HashMap::new(),
            入口函数标识: None,
            优化器: Some(优化管线器::默认()),
            当前实参列表: Vec::new(),
            系统调用函数标识: None,
            注册外部函数标识: None,
            注册包装调度函数标识: None,
            注册外部结构体布局标识: None,
            设置FFI结构体名标识: None,
            设置FFI全局变量标记标识: None,
            设置FFI数组元数据标识: None,
            注册回调提供者标识: None,
            设置回调调度标识: None,
            注册用户函数调用器标识: None,
            回调桥函数标识: None,
            回调候选函数集: Vec::new(),
            入口函数索引: None,
            当前函数索引: None,
            常量池字节: HashMap::new(),
            调试信息: 调试信息生成器::新建(),
            当前函数调试信息: Vec::new(),
            函数返回类型: HashMap::new(),
            函数参数数映射: HashMap::new(),
            thunk映射: HashMap::new(),
            http处理器集合: HashSet::new(),
            当前块标识符: 0,
            当前异常目标: 异常目标::无,
            异常返回块: None,
            异常函数缓存: HashMap::new(),
            异常位置计数: 0,
            当前异常位置文本: None,
            // 🆕 方案B S4：catch块集合/finally块集合 已删除。
            当前sret指针: None,
            当前影子块基址: None,
            当前影子槽位映射: HashMap::new(),
            当前局部变量表: Vec::new(),
            闭包索引列表: Vec::new(),
            闭包捕获数映射: HashMap::new(),
            闭包参数数映射: HashMap::new(),
            闭包任务thunk映射: HashMap::new(),
            全局数据标识: None,
            全局全局值: None,
            字符串常量偏移: HashMap::new(),
            字符串常量数据标识: None,
            字符串常量全局值: None,
        }
    }

    pub fn 设置优化级别(&mut self, 级别: 优化级别) {
        self.优化器 = Some(优化管线器::新建(级别));
    }

    pub fn 编译模块(&mut self, 底层表示模块: &底层表示模块) -> Result<PathBuf, String> {
        let triple = Triple::host();
        let mut flag_builder = settings::builder();
        flag_builder.set("opt_level", "none").unwrap();
        // 非 Windows 目标生成位置无关代码：系统 cc 默认产出 PIE，非 PIC 目标文件的绝对重定位
        // 会落进只读代码段（DT_TEXTREL 警告；lld/mold 或 -z text 下直接链接失败）。
        if !cfg!(windows) {
            flag_builder.set("is_pic", "true").unwrap();
        }
        let isa = isa::lookup(triple)
            .map_err(|e| format!("不支持的架构: {}", e))?
            .finish(settings::Flags::new(flag_builder))
            .map_err(|e| format!("创建ISA失败: {}", e))?;
        let 对象构建器 = ObjectBuilder::new(
            isa,
            "智语言".to_string(),
            cranelift_module::default_libcall_names(),
        )
        .map_err(|e| format!("创建 ObjectBuilder 失败: {}", e))?;
        let mut 模块 = ObjectModule::new(对象构建器);

        // 第零遍：收集字符串常量字节（后续在栈上逐字节展开）
        self.常量池字节.clear();
        for (i, 常量) in 底层表示模块.常量池.iter().enumerate() {
            if let 常量值::文本(字符串) = 常量 {
                self.常量池字节.insert(i, 字符串.as_bytes().to_vec());
            }
        }

        // ⚡双模式⚡ 预遍：收集 HTTP 处理器函数索引（CALL 147 的处理器实参），供下方签名 sret 前置。
        // HTTP 处理器源码声明 空值 返回，但运行时经 sret 返回 网络响应（与解释器 调用函数带参数 返回结构一致），
        // 编译模式须强制前置 sret 指针，否则 thunk 调用 (sret,标签,句柄) 与 fn0(标签,句柄) 参数数不匹配。
        self.http处理器集合.clear();
        for 底层表示函数 in &底层表示模块.函数列表 {
            for 基本块 in &底层表示函数.基本块列表 {
                let 指令列表 = &基本块.指令列表;
                // 前向扫描：记录最新一次「加载 函数引用(X)」的 X；遇到 CALL 147 时，
                // 把该 X 当作 HTTP 处理器索引加入集合。
                // 此前用脆弱的 idx-2 偏移匹配，当实参含复合字面量（如选项 map）插入多条
                // 构造指令后，偏移错位导致处理器漏判 → 其签名无 sret → thunk 调 (sret,标签,句柄)
                // 与 fn(标签,句柄) 参数数不匹配（verifier: got 2, expected 3）。前向扫描彻底
                // 消除对实参发射顺序/间隔的依赖。
                let mut 最近函数引用: Option<usize> = None;
                for 指令 in 指令列表 {
                    if let 操作码::加载 = 指令.操作码 {
                        if let 操作数::函数引用(引用索引) = &指令.操作数1 {
                            最近函数引用 = Some(*引用索引);
                        }
                    } else if let 操作码::调用 = 指令.操作码 {
                        if let 操作数::函数引用(call_id) = &指令.操作数1 {
                            if *call_id == 147 {
                                if let Some(处理器索引) = 最近函数引用.take() {
                                    self.http处理器集合.insert(处理器索引);
                                }
                            }
                        }
                    }
                }
            }
        }

        // 第一遍：声明所有函数
        // 函数索引约定与前端/解释器保持一致：内置函数占 0-199，用户函数索引 = 200 + 列表序号
        self.函数名映射.clear();
        self.入口函数标识 = None;
        for (i, 底层表示函数) in 底层表示模块.函数列表.iter().enumerate() {
            // HTTP 处理器（经 CALL 147 传入 网络.启动服务）运行时必然返回一个 网络响应（非空），
            // 但智语言源码常声明为 `空值` 返回。若不修正，编译模式按 void 声明（无 sret），而所有
            // 调用点（HTTP thunk、回调桥）对非空返回一律按 sret 传参 → 参数数不匹配 + 返回值丢弃。
            // 统一在此算「覆盖后的返回类型」作为 sret 决策的【单一真相源】（与 `未推断` 返回同款
            // 占位处理）：签名、形参偏移、回调桥、HTTP thunk 全部基于它自动一致，消除在各调用点
            // 重复加 http处理器集合 特例导致漏判（此前漏掉回调桥即此 bug 的根因）。
            let 返回类型 = if self.http处理器集合.contains(&(frontend::底层表示::模块结构::用户函数索引基址 + i))
                && 底层表示函数.返回类型 == frontend::底层表示::类型定义::底层表示类型::空值
            {
                frontend::底层表示::类型定义::底层表示类型::整数64
            } else {
                底层表示函数.返回类型.clone()
            };
            let mut 签名 = Signature::new(CallConv::triple_default(&Triple::host()));
            // ⚠️⚡双模式⚡: 统一值表示 ABI —— 每个参数 = (标签,载荷) 两个 i64；
            //   返回值同理（非空返回 = (标签,载荷) 两个 i64）。
            //   此处声明必须与下方 编译函数 的签名完全一致，否则 declare_function 二次声明会因
            //   签名不兼容报错（编译模式此前因测试用例未覆盖"非空返回的自定义函数"而长期潜伏）。
            for (_, _类型) in &底层表示函数.形参列表 {
                let _ = _类型;
                签名.params.push(AbiParam::new(types::I64));
                签名.params.push(AbiParam::new(types::I64));
            }
            // ⚡双模式⚡ D5 修复：非空返回函数改用 sret 指针——在参数最前插入一个 i64 指针参数，
            // 被调用方把 (标签,载荷) 写入该 16 字节栈槽后返回 void。Win64 下 i128/双 i64 返回 ABI
            // 不受 Cranelift 0.110.3 支持，sret(i64 指针) 干净返回于 RAX，调用方再读回 (标签,载荷)。
            // 此处仅看「覆盖后的返回类型」单一真相源，不再依赖 http处理器集合 特例。
            if 返回类型 != frontend::底层表示::类型定义::底层表示类型::空值
            {
                签名.params.insert(0, AbiParam::new(types::I64));
            }
            // 符号名唯一化：F12 方法重载——同名方法(不同形参)在 函数列表 中以同一
            // `结构体名.方法名` 出现多次（如 计算器.加法 ×4）。Cranelift 按符号名声明函数，
            // 同名不同签名会冲突。故无论是否为匿名函数，均按 (名称, 索引) 生成唯一符号名；
            // 调用点经 函数名映射[索引] 的 FuncId 引用，与符号名无关，故不影响调用正确性。
            let 符号名 = format!("{}_{}", 底层表示函数.名称, frontend::底层表示::模块结构::用户函数索引基址 + i);
            let 函数标识 = 模块
                .declare_function(&符号名, Linkage::Export, &签名)
                .map_err(|e| format!("声明函数失败: {}", e))?;
            self.函数名映射.insert(frontend::底层表示::模块结构::用户函数索引基址 + i, 函数标识);
            self.函数返回类型.insert(frontend::底层表示::模块结构::用户函数索引基址 + i, 返回类型);
            self.函数参数数映射.insert(frontend::底层表示::模块结构::用户函数索引基址 + i, 底层表示函数.形参列表.len());
            if 底层表示模块.入口函数索引 == Some(i) {
                self.入口函数标识 = Some(函数标识);
                self.入口函数索引 = Some(frontend::底层表示::模块结构::用户函数索引基址 + i);
            }
        }

        // 第 1.2 遍：声明模块级全局变量持久区（Cranelift 静态数据区）。
        // 每个全局 = 16 字节（标签 i64 + 载荷 i64），索引 = 前端下发的 全局变量(gi+1)。
        // gi 范围 0..全局数量-1 → 实际索引 1..=全局数量；索引空间另为 全局变量(0) 预留一个槽，
        // 故区域大小 = (全局数量 + 1) * 16。与解释器 全局变量表 语义对齐（跨函数可见、可读写）。
        self.全局数据标识 = None;
        self.全局全局值 = None;
        // ⚡双模式⚡（方案C2 后现状）：模块级全局变量区【始终声明】——最低 16 字节（全局变量(0) 槽）
        // 现为**空闲普通槽**：原「异常重抛标志」通道已废除（异常状态改由专用 opcode 直调 base，
        // 见本文件 偏差A 字段注释）；用户全局从偏移 16 起。
        // 此前仅在含用户全局时才声明，导致「取全局基址越界 panic」；
        // 现改为始终声明（哪怕仅 16 字节零初始化静态，开销可忽略；对齐解释器每程序一份 全局变量表）。
        {
            let 全局数量 = 底层表示模块.全局变量列表.len();
            let 数据标识 = 模块
                .declare_data("智语言全局变量区", Linkage::Export, true, false)
                .map_err(|e| format!("声明全局数据区失败: {}", e))?;
            let mut 数据描述 = cranelift_module::DataDescription::new();
            数据描述.init = cranelift_module::Init::Zeros { size: (全局数量 + 1) * 16 };
            模块
                .define_data(数据标识, &数据描述)
                .map_err(|e| format!("定义全局数据区失败: {}", e))?;
            self.全局数据标识 = Some(数据标识);
        }

        // 第 1.3 遍：定义字符串常量静态区（COFF 只读数据段）。
        // 🆕 方案A：所有字符串字面量拼入单一数据段，运行时零拷贝引用（智语言分配文本自静态）。
        // 替代此前「每次函数调用运行时 alloc + 逐字节 store」的低效路径，消除编译模式热路径重复分配。
        self.字符串常量偏移.clear();
        self.字符串常量数据标识 = None;
        if !self.常量池字节.is_empty() {
            let mut 拼装: Vec<u8> = Vec::new();
            let 索引列表: Vec<usize> = {
                let mut v: Vec<usize> = self.常量池字节.keys().cloned().collect();
                v.sort();
                v
            };
            for 索引 in 索引列表 {
                let 字节 = self.常量池字节.get(&索引).cloned().unwrap_or_default();
                // 4 字节对齐，避免未对齐读取（静态段按 u8 拼接，运行时按指针读，需对齐偶数字节）
                let 偏移 = 拼装.len();
                self.字符串常量偏移.insert(索引, (偏移, 字节.len()));
                拼装.extend_from_slice(&字节);
                // 填充至偶数字节边界
                while 拼装.len() % 2 != 0 {
                    拼装.push(0);
                }
            }
            let 数据标识 = 模块
                .declare_data("智语言字符串常量区", Linkage::Local, false, false)
                .map_err(|e| format!("声明字符串常量区失败: {}", e))?;
            let mut 数据描述 = cranelift_module::DataDescription::new();
            // 按字节填充常量区（Init::Bytes 即只读 .rdata 段）
            let 常量数据: Box<[u8]> = 拼装.clone().into_boxed_slice();
            数据描述.init = cranelift_module::Init::Bytes { contents: 常量数据 };
            模块
                .define_data(数据标识, &数据描述)
                .map_err(|e| format!("定义字符串常量区失败: {}", e))?;
            self.字符串常量数据标识 = Some(数据标识);
        }

        // 第 1.5 遍：为 任务启动 引用的命名任务体生成 thunk（编译模式任务桥接）
        // 仅处理「命名函数 + 无参 + 返回基本类型」任务；匿名/带参/复合返回由后续 Stage 处理。
        for 底层表示函数 in &底层表示模块.函数列表 {
            for 基本块 in &底层表示函数.基本块列表 {
                for 指令 in &基本块.指令列表 {
                    match 指令.操作码 {
                        操作码::任务启动 => {
                            if let 操作数::函数引用(索引) = &指令.操作数1 {
                                if !self.thunk映射.contains_key(索引) {
                                    let 返回类型 = self.函数返回类型.get(索引).cloned()
                                        .unwrap_or(frontend::底层表示::类型定义::底层表示类型::空值);
                                    let thunk = self.生成任务thunk(&mut 模块, *索引, &返回类型)?;
                                    self.thunk映射.insert(*索引, thunk);
                                }
                            }
                        }
                        _ => {}
                    }
                }
            }
        }

        // 第 1.5b 遍：扫描 创建闭包 指令，收集闭包函数索引和各自捕获数（编译模式 dispatch 用）。
        // 捕获数直接取 创建闭包 指令的 操作数2（降级层在 语句转换 已精确写入 候选捕获.len()），
        // 不再靠"数连续 加载(无)"推算——后者会被前一条调用的参数/常量 加载(无) 污染，
        // 使 闭包捕获数映射 与函数签名（捕获列表.len()）不一致 → 闭包编译模式参数计数错位。
        for 底层表示函数 in &底层表示模块.函数列表 {
            for 基本块 in &底层表示函数.基本块列表 {
                for 指令 in &基本块.指令列表 {
                    if matches!(指令.操作码, 操作码::创建闭包) {
                        if let 操作数::函数引用(索引) = &指令.操作数1 {
                            if !self.闭包索引列表.contains(索引) {
                                self.闭包索引列表.push(*索引);
                            }
                            let 捕获数 = match &指令.操作数2 {
                                Some(操作数::立即整数(n)) if *n >= 0 => *n as usize,
                                _ => 0,
                            };
                            self.闭包捕获数映射.insert(*索引, 捕获数);
                            // 真实参数数 = 闭包函数 形参列表.len() - 捕获数（捕获已前置到形参列表）
                            let 闭包函数位置 = (*索引 as usize).saturating_sub(frontend::底层表示::模块结构::用户函数索引基址);
                            let 真实参数数 = 底层表示模块.函数列表.get(闭包函数位置)
                                .map(|f| f.形参列表.len().saturating_sub(捕获数))
                                .unwrap_or(0);
                            self.闭包参数数映射.insert(*索引, 真实参数数);
                        }
                    }
                }
            }
        }

        // 第 1.5c 遍：为 任务启动 引用闭包/匿名函数时生成闭包任务 thunk
        // 按任务参数数（0=无参, 1=1个参数, ...）生成对应的闭包 dispatch thunk。
        for 底层表示函数 in &底层表示模块.函数列表 {
            for 基本块 in &底层表示函数.基本块列表 {
                for 指令 in &基本块.指令列表 {
                    if matches!(指令.操作码, 操作码::任务启动)
                        && !matches!(&指令.操作数1, 操作数::函数引用(_))
                    {
                        let 参数数 = if 指令.操作数2.is_some() { 1 } else { 0 };
                        if !self.闭包任务thunk映射.contains_key(&参数数) {
                            let thunk = self.生成任务thunk闭包(&mut 模块, 参数数)?;
                            self.闭包任务thunk映射.insert(参数数, thunk);
                        }
                    }
                }
            }
        }

        // 第 1.6 遍：HTTP 处理器 thunk 生成（2026-08-19 阶段二已删除）
        // HTTP 处理器统一经 base 执行后台用户函数 调度，编译模式回退到 回调桥
        // （回调候选集播种 http处理器集合，见 第1.6b 遍末尾），不再生成 HTTP 专用 thunk。

        // 第 1.5d 遍：导出块 — 为 导出函数 生成**规范 §6.5.3 的真实 C 签名** thunk
        // （仅当存在导出函数时执行，对普通程序零影响）。
        // 形参/返回类型取自 函数列表（与 生成导出头文件 同一来源），保证头文件与二进制一致。
        for (导出名, c符号) in 底层表示模块.导出函数列表.iter() {
            if let Some(索引) = 底层表示模块.函数列表.iter().position(|f| &f.名称 == 导出名) {
                let 函数 = &底层表示模块.函数列表[索引];
                let 形参类型: Vec<_> = 函数.形参列表.iter().map(|(_, t)| t.clone()).collect();
                let 声明返回类型 = 函数.返回类型.clone();
                self.生成导出thunk(
                    &mut 模块,
                    frontend::底层表示::模块结构::用户函数索引基址 + 索引,
                    &形参类型,
                    &声明返回类型,
                    c符号,
                )?;
            }
        }

        // 第 1.6b 遍：FFI 回调候选扫描 —— 收集被 FFI 函数指针形参引用的智语言用户函数索引（≥200）。
        // 这些函数将作为 C 回调目标；编译模式需为其生成「按索引调用」桥 thunk 并登记为用户函数调用器。
        // 扫描策略：遍历全部 加载 指令，凡 操作数1 为 操作数::函数引用(idx≥200) 且作为 调用/外部调用 的
        // 实参压栈（紧邻调用指令之前回溯到的 加载），即候选。
        // 🆕 2026-08-10: 从「仅外部调用」放宽到「调用+外部调用」——库封装层（普通函数）把函数名
        // 作为回调实参透传给外部块函数（如 事件.连接信号(按钮, clicked, 我的回调) 先调用普通封装
        // 函数、内部再透传给 外部_连接信号），这类函数引用作为普通函数实参也必须进入候选集，
        // 否则编译模式回调桥 thunk 不生成、FFI 回调失败（c_handler=NULL）。
        // HTTP/task 的函数引用走各自 thunk，不会在此出现 标签_函数指针，故无害。
        for 底层表示函数 in &底层表示模块.函数列表 {
            for 基本块 in &底层表示函数.基本块列表 {
                let 指令列表 = &基本块.指令列表;
                for idx in 0..指令列表.len() {
                    if matches!(指令列表[idx].操作码, 操作码::调用 | 操作码::外部调用) {
                        // 回溯紧邻的 加载 指令（实参压栈），找 函数引用 操作数
                        for j in (0..idx).rev() {
                            if !matches!(指令列表[j].操作码, 操作码::加载) {
                                break;
                            }
                            if let 操作数::函数引用(函数索引) = &指令列表[j].操作数1 {
                                if *函数索引 >= frontend::底层表示::模块结构::用户函数索引基址 && !self.回调候选函数集.contains(函数索引) {
                                    self.回调候选函数集.push(*函数索引);
                                }
                            }
                        }
                    }
                }
            }
        }

        // 🆕 2026-08-19 阶段二：HTTP 处理器并入回调候选集——147 统一经 base 执行后台用户函数
        // 调度，编译模式回退到 回调桥（用户函数调用器）。处理器须在候选集内，回调桥才能按索引
        // 分派；其 sret 决策读 函数返回类型（第1遍已将 http处理器集合 覆盖为整数64）天然正确。
        for &处理器索引 in &self.http处理器集合 {
            if !self.回调候选函数集.contains(&处理器索引) {
                self.回调候选函数集.push(处理器索引);
            }
        }
        // 第 1.6c 遍：若存在 FFI 回调候选，生成「按索引调用」回调桥 thunk（编译模式用户函数调用器）。
        // 必须在 第二遍 之前定义（func_addr 在入口 prologue 引用），对无回调程序零影响。
        if !self.回调候选函数集.is_empty() {
            let 桥标识 = self.生成回调桥(&mut 模块, 底层表示模块)?;
            self.回调桥函数标识 = Some(桥标识);
        }

        // 第二遍：编译每个函数体
        for (i, 底层表示函数) in 底层表示模块.函数列表.iter().enumerate() {
            self.当前函数索引 = Some(frontend::底层表示::模块结构::用户函数索引基址 + i);
            if let Err(e) = self.编译函数(&mut 模块, 底层表示函数, frontend::底层表示::模块结构::用户函数索引基址 + i, 底层表示模块) {
                return Err(format!("编译函数【{}】失败: {}", 底层表示函数.名称, e));
            }
        }
        self.当前函数索引 = None;

        // 声明入口
        if let Some(入口索引) = 底层表示模块.入口函数索引 {
            if 入口索引 < 底层表示模块.函数列表.len() {
                let 入口名 = &底层表示模块.函数列表[入口索引].名称.clone();
                let 入口返回类型 = &底层表示模块.函数列表[入口索引].返回类型;
                self.声明入口(&mut 模块, 入口名, 入口返回类型)?;
            }
        }


        let 产物 = 模块.finish();
        let mut 对象数据 = 产物.emit().map_err(|e| format!("生成对象文件失败: {}", e))?;

        // 追加DWARF调试信息到目标文件
        if !self.调试信息.函数调试信息.is_empty() {
            let dwarf数据 = self.调试信息.生成DWARF数据();
            对象数据.extend_from_slice(b"\0DWARF\0");
            对象数据.extend_from_slice(&dwarf数据);
        }

        // 🆕 唯一临时对象文件：进程号 + 单调序号，避免所有编译共用固定名「智语言_output.o」
        // 造成顺序编译串台（前次对象文件被后续链接误用 → exe 跑出错误源码的输出）。
        let 序号 = 对象文件计数器.fetch_add(1, Ordering::SeqCst);
        let 对象路径 = std::env::temp_dir().join(format!("智语言_output_{}_{}.o", std::process::id(), 序号));
        std::fs::write(&对象路径, 对象数据)
            .map_err(|e| format!("写入对象文件失败: {}", e))?;
        Ok(对象路径)
    }

    fn 声明入口(
        &mut self,
        模块: &mut ObjectModule,
        _入口名: &str,
        入口返回类型: &frontend::底层表示::类型定义::底层表示类型,
    ) -> Result<(), String> {
        let mut 签名 = Signature::new(CallConv::triple_default(&Triple::host()));
        签名.returns.push(AbiParam::new(types::I32));

        let main标识 = 模块
            .declare_function("main", Linkage::Export, &签名)
            .map_err(|e| format!("声明 main 失败: {}", e))?;

        let mut 上下文 = Context::new();
        // 设置函数签名为声明的签名
        上下文.func.signature = 签名.clone();
        let mut 构建上下文 = std::mem::take(&mut self.函数构建上下文);
        let mut 构建器 = FunctionBuilder::new(&mut 上下文.func, &mut 构建上下文);
        let 入口块 = 构建器.create_block();
        构建器.switch_to_block(入口块);

        let 启动签名 = Signature::new(CallConv::triple_default(&Triple::host()));
        let 启动函数 = 模块
            .declare_function("智语言启动", Linkage::Import, &启动签名)
            .map_err(|e| format!("声明运行时启动失败: {}", e))?;

        let 调用 = 模块.declare_func_in_func(启动函数, &mut 构建器.func);
        构建器.ins().call(调用, &[]);

        if let Some(入口标识) = self.入口函数标识 {
            // D5 修复：非空返回入口经 sret 传入 16 字节栈槽地址；void 入口直接无参调用。
            let 入口实参: Vec<cranelift_codegen::ir::Value> = if *入口返回类型
                != frontend::底层表示::类型定义::底层表示类型::空值
            {
                let sret槽 = 构建器.create_sized_stack_slot(StackSlotData::new(StackSlotKind::ExplicitSlot, 16, 4));
                vec![构建器.ins().stack_addr(types::I64, sret槽, 0)]
            } else {
                vec![]
            };
            let 入口调用 = 模块.declare_func_in_func(入口标识, &mut 构建器.func);
            构建器.ins().call(入口调用, &入口实参);

            // 顶层未捕获异常检查：与解释器 运行程序 顶层 打印未捕获异常 语义对齐。
            // 启动程序 抛出的未捕获异常经 异常返回块 提前返回并置 待处理 标志；
            // 此处检测并在退出前打印（匹配解释器格式），进程以非零退出，实现双模统一。
            let 待处理函数 = self.取异常函数(模块, "智语言_异常待处理")?;
            let 待处理声明 = 模块.declare_func_in_func(待处理函数, &mut 构建器.func);
            let 待处理调用 = 构建器.ins().call(待处理声明, &[]);
            let 待处理标志 = 构建器.inst_results(待处理调用)[0];

            let 异常块 = 构建器.create_block();
            let 正常块 = 构建器.create_block();
            构建器.ins().brif(待处理标志, 异常块, &[], 正常块, &[]);

            构建器.switch_to_block(异常块);
            构建器.seal_block(异常块);
            let 打印函数 = self.取异常函数(模块, "智语言_打印当前异常")?;
            let 打印声明 = 模块.declare_func_in_func(打印函数, &mut 构建器.func);
            构建器.ins().call(打印声明, &[]);
            let 退出码 = 构建器.ins().iconst(types::I32, 1);
            构建器.ins().return_(&[退出码]);

            构建器.switch_to_block(正常块);
            构建器.seal_block(正常块);
        }

        let 返回值 = 构建器.ins().iconst(types::I32, 0);
        构建器.ins().return_(&[返回值]);
        构建器.seal_all_blocks();
        构建器.finalize();
        self.函数构建上下文 = 构建上下文;

        模块
            .define_function(main标识, &mut 上下文)
            .map_err(|e| format!("定义 main 失败: {:#?}", e))?;
        Ok(())
    }
}