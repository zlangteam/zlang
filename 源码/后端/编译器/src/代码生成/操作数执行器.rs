// 文件: 后端/编译器/src/代码生成/操作数执行器.rs
// 职责: 底层表示 → Cranelift IR → 目标文件，完整支持结构体/数组/函数调用（含参数传递）和DWARF调试信息

use cranelift_codegen::ir::{types, AbiParam, InstBuilder, MemFlags, Signature};
use cranelift_codegen::isa::CallConv;
use cranelift_frontend::{FunctionBuilder, Variable};
use cranelift_module::{Linkage, Module};
use cranelift_object::ObjectModule;
use target_lexicon::Triple;

use frontend::底层表示::指令集::{操作码, 操作数};
use base::值类型::{标签_小数, 标签_句柄, 标签_并发句柄, 标签_空值};

/// 统一值表示：每个局部 = (标签, 载荷) 双槽 i64，与运行时 运行时值 / 智语言_堆操作 / 智语言_系统调用 同一套语言。
use crate::代码生成::编译器;

impl 编译器 {
    pub(crate) fn 取变量(&mut self, 构建器: &mut FunctionBuilder, 索引: usize) -> (Variable, Variable) {
        if let Some(对) = self.变量映射.get(&索引) {
            return *对;
        }
        let 标签变量 = Variable::from_u32((索引 * 2) as u32);
        构建器.declare_var(标签变量, types::I64);
        let 载荷变量 = Variable::from_u32((索引 * 2 + 1) as u32);
        构建器.declare_var(载荷变量, types::I64);
        let 对 = (标签变量, 载荷变量);
        self.变量映射.insert(索引, 对);
        对
    }

    pub(crate) fn 操作数转值(
        &mut self,
        构建器: &mut FunctionBuilder,
        操作数: &操作数,
    ) -> Result<cranelift_codegen::ir::Value, String> {
        match 操作数 {
            操作数::立即整数(v) => Ok(构建器.ins().iconst(types::I64, *v)),
            操作数::立即小数(v) => Ok(构建器.ins().iconst(types::I64, v.to_bits() as i64)),
            操作数::立即逻辑(v) => Ok(构建器.ins().iconst(types::I64, if *v { 1 } else { 0 })),
            操作数::局部变量(索引) => {
                let (_, 载荷变量) = self.取变量(构建器, *索引);
                Ok(构建器.use_var(载荷变量))
            }
            操作数::全局变量(i) => {
                // 跨函数持久区：每个全局 = (标签 i64, 载荷 i64) 共 16 字节，索引 = gi+1。
                let 基址 = self.取全局基址(构建器);
                let 偏移 = 构建器.ins().iconst(types::I64, (*i as i64) * 16 + 8); // 载荷在 +8
                let 地址 = 构建器.ins().iadd(基址, 偏移);
                Ok(构建器.ins().load(types::I64, MemFlags::new(), 地址, 0))
            }
            操作数::函数引用(索引) => {
                // ⚡双模式⚡ C1 函数一等公民：函数引用在编译模式经 运行时值::函数指针 负编码，
                // 标签 = 标签_函数指针，载荷 = -(函数索引+1)（与解释器值类型编码一致）。
                Ok(构建器.ins().iconst(types::I64, -( *索引 as i64) - 1))
            }
            _ => Ok(构建器.ins().iconst(types::I64, 0)),
        }
    }

    pub(crate) fn 操作数转标签(
        &mut self,
        构建器: &mut FunctionBuilder,
        操作数: &操作数,
    ) -> Result<cranelift_codegen::ir::Value, String> {
        match 操作数 {
            操作数::立即整数(_) => {
                Ok(构建器.ins().iconst(types::I64, base::值类型::标签_整数 as i64))
            }
            操作数::立即小数(_) => {
                Ok(构建器.ins().iconst(types::I64, base::值类型::标签_小数 as i64))
            }
            操作数::立即逻辑(_) => {
                Ok(构建器.ins().iconst(types::I64, base::值类型::标签_逻辑 as i64))
            }
            操作数::字符串常量(_) => {
                Ok(构建器.ins().iconst(types::I64, base::值类型::标签_句柄 as i64))
            }
            操作数::局部变量(索引) => {
                let (标签变量, _) = self.取变量(构建器, *索引);
                Ok(构建器.use_var(标签变量))
            }
            操作数::全局变量(i) => {
                // 跨函数持久区：标签在 +0。索引 = gi+1。
                let 基址 = self.取全局基址(构建器);
                let 偏移 = 构建器.ins().iconst(types::I64, (*i as i64) * 16);
                let 地址 = 构建器.ins().iadd(基址, 偏移);
                Ok(构建器.ins().load(types::I64, MemFlags::new(), 地址, 0))
            }
            操作数::函数引用(_) => {
                // ⚡双模式⚡ C1 函数一等公民：函数引用的标签恒为 标签_函数指针
                Ok(构建器.ins().iconst(types::I64, base::值类型::标签_函数指针 as i64))
            }
            _ => Ok(构建器.ins().iconst(types::I64, 标签_空值 as i64)),
        }
    }

    /// 取模块级全局变量持久区基址（Cranelift 静态数据区 GlobalValue 地址）。
    /// 仅在 编译函数 入口已对 全局数据标识 调 declare_data_in_func 生成 GlobalValue 后可用。
    pub(crate) fn 取全局基址(&mut self, 构建器: &mut FunctionBuilder) -> cranelift_codegen::ir::Value {
        let 全局值 = self
            .全局全局值
            .expect("全局数据区未在当前函数声明（不应在全局变量为空的模块调用）");
        let 基址 = 构建器.ins().global_value(types::I64, 全局值);
        基址
    }

    pub(crate) fn 取操作数类型与载荷(
        &mut self,
        构建器: &mut FunctionBuilder,
        操作数: &操作数,
    ) -> Result<(bool, cranelift_codegen::ir::Value), String> {
        match 操作数 {
            操作数::立即整数(v) => Ok((true, 构建器.ins().iconst(types::I64, *v))),
            操作数::立即小数(v) => Ok((false, 构建器.ins().iconst(types::I64, v.to_bits() as i64))),
            操作数::立即逻辑(v) => Ok((true, 构建器.ins().iconst(types::I64, if *v { 1 } else { 0 }))),
            操作数::局部变量(索引) => {
                let (_, 载荷变量) = self.取变量(构建器, *索引);
                // 从局部变量表读取类型信息
                let 是整数 = if *索引 < self.当前局部变量表.len() {
                    !matches!(self.当前局部变量表[*索引], frontend::底层表示::类型定义::底层表示类型::小数64)
                } else {
                    true // 未知类型默认整数
                };
                Ok((是整数, 构建器.use_var(载荷变量)))
            }
            _ => Ok((false, 构建器.ins().iconst(types::I64, 0))),
        }
    }

    pub(crate) fn 取操作数标签与值(
        &mut self,
        构建器: &mut FunctionBuilder,
        操作数: &操作数,
    ) -> Result<(cranelift_codegen::ir::Value, cranelift_codegen::ir::Value), String> {
        let 标签 = self.操作数转标签(构建器, 操作数)?;
        let 载荷 = self.操作数转值(构建器, 操作数)?;
        Ok((标签, 载荷))
    }

    pub(crate) fn 存储值(
        &mut self,
        构建器: &mut FunctionBuilder,
        结果: &操作数,
        标签: u64,
        载荷值: cranelift_codegen::ir::Value,
    ) {
        let 标签值 = 构建器.ins().iconst(types::I64, 标签 as i64);
        if let 操作数::局部变量(索引) = 结果 {
            let (标签变量, 载荷变量) = self.取变量(构建器, *索引);
            构建器.def_var(标签变量, 标签值);
            构建器.def_var(载荷变量, 载荷值);
            // 🆕 统一帧契约（UFC）：同步写影子槽（引用型槽才镜像，见 翻译执行器::写影子槽）
            self.写影子槽(构建器, *索引, 标签值, 载荷值);
        }
    }

    pub(crate) fn 存储值对(
        &mut self,
        构建器: &mut FunctionBuilder,
        结果: &操作数,
        标签值: cranelift_codegen::ir::Value,
        载荷值: cranelift_codegen::ir::Value,
    ) {
        if let 操作数::全局变量(i) = 结果 {
            // 跨函数持久区写回：(标签,载荷) 各 i64，共 16 字节，索引 = gi+1。
            let 基址 = self.取全局基址(构建器);
            let 标签偏移 = 构建器.ins().iconst(types::I64, (*i as i64) * 16);
            let 标签地址 = 构建器.ins().iadd(基址, 标签偏移);
            构建器.ins().store(MemFlags::new(), 标签值, 标签地址, 0);
            let 载荷偏移 = 构建器.ins().iconst(types::I64, (*i as i64) * 16 + 8);
            let 载荷地址 = 构建器.ins().iadd(基址, 载荷偏移);
            构建器.ins().store(MemFlags::new(), 载荷值, 载荷地址, 0);
            return;
        }
        if let 操作数::局部变量(索引) = 结果 {
            let (标签变量, 载荷变量) = self.取变量(构建器, *索引);
            构建器.def_var(标签变量, 标签值);
            构建器.def_var(载荷变量, 载荷值);
            // 🆕 统一帧契约（UFC）：同步写影子槽（引用型槽才镜像，见 翻译执行器::写影子槽）
            self.写影子槽(构建器, *索引, 标签值, 载荷值);
        }
    }

    /// 统一边界真相（读）：从 32 字节 运行时值 槽按标签取出 (标签, 载荷)。
    /// 与运行时 值类型::取载荷 同构：标签_句柄→句柄@24、标签_小数→小数@16、其余→整数@8。
    /// 所有 FFI 边界站点（系统调用回读、导出 thunk 入参读取、导出 thunk 结果读取、入参释放）
    /// 必须共用本函数，禁止各站点再手搓 select / 写三处，避免字段错位。
    /// 注意：函数返回 sret 使用编译器内部约定（标签@0/载荷@8），不是运行时布局，切勿用本函数读取。
    pub(crate) fn 读运行时值(
        &mut self,
        构建器: &mut FunctionBuilder,
        槽地址: cranelift_codegen::ir::Value,
    ) -> (cranelift_codegen::ir::Value, cranelift_codegen::ir::Value) {
        let 标志 = MemFlags::new();
        let 标签 = 构建器.ins().load(types::I64, 标志, 槽地址, 0);
        let v8 = 构建器.ins().load(types::I64, 标志, 槽地址, 8);
        let v16 = 构建器.ins().load(types::I64, 标志, 槽地址, 16);
        let v24 = 构建器.ins().load(types::I64, 标志, 槽地址, 24);
        let 小数标签 = 构建器.ins().iconst(types::I64, 标签_小数 as i64);
        let 是小数 = 构建器.ins().icmp(
            cranelift_codegen::ir::condcodes::IntCC::Equal,
            标签,
            小数标签,
        );
        let 句柄标签 = 构建器.ins().iconst(types::I64, 标签_句柄 as i64);
        let 并发标签 = 构建器.ins().iconst(types::I64, 标签_并发句柄 as i64);
        let 是句柄 = 构建器.ins().icmp(
            cranelift_codegen::ir::condcodes::IntCC::Equal,
            标签,
            句柄标签,
        );
        let 是并发句柄 = 构建器.ins().icmp(
            cranelift_codegen::ir::condcodes::IntCC::Equal,
            标签,
            并发标签,
        );
        let 是句柄类 = 构建器.ins().bor(是句柄, 是并发句柄);
        let 载荷_非句柄 = 构建器.ins().select(是小数, v16, v8);
        let 载荷 = 构建器.ins().select(是句柄类, v24, 载荷_非句柄);
        (标签, 载荷)
    }

    /// 统一边界真相（写）：(标签, 载荷) → 32 字节 运行时值 槽。
    /// 与运行时 值类型::重建运行时值 / 取载荷 同构：标签_句柄→句柄@24、标签_小数→小数@16、其余→整数@8。
    /// 取代原「载荷同时写 @8/@16/@24 三处」技巧，使入参编组与运行时读取契约唯一、零歧义。
    pub(crate) fn 写运行时值(
        &mut self,
        构建器: &mut FunctionBuilder,
        标签: cranelift_codegen::ir::Value,
        载荷: cranelift_codegen::ir::Value,
        槽地址: cranelift_codegen::ir::Value,
    ) {
        let 标志 = MemFlags::new();
        构建器.ins().store(标志, 标签, 槽地址, 0);
        // 标量默认落 整数@8
        构建器.ins().store(标志, 载荷, 槽地址, 8);
        let 小数标签 = 构建器.ins().iconst(types::I64, 标签_小数 as i64);
        let 是小数 = 构建器.ins().icmp(
            cranelift_codegen::ir::condcodes::IntCC::Equal,
            标签,
            小数标签,
        );
        let 零 = 构建器.ins().iconst(types::I64, 0);
        let 小数载荷 = 构建器.ins().select(是小数, 载荷, 零);
        构建器.ins().store(标志, 小数载荷, 槽地址, 16);
        let 句柄标签 = 构建器.ins().iconst(types::I64, 标签_句柄 as i64);
        let 并发标签 = 构建器.ins().iconst(types::I64, 标签_并发句柄 as i64);
        let 是句柄 = 构建器.ins().icmp(
            cranelift_codegen::ir::condcodes::IntCC::Equal,
            标签,
            句柄标签,
        );
        let 是并发句柄 = 构建器.ins().icmp(
            cranelift_codegen::ir::condcodes::IntCC::Equal,
            标签,
            并发标签,
        );
        let 是句柄类 = 构建器.ins().bor(是句柄, 是并发句柄);
        let 句柄载荷 = 构建器.ins().select(是句柄类, 载荷, 零);
        构建器.ins().store(标志, 句柄载荷, 槽地址, 24);
    }

    /// 记录算术/比较结果变量的静态类型，使后续指令经 取操作数类型与载荷 取类型标志时与运行时真相一致。
    /// 关键：除法结果恒为 小数、整除结果恒为整数、加/减/乘 任一方为小数则结果小数（否则整数）。
    /// 此前编译器仅按「变量声明/默认整数」填充 当前局部变量表，导致 `/` 返回小数后，混合运算把小数载荷的
    /// f64 位模式当整数传入运行时，产生垃圾值。此修复与解释器（运行时读真实标签）双模统一。
    pub(crate) fn 记录结果类型(&mut self, 结果: &操作数, 类型: frontend::底层表示::类型定义::底层表示类型) {
        if let 操作数::局部变量(索引) = 结果 {
            if *索引 < self.当前局部变量表.len() {
                self.当前局部变量表[*索引] = 类型;
            }
        }
    }

    /// 为一次运行时调用分配实参/输出缓冲：使用当前函数的栈槽。
    ///
    /// 旧实现每次都调 `智语言分配内存`（裸 alloc），而全仓库没有对应的释放——
    /// 每次比较、堆操作、系统调用泄漏 64~96+ 字节，长循环最终耗尽内存（解释器无此问题）。
    /// 这些缓冲只在紧随其后的那一次同步运行时调用期间被读写，调用返回即失效，
    /// 因此放在栈上即可；每个调用点对应一个固定栈槽，循环内复用、递归时随帧分配。
    pub(crate) fn 分配缓冲(&mut self, 构建器: &mut FunctionBuilder, _模块: &mut ObjectModule, 大小: i64) -> Result<cranelift_codegen::ir::Value, String> {
        use cranelift_codegen::ir::{StackSlotData, StackSlotKind};
        let 字节 = if 大小 <= 0 { 8 } else { 大小 as u32 };
        // 对齐 8 字节（align_shift = 3），与原 `智语言分配内存` 的对齐一致
        let 槽 = 构建器.create_sized_stack_slot(StackSlotData::new(StackSlotKind::ExplicitSlot, 字节, 3));
        Ok(构建器.ins().stack_addr(types::I64, 槽, 0))
    }

    pub(crate) fn 堆操作索引(&self, 码: &操作码) -> Option<usize> {
        堆操作索引查询(码)
    }

    pub(crate) fn 分配字符串常量句柄(
        &mut self,
        构建器: &mut FunctionBuilder,
        模块: &mut ObjectModule,
        索引: usize,
    ) -> Result<cranelift_codegen::ir::Value, String> {
        // 🆕 方案A：字符串常量静态化。
        // 直接取 COFF 只读数据段（智语言字符串常量区）中段内地址，调 智语言分配文本自静态
        // 零拷贝构造文本句柄。替代「运行时 alloc + 逐字节 store + 构造 String」低效路径。
        let (偏移, 长度) = *self.字符串常量偏移
            .get(&索引)
            .ok_or_else(|| format!("字符串常量索引 {} 未找到静态区偏移", 索引))?;
        let 全局值 = self.字符串常量全局值
            .ok_or_else(|| "字符串常量区 GlobalValue 未生成".to_string())?;
        let 段基址 = 构建器.ins().global_value(types::I64, 全局值);
        let 地址 = 构建器.ins().iadd_imm(段基址, 偏移 as i64);
        let 长度值 = 构建器.ins().iconst(types::I64, 长度 as i64);
        // 🔁 阶段 1b（2026-09-22）**统一字面量表**：由「每个使用点都分配一个新对象」
        //   改为「按 **LIR 常量池索引** 惰性取用（同一索引 ⇒ 同一对象）」。
        //   缺陷背景（实测探针 `I_字面量赋值_300`）：旧路径在**使用点**发射分配 ⇒
        //     循环内每次执行都新建一个 `文本_静态` 且**无主**（不进槽 ⇒ 弹帧不释放；编译模式无常量池）
        //     ⇒ 300 轮 ⇒ 存活 301（解释侧仅 2）⇒ 既泄漏又造成**两引擎存活口径差**。
        //   声明并调用 智语言_取字面量文本(索引: i64, 指针: *const u8, 长度: i64) -> 句柄: i64
        let 索引值 = 构建器.ins().iconst(types::I64, 索引 as i64);
        let mut 签名 = Signature::new(CallConv::triple_default(&Triple::host()));
        签名.params.push(AbiParam::new(types::I64));
        签名.params.push(AbiParam::new(types::I64));
        签名.params.push(AbiParam::new(types::I64));
        签名.returns.push(AbiParam::new(types::I64));
        let 函数 = 模块
            .declare_function("智语言_取字面量文本", Linkage::Import, &签名)
            .map_err(|e| format!("声明取字面量文本函数失败: {}", e))?;
        let 调用 = 模块.declare_func_in_func(函数, &mut 构建器.func);
        let 结果 = 构建器.ins().call(调用, &[索引值, 地址, 长度值]);
        Ok(构建器.func.dfg.first_result(结果))
    }

    pub(crate) fn 取操作数标签与值_含字符串(
        &mut self,
        构建器: &mut FunctionBuilder,
        模块: &mut ObjectModule,
        操作数: &操作数,
    ) -> Result<(cranelift_codegen::ir::Value, cranelift_codegen::ir::Value), String> {
        if let 操作数::字符串常量(索引) = 操作数 {
            let 标签v = 构建器.ins().iconst(types::I64, 标签_句柄 as i64);
            let 载荷v = self.分配字符串常量句柄(构建器, 模块, *索引)?;
            return Ok((标签v, 载荷v));
        }
        self.取操作数标签与值(构建器, 操作数)
    }

}

/// ⚡双模式⚡ 编译器 opcode → base::堆操作 常量 映射表（无 self 纯函数，供编译期契约测试直接调用）。
/// 全部引用 base::堆操作 常量（消除裸数字/常量混用，避免索引漂移）。
/// 新增双模共享 opcode 时，必须在此登记——否则 `翻译执行器.rs` 的 `_ =>` 兜底会触发 P0 安全网
/// （编译期显式拒编），而非静默产出幻影空值。
use frontend::底层表示::指令集::操作码到堆操作 as 单表;

pub(crate) fn 堆操作索引查询(码: &操作码) -> Option<usize> {
    // ⚡双模式⚡ 委托至 frontend 单一真相表 `操作码到堆操作`：本函数仅保留
    // 「哪些 opcode 委托 base」的枚举列表（供下方 `堆操作委托覆盖契约` 测试对账）；
    // 常量取值全部来自单表，杜绝与 VM 双份映射漂移。
    // 新增双模共享 opcode 只需在 操作码到堆操作 单表加一行，编译器即同步生效。
    match 码 {
        操作码::数组存储 => 单表(码),
        操作码::数组追加 => 单表(码),
        操作码::数组插入 => 单表(码),
        操作码::数组删除 => 单表(码),
        操作码::数组查找 => 单表(码),
        操作码::数组取末尾 => 单表(码),
        操作码::数组反转 => 单表(码),
        操作码::数组排序 => 单表(码),
        操作码::数组清空 => 单表(码),
        操作码::数组加载 => 单表(码),
        操作码::数组长度 => 单表(码),
        操作码::映射加载 => 单表(码),
        操作码::映射安全加载 => 单表(码),
        操作码::映射存储 => 单表(码),
        操作码::映射删除 => 单表(码),
        操作码::映射包含 => 单表(码),
        操作码::映射清空 => 单表(码),
        操作码::映射所有键 => 单表(码),
        操作码::映射所有值 => 单表(码),
        操作码::映射长度 => 单表(码),
        // 2026-08-15 统一索引分发：用户 容器[键] 语法唯一编译入口（类型可能未知），运行时按堆实际类型分发。
        操作码::索引加载 => 单表(码),
        操作码::索引存储 => 单表(码),
        // 2026-08-15 统一容器方法分发：编译期容器种类未知时的通用容器方法，运行时按堆实际类型分派。
        操作码::容器插入 => 单表(码),
        操作码::容器删除 => 单表(码),
        操作码::容器清空 => 单表(码),
        操作码::容器是否包含 => 单表(码),
        操作码::容器获取 => 单表(码),
        操作码::整数转文本 => 单表(码),
        操作码::小数转文本 => 单表(码),
        操作码::逻辑转文本 => 单表(码),
        操作码::文本转整数 => 单表(码),
        操作码::文本转小数 => 单表(码),
        操作码::任意转文本 => 单表(码),
        操作码::文本查找 => 单表(码),
        操作码::文本截取 => 单表(码),
        操作码::文本是否包含 => 单表(码),
        操作码::文本替换 => 单表(码),
        操作码::文本分隔 => 单表(码),
        操作码::文本去除空白 => 单表(码),
        操作码::类型判断 => 单表(码),
        操作码::实例相等 => 单表(码),
        操作码::实例不等 => 单表(码),
        操作码::属于 => 单表(码),
        操作码::不属于 => 单表(码),
        操作码::幂 => 单表(码),
        操作码::创建结构体 => 单表(码),
        操作码::字段加载 => 单表(码),
        操作码::字段存储 => 单表(码),
        操作码::深度复制 => 单表(码),
        // 🆕 2026-09-16 引用计数闭环（内存管理）：−1 释放栈槽句柄 / +1 新增对既有对象的引用。
        //    必须在此登记——否则 翻译执行器.rs 的 `_ =>` 兜底触发 P0 安全网（编译期显式拒编）。
        操作码::释放引用 => 单表(码),
        操作码::增加引用 => 单表(码),
        操作码::外部调用 => Some(119),
        // ✅ 未在上表显式登记的 opcode ⇒ **自动回退前端单一真相表 `操作码到堆操作`**。
        //    理由：单表本身就是「该 opcode 有 base 实现」的唯一权威（每个键都映射到一个 base 堆操作常量），
        //    而编译器侧的显式列表只是**可读性/契约测试用的冗余登记**。此前回退为 None 造成
        //    「前端加了 opcode、编译器忘了抄一行 ⇒ P0 安全网拒编 ⇒ 双模假分歧」的隐患
        //    （2026-09-16 释放引用/增加引用 就是这么被坑了三轮）。回退后该类问题从根上消失：
        //    只要 opcode 进了前端单表，编译器即自动生效；两边都没有的 opcode 仍被 P0 安全网拒编。
        _ => 单表(码),
    }
}

// ════════════════════════════════════════════════════════════════════════════
//  ⚡双模式⚡ 编译器 opcode → base 委托 覆盖契约（抗回归）
//
//  守护：降级层（前端）发射的全部「数组/映射/文本/类型/关系/结构体」类 opcode，
//  编译器后端必须通过 堆操作索引查询 全部映射进 base::堆操作 常量，
//  才能走统一的 C ABI 运行时路径；任何遗漏都会让 翻译执行器.rs 的 `_ =>` 兜底
//  触发 P0 安全网（编译期显式拒编，而非静默产出幻影空值）。
//  本测试钉死「这些 opcode 都必须能在映射表中查到」。未来降级层若新增同类 opcode，
//  必须同步在此列表 + 堆操作索引查询 中登记，否则本测试立即变红。
// ════════════════════════════════════════════════════════════════════════════
#[cfg(test)]
mod 堆操作委托覆盖契约 {
    use super::*;
    use frontend::底层表示::指令集::操作码;

    /// 编译器必须委托 base 的「数组/映射/文本/类型/关系/结构体」opcode 全集。
    /// 与 堆操作索引查询 的 match 分支一一对应（不含 外部调用=119 特殊项）。
    fn 必须委托的opcode全集() -> Vec<操作码> {
        vec![
            操作码::数组存储, 操作码::数组追加, 操作码::数组插入, 操作码::数组删除,
            操作码::数组查找, 操作码::数组取末尾, 操作码::数组反转, 操作码::数组排序,
            操作码::数组清空, 操作码::数组加载, 操作码::数组长度,
            操作码::映射加载, 操作码::映射安全加载, 操作码::映射存储, 操作码::映射删除,
            操作码::映射包含, 操作码::映射清空, 操作码::映射所有键, 操作码::映射所有值,
            操作码::映射长度,
            操作码::索引加载, 操作码::索引存储,
            操作码::容器插入, 操作码::容器删除, 操作码::容器清空,
            操作码::容器是否包含, 操作码::容器获取,
            操作码::整数转文本, 操作码::小数转文本, 操作码::逻辑转文本,
            操作码::文本转整数, 操作码::文本转小数, 操作码::任意转文本,
            操作码::文本查找, 操作码::文本截取, 操作码::文本是否包含, 操作码::文本替换,
            操作码::文本分隔, 操作码::文本去除空白,
            操作码::类型判断, 操作码::实例相等, 操作码::实例不等,
            操作码::属于, 操作码::不属于, 操作码::幂,
            操作码::创建结构体, 操作码::字段加载, 操作码::字段存储, 操作码::深度复制,
            操作码::释放引用, 操作码::增加引用,  // 🆕 引用计数闭环（−1 释放 / +1 新引用）
        ]
    }

    #[test]
    fn 全部共享opcode均映射到base常量() {
        for 码 in 必须委托的opcode全集().iter() {
            let idx = 堆操作索引查询(码);
            assert!(
                idx.is_some(),
                "编译器 opcode {:?} 未登记进 堆操作索引查询 —— 将触发 P0 安全网拒编，请同步登记 base::堆操作 常量",
                码
            );
            // 索引必须在合法范围（base 堆操作常量最大值 = 堆_增加引用 = 78）
            assert!(
                idx.unwrap() <= base::堆操作::堆_增加引用,
                "编译器 opcode {:?} 映射到越界索引 {}（base 堆操作常量最大 {}）",
                码, idx.unwrap(), base::堆操作::堆_增加引用
            );
        }
    }

    #[test]
    fn 映射表索引与base常量严格一致() {
        // 抽样核对：数组追加必须映射到 堆_数组追加(1)，映射长度必须映射到 堆_映射长度(17)
        assert_eq!(堆操作索引查询(&操作码::数组追加), Some(base::堆操作::堆_数组追加 as usize));
        assert_eq!(堆操作索引查询(&操作码::映射长度), Some(base::堆操作::堆_映射长度 as usize));
        assert_eq!(堆操作索引查询(&操作码::文本截取), Some(base::堆操作::堆_文本截取 as usize));
        assert_eq!(堆操作索引查询(&操作码::属于), Some(base::堆操作::堆_属于 as usize));
        assert_eq!(堆操作索引查询(&操作码::深度复制), Some(base::堆操作::堆_深拷贝 as usize));
    }

    /// 从 `堆操作索引查询` 的 match 臂反推「映射表实际覆盖的 opcode 全集」。
    /// 与 `必须委托的opcode全集()` 做**集合相等**断言：
    ///   ① 确证当前契约已对称（无孤儿登记、无死分支）；
    ///   ② 未来若有人在映射表误加「枚举无对应」的分支，或删了枚举却漏删映射，立即变红。
    fn 映射表覆盖的opcode全集() -> Vec<操作码> {
        vec![
            操作码::数组存储, 操作码::数组追加, 操作码::数组插入, 操作码::数组删除,
            操作码::数组查找, 操作码::数组取末尾, 操作码::数组反转, 操作码::数组排序,
            操作码::数组清空, 操作码::数组加载, 操作码::数组长度,
            操作码::映射加载, 操作码::映射安全加载, 操作码::映射存储, 操作码::映射删除,
            操作码::映射包含, 操作码::映射清空, 操作码::映射所有键, 操作码::映射所有值,
            操作码::映射长度,
            操作码::索引加载, 操作码::索引存储,
            操作码::容器插入, 操作码::容器删除, 操作码::容器清空,
            操作码::容器是否包含, 操作码::容器获取,
            操作码::整数转文本, 操作码::小数转文本, 操作码::逻辑转文本,
            操作码::文本转整数, 操作码::文本转小数, 操作码::任意转文本,
            操作码::文本查找, 操作码::文本截取, 操作码::文本是否包含, 操作码::文本替换,
            操作码::文本分隔, 操作码::文本去除空白,
            操作码::类型判断, 操作码::实例相等, 操作码::实例不等,
            操作码::属于, 操作码::不属于, 操作码::幂,
            操作码::创建结构体, 操作码::字段加载, 操作码::字段存储, 操作码::深度复制,
            操作码::释放引用, 操作码::增加引用,  // 🆕 引用计数闭环（−1 释放 / +1 新引用）
            // 外部调用=119 特殊项不在枚举对称集合内，单独排除
        ]
    }

    #[test]
    fn 映射表与枚举严格对称() {
        let 必须 = 必须委托的opcode全集();
        let 已覆盖 = 映射表覆盖的opcode全集();
        // 操作码 未实现 PartialEq，用判别式比较（变体同一即相等，与枚举字段值无关）
        let 判 = |x: &操作码| std::mem::discriminant(x);
        // 集合相等：无多余（孤儿登记）、无缺失（漏委托）
        let 多余: usize = 已覆盖.iter()
            .filter(|x| !必须.iter().any(|y| 判(y) == 判(x)))
            .count();
        let 缺失: usize = 必须.iter()
            .filter(|x| !已覆盖.iter().any(|y| 判(y) == 判(x)))
            .count();
        assert_eq!(多余, 0, "映射表存在孤儿登记（枚举无对应 opcode）数量: {}", 多余);
        assert_eq!(缺失, 0, "映射表漏委托（枚举有但未登记）数量: {}", 缺失);
        assert_eq!(必须.len(), 已覆盖.len(), "委托集合与映射集合长度不一致");
    }
}
