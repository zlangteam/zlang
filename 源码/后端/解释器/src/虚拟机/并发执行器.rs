#![allow(non_snake_case)]
#![allow(dead_code)]
#![allow(private_interfaces)]

use crate::虚拟机::*;
use std::cmp::Reverse;
use base::值类型::*;

// 文件: 后端/解释器/src/虚拟机/并发执行器.rs
// 统一架构：所有值操作用运行时值，不再涉及 值 enum

impl crate::虚拟机::虚拟机 {
    pub(crate) fn 执行任务启动寄存器(&mut self) {
        let _count = self.代码[self.指令指针]; self.指令指针 += 1;
        let dest = self.解析操作数();
        let task_value = self.读带标记操作数();

        // 路径1：闭包环境（闭包句柄）
        if task_value.标签 == 标签_句柄 {
            if let Some(堆数据::闭包环境 { 函数索引, 捕获变量 }) = 堆读取(task_value.句柄) {
                let func_idx = 函数索引;
                if func_idx >= frontend::底层表示::模块结构::用户函数索引基址 && func_idx < 65535 {
                    let fi = func_idx - frontend::底层表示::模块结构::用户函数索引基址;
                    if fi < self.函数表.len() {
                        let 参数: Vec<base::公共服务::拥有值> =
                            捕获变量.iter().map(|v| self.运行时值到拥有值(v)).collect();
                        let 字节码 = self.原始字节码.clone();
                        // ★ 2026-10-02（**F34 修复**）：**子 VM 继承父 VM 的全局变量表** ✗→✓
                        //   缺陷（**真双模分歧** ✗）：任务体内读「模块变量/常量」时，解释模式得 **空值** ✗
                        //     （子 VM 由 `虚拟机::新建()` 创建 ⇒ `全局变量表` 为空 ✗），
                        //     而编译模式直接访存真全局 ⇒ 正确 ✓（实测 S6：解释 空值 ✗ / 编译 101 ✓）。
                        //   修法：任务启动时把父 VM 的全局表**浅拷贝**入子 VM ✓（句柄别名 ✓）
                        //     ⇒ 子 VM 内读全局 = 与父一致 ✓。
                        //   ⚠ 记账说明：**刻意不 `堆增加引用`** ✓ —— 子 VM 生命周期内只**读**全局；
                        //     其帧释放只作用于 `局部变量表`（见 UFC 契约注释 ✓），全局槽覆盖写入亦不 −1
                        //     （`存储` 明确"GC 自动管理，无需手动释放" ✓）⇒ 别名不产生额外所有权，无泄漏/无误回收 ✓。
                        //   ⚠ 残留差异（**已知限制**，规范未定义）：任务**写**全局 ⇒ 解释模式仅改子 VM 副本
                        //     （不回写父 ✓）；编译模式是共享内存（真写 ✓）⇒ 并发写全局本属数据竞争，
                        //     规范未定义 ⇒ 按"宁可保守、绝不破坏既有语义"处理 ✓ 并登记 ✓。
                        let 父全局变量表 = self.全局变量表.clone();
                        let 任务句柄 = base::公共服务::任务句柄::启动(move || {
                            let mut 子虚拟机 = 虚拟机::新建();
                            子虚拟机.全局变量表 = 父全局变量表;
                            // 任务体的未捕获异常由任务机制以 拥有值::异常 捕获并回传给父帧的 等待结果()/批量等待()，
                            // 再由父 VM 的 捕获 块捕获；此处抑制子 VM 自身的「未捕获异常」打印，避免 stderr 误报。
                            子虚拟机.抑制异常打印 = true;
                            子虚拟机.加载字节码(&字节码);
                            let 实参: Vec<运行时值> =
                                参数.iter().map(|p| 子虚拟机.拥有值到运行时值(p)).collect();
                            // ⚠ 传 `clone()`（仅句柄浅拷贝，计数不变）—— 本任务随后要对**同一批对象**显式 −1 ✓
                            let 结果 = 子虚拟机.运行函数到拥有值(fi, 实参.clone());
                            // 🔁 2026-09-24（**D1：任务在途值记账**）★：**任务实参**由 `拥有值到运行时值`
                            //   为**本任务新建**（每个堆对象计数 = 1），而 **形参从不登记**（`函数转换.rs` 借用约定
                            //   明示"形参从不登记 ⇒ 释放形参引用本就不可能"）⇒ 任务结束**必须显式 −1** ✗
                            //   否则每个任务泄漏「捕获数」个对象（`115` 实测：闭包环境 30 → 243 → 280 ✓✓）。
                            //   安全性（三重）：① 实参对象为**本任务独占新建**（非调用方对象 ⇒ 无别名）✓
                            //     ② 任务体若把实参**存入槽/容器**，值语义路径已生成**独立副本**（深拷贝）✓
                            //     ③ 任务结果经 `运行函数到拥有值` **深提取**为 `拥有值`（不含堆句柄）✓
                            //     ⇒ 此处 −1 不伤结果、不伤调用方 ✓（与下方"常量池释放"同款收口）
                            //   ⚠ 只对 `标签_句柄` 且非 0 的实参释放（句柄 0 已由"保留 0 号槽"约定排除）✓
                            for 一个实参 in &实参 {
                                if 一个实参.标签 == 标签_句柄 && 一个实参.句柄 != 0 {
                                    base::值类型::堆释放(一个实参.句柄);
                                }
                            }
                            // 🆕 2026-09-22 内存（D1）：**任务子虚拟机退出即释放其常量池**。
                            //   原因：`加载字节码` 对**每个 VM 实例**重复 `堆分配` 文本常量（`常量池.push(句柄)`）
                            //   且从不释放 ⇒ 每启动一个任务就泄漏「常量个数」个堆对象
                            //   （`115内存并发任务通道边界` 实测：存活 6492 / 不可达 6477 ≈ 23/任务）。
                            //   安全性：子 VM 退出后常量池再无使用者；任务结果经 `拥有值到运行时值` 为**独立深拷贝** ⇒ 释放不伤结果。
                            子虚拟机.释放常量池();
                            if 子虚拟机.异常激活 {
                                return base::公共服务::拥有值::异常(
                                    子虚拟机.异常信息类型.clone(),
                                    子虚拟机.异常信息描述.clone(),
                                );
                            }
                            结果
                        });
                        let 句柄id = base::公共服务::创建任务句柄(任务句柄);
                        self.写结果(&dest, 运行时值::并发句柄(句柄id));
                        return;
                    }
                }
            }
        }

        // 路径2：函数指针（命名函数任务）
        // ⚡双模式⚡ 与编译模式 G2 命名函数任务路径一致：无参启动，返回类型经 运行函数到拥有值 通用打包任意值类型。
        // 注意：函数指针值采用负编码（整数 = -(函数索引) - 1），需经 取函数索引() 还原真实函数索引（如 201），
        // 再按 处理调用寄存器 约定减去 200 得到运行时函数表索引。
        if task_value.标签 == 标签_函数指针 {
            let func_idx = task_value.取函数索引().unwrap_or(0);
            if func_idx < 65535 {
                let fi = if func_idx >= frontend::底层表示::模块结构::用户函数索引基址 { func_idx - frontend::底层表示::模块结构::用户函数索引基址 } else { func_idx };
                if fi < self.函数表.len() {
                    let 参数: Vec<base::公共服务::拥有值> = vec![];
                    let 字节码 = self.原始字节码.clone();
                    // ★2026-10-18 补齐（与闭包任务路径 :40-43 对齐 / F34 残留半边）：
                    //   命名函数任务同样要**继承父 VM 的全局变量表**，否则任务体内读模块级变量得 `空值`
                    //   ⇒ 同一程序里「任务读模块变量」因任务形态（λ vs 具名函数）而异 ✗。
                    //   记账口径与闭包路径逐字相同：**浅拷贝、刻意不 堆增加引用**
                    //   （子 VM 生命周期内只读全局；其帧释放只作用于自身局部变量表）。
                    let 父全局变量表 = self.全局变量表.clone();
                    let 任务句柄 = base::公共服务::任务句柄::启动(move || {
                        let mut 子虚拟机 = 虚拟机::新建();
                        子虚拟机.抑制异常打印 = true;
                        子虚拟机.全局变量表 = 父全局变量表;
                        子虚拟机.加载字节码(&字节码);
                        let 实参: Vec<运行时值> =
                            参数.iter().map(|p| 子虚拟机.拥有值到运行时值(p)).collect();
                        let 结果 = 子虚拟机.运行函数到拥有值(fi, 实参);
                        // 🆕 2026-09-22 内存（D1）：命名函数任务路径同样释放子虚拟机常量池（与闭包路径一致）。
                            子虚拟机.释放常量池();
                        if 子虚拟机.异常激活 {
                            return base::公共服务::拥有值::异常(
                                子虚拟机.异常信息类型.clone(),
                                子虚拟机.异常信息描述.clone(),
                            );
                        }
                        结果
                    });
                    let 句柄id = base::公共服务::创建任务句柄(任务句柄);
                    self.写结果(&dest, 运行时值::并发句柄(句柄id));
                    return;
                }
            }
        }

        self.写结果(&dest, 运行时值::空值());
    }

    pub(crate) fn 执行任务等待寄存器(&mut self) {
        let _count = self.代码[self.指令指针]; self.指令指针 += 1;
        let dest = self.解析操作数();
        let task_val = self.读带标记操作数();

        if let Some(id) = self.提取句柄id(&task_val) {
            if let Some(句柄) = base::公共服务::取任务(id) {
                let 结果拥有值 = 句柄.等待结果();
                let 结果 = match &结果拥有值 {
                    base::公共服务::拥有值::异常(类型, 描述) => {
                        self.触发异常(类型, 描述.clone());
                        运行时值::空值()
                    }
                    _ => self.拥有值到运行时值(&结果拥有值),
                };
                // ⚠ 2026-09-22 修正（**双模分裂**）：**不得在 `等待结果` 后释放句柄**。
                //   ① 规范语义：`任务.rs` 明确「结果缓存：支持**多次** 等待结果()」，规范 §5.7.2 亦要求
                //      「超时批量等待后句柄仍有效」⇒ 句柄 + 结果缓存属**必需驻留**；
                //   ② 双模一致：编译模式（base C-ABI 路径）**不释放**句柄 ⇒ 解释模式释放会造成分裂，
                //      实测表现：`任务.等待结果()` 之后再 `并发.批量等待([任务])` ⇒ 句柄被滤空 ⇒ 返回空数组 ⇒
                //      再索引即「数组越界」（解释 FAIL / 编译 PASS，`116` 第 8 条即此断言）。
                //   ③ 内存侧：句柄表条目本身非堆槽（其驻留不产生堆泄漏），堆对象仍由引用计数正常回收。
                // base::公共服务::释放句柄(id);   // ← 已按上述裁定停用（保留本行以备追溯）
                self.写结果(&dest, 结果);
                return;
            }
        }
        self.写结果(&dest, 运行时值::空值());
    }

    pub(crate) fn 执行批量等待寄存器(&mut self) {
        let _count = self.代码[self.指令指针]; self.指令指针 += 1;
        let dest = self.解析操作数();
        let arr_val = self.读带标记操作数();

        let 句柄id列表: Vec<u64> = if arr_val.标签 == 标签_句柄 {
            if let Some(堆数据::数组(arr)) = 堆读取(arr_val.句柄) {
                arr.iter().filter_map(|v| self.提取句柄id(v)).collect()
            } else { vec![] }
        } else { vec![] };

        if !句柄id列表.is_empty() {
            let 句柄列表: Vec<base::公共服务::任务句柄> = 
                句柄id列表.iter().filter_map(|id| base::公共服务::取任务(*id)).collect();
            let 结果们 = base::公共服务::任务句柄::批量等待(&句柄列表);
            // 🆕 2026-09-22 内存（D1）：与 `执行任务等待寄存器`（`:104 释放句柄`）对齐 ——
            //   等待完成后**释放任务句柄**。此前批量等待从不释放 ⇒ 全局句柄表 + 任务结果缓存（`拥有值`）
            //   长期驻留 ⇒ 逐轮泄漏（同一用例批量等待 50 个句柄即泄漏 50 份驻留）。
            //   安全性：结果已由 `批量等待` 取出（`结果们` 为独立 `拥有值`）⇒ 移除句柄表条目不影响本次取值；
            //   `释放句柄` 对不存在的 id 幂等 ⇒ 与后续 `等待结果` 二次调用不冲突（仅句柄失效，本函数即刻返回空值）。
            // ⚠ 2026-09-22 回退：**不在此释放句柄** —— 规范 §5.7.2「超时批量等待后句柄仍有效」⇒ 内核必须保留句柄+结果缓存（必需驻留，非泄漏）。
            // ⚡双模式⚡ 规范 5.7.2：任一任务异常时批量等待抛出对应异常（与 执行任务等待寄存器 对齐，
            // 显式识别 拥有值::异常 走 VM 异常机制触发异常，而非经 base 线程局部异常静默丢失）。
            for 结果 in 结果们.iter() {
                if let base::公共服务::拥有值::异常(类型, 描述) = 结果 {
                    self.触发异常(类型, 描述.clone());
                    运行时值::空值();
                    return;
                }
            }
            let 结果数组: Vec<运行时值> = 结果们.iter().map(|v| self.拥有值到运行时值(v)).collect();
            let aid = 堆分配(堆数据::数组(结果数组));
            self.写结果(&dest, 运行时值::句柄(aid));
            return;
        }
        // ⚡双模式⚡ 空任务列表必须返回「空数组」而非 空值，依据有二：
        //   ① 与同族 执行批量等待超时寄存器 一致（该分支对空列表返回空数组）；
        //   ② 与编译模式 base 162（并发批量等待）一致——其空输入走 分配数组(vec![]) 返回空数组。
        //   此前返回 空值，造成 并发.批量等待([]) 双模行为分裂（解释=空值 / 编译=空数组），
        //   并使 并发.批量等待([]).长度() 抛「空值 类型不支持容器方法」。
        let 空数组句柄 = 堆分配(堆数据::数组(Vec::new()));
        self.写结果(&dest, 运行时值::句柄(空数组句柄));
    }

    pub(crate) fn 执行任务让出寄存器(&mut self) {
        let count = self.代码[self.指令指针]; self.指令指针 += 1;
        let dest = if count >= 1 { Some(self.解析操作数()) } else { None };
        self.任务让出_自愿();
        if let Some(d) = dest { self.写结果(&d, 运行时值::空值()); }
    }

    pub(crate) fn 执行批量等待超时寄存器(&mut self) {
        let _count = self.代码[self.指令指针]; self.指令指针 += 1;
        let dest = self.解析操作数();
        let arr_val = self.读带标记操作数();
        let timeout_val = self.读带标记操作数();

        let 超时毫秒 = if timeout_val.标签 == 标签_整数 { timeout_val.整数 as u64 } else { 0 };

        let 句柄id列表: Vec<u64> = if arr_val.标签 == 标签_句柄 {
            if let Some(堆数据::数组(arr)) = 堆读取(arr_val.句柄) {
                arr.iter().filter_map(|v| self.提取句柄id(v)).collect()
            } else { vec![] }
        } else { vec![] };

        // 超时为 0 不特判：与编译模式一致，交给 base 的 批量等待超时 做「立即查询」——
        // 已完成任务返回真实结果，未完成的返回空值。
        if 句柄id列表.is_empty() {
            let 空结果 = 堆分配(堆数据::数组(vec![运行时值::空值(); 句柄id列表.len()]));
            self.写结果(&dest, 运行时值::句柄(空结果));
            return;
        }

        let 句柄列表: Vec<base::公共服务::任务句柄> = 
            句柄id列表.iter().filter_map(|id| base::公共服务::取任务(*id)).collect();
        let 结果们 = base::公共服务::任务句柄::批量等待超时(&句柄列表, 超时毫秒);
        // ⚠ 2026-09-22 回退（**F4 根因，已修**）：**绝不在此释放句柄**。
        //   规范 §5.7.2：超时批量等待后句柄**仍有效**，稍后 `等待结果()` 必须能取到最终结果。
        //   实测教训：此处释放 ⇒ `取任务` 返回 None ⇒ `等待结果` 得空值、`是否完成` 误报真、
        //   `批量等待([句柄])` 因句柄被过滤成空列表而返回空数组（再索引即「数组越界」）——
        //   即 `总并发模块全面验证`/`详细验证` 各 1 例失败的**唯一**原因（编译模式走 base C-ABI，
        //   其同族代码早已回退 ⇒ 表现为"解释 FAIL / 编译 PASS"的双模分裂）。
        //   ⇒ 句柄 + 结果缓存属**必需驻留**；若要回收须由语言层新增显式释放句柄 API。
        // ⚡双模式⚡ 超时版批量等待同样在任一已完成任务异常时触发 VM 异常（超时任务填空值不参与判断）。
        for 结果 in 结果们.iter() {
            if let base::公共服务::拥有值::异常(类型, 描述) = 结果 {
                self.触发异常(类型, 描述.clone());
                return;
            }
        }
        let 结果数组: Vec<运行时值> = 结果们.iter().map(|v| self.拥有值到运行时值(v)).collect();
        let aid = 堆分配(堆数据::数组(结果数组));
        self.写结果(&dest, 运行时值::句柄(aid));
    }

    pub(crate) fn 执行通道创建寄存器(&mut self) {
        let _count = self.代码[self.指令指针]; self.指令指针 += 1;
        let dest = self.解析操作数();
        let capacity_val = self.读带标记操作数();
        let 容量 = if capacity_val.标签 == 标签_整数 { capacity_val.整数 as usize } else { 0 };
        let id = base::公共服务::创建通道句柄(容量);
        // 用运行时堆分配通道结构体
        let aid = 堆分配(堆数据::结构体 {
            类型索引: 通道类型索引,
            字段值: vec![运行时值::整数(id as i64)],
        });
        self.写结果(&dest, 运行时值::句柄(aid));
    }

    pub(crate) fn 执行通道发送寄存器(&mut self) {
        let _count = self.代码[self.指令指针]; self.指令指针 += 1;
        let dest_chan = self.解析操作数();
        let dest_result = self.解析操作数();
        let src_chan = self.解析操作数();
        let src_data = self.解析操作数();
        let 通道值 = self.取值(&src_chan);
        let 数据 = self.取值(&src_data);

        if let Some(id) = self.提取句柄id(&通道值) {
            if let Some(通道) = base::公共服务::取通道(id) {
                match 通道.发送(self.运行时值到拥有值(&数据)) {
                    Err(_) => self.触发异常("运行时异常", "向已关闭通道发送数据".to_string()),
                    Ok(()) => {}
                }
            }
        }
        self.写结果(&dest_chan, 通道值);
        self.写结果(&dest_result, 运行时值::空值());
    }

    pub(crate) fn 执行通道接收寄存器(&mut self) {
        let _count = self.代码[self.指令指针]; self.指令指针 += 1;
        let dest_chan = self.解析操作数();
        let dest_data = self.解析操作数();
        let src_chan = self.解析操作数();
        let _dummy = self.解析操作数();
        let 通道值 = self.取值(&src_chan);

        let recv_result = if let Some(id) = self.提取句柄id(&通道值) {
            if let Some(通道) = base::公共服务::取通道(id) {
                match 通道.接收() {
                    Some(数据) => self.拥有值到运行时值(&数据),
                    None => 运行时值::空值(),
                }
            } else { 运行时值::空值() }
        } else { 运行时值::空值() };

        self.写结果(&dest_chan, 通道值);
        self.写结果(&dest_data, recv_result);
    }

    pub(crate) fn 执行通道关闭寄存器(&mut self) {
        let _count = self.代码[self.指令指针]; self.指令指针 += 1;
        let dest = self.解析操作数();
        let chan_src = self.解析操作数();
        let 通道值 = self.取值(&chan_src);

        if let Some(id) = self.提取句柄id(&通道值) {
            if let Some(通道) = base::公共服务::取通道(id) {
                通道.关闭();
            }
            self.写结果(&dest, 通道值);
        } else {
            self.写结果(&dest, 运行时值::空值());
        }
    }

    pub(crate) fn 任务让出_自愿(&mut self) {
        let 当前 = self.当前任务标识;
        if 当前 > 0 {
            if let Some(当前任务) = self.任务表.get_mut(&当前) {
                当前任务.保存的指令指针 = self.指令指针;
                当前任务.保存的局部变量表 = self.局部变量表.clone();
                当前任务.保存的操作数栈 = self.操作数栈.clone();
                当前任务.状态 = 任务状态::就绪;
                self.任务就绪队列.push_back(当前);
            }
        } else {
            self.主任务_保存 = Some((self.指令指针, self.局部变量表.clone(), self.操作数栈.clone(), self.局部变量基址));
            self.主任务_就绪 = true;
        }
        self.调度下一任务();
    }

    pub(crate) fn 调度下一任务(&mut self) {
        self.检查定时器();
        if let Some(下一个) = self.任务就绪队列.pop_front() {
            self.加载任务(下一个);
            return;
        }
        if self.主任务_就绪 {
            self.恢复主任务_简单();
            return;
        }
        self.时间片计数器 = 1000;
    }

    pub(crate) fn 检查定时器(&mut self) {
        let 当前步数 = self.执行步数;
        let mut 过期条目: Vec<定时器条目> = vec![];
        while let Some(Reverse(条目)) = self.定时器堆.peek() {
            if 条目.截止步数 > 当前步数 { break; }
            let Reverse(条目) = self.定时器堆.pop().unwrap();
            if self.已取消定时器.contains(&条目.定时器标识) {
                self.已取消定时器.remove(&条目.定时器标识);
                continue;
            }
            过期条目.push(条目);
        }

        for 条目 in 过期条目 {
            if 条目.任务标识 == 0 {
                if let Some((ref task_list, ref dest)) = self.主程序批量等待超时 {
                    let dest = dest.clone();
                    let 任务列表 = task_list.clone();
                    let 结果数组: Vec<运行时值> = 任务列表.iter()
                        .map(|t| {
                            if self.任务已完成(*t) { self.取任务结果(*t) }
                            else { 运行时值::空值() }
                        })
                        .collect();
                    let aid = 堆分配(堆数据::数组(结果数组));
                    self.主程序批量等待超时 = None;
                    self.主程序活跃定时器标识 = None;
                    self.恢复主任务_带结果(dest, 运行时值::句柄(aid));
                }
                continue;
            }

            let 延期操作 = {
                if let Some(ref c) = self.任务表.get(&条目.任务标识) {
                    c.批量等待超时任务.as_ref().map(|(tl, d)| (条目.任务标识, tl.clone(), d.clone()))
                } else { None }
            };
            if let Some((任务标识, 任务列表, dest)) = 延期操作 {
                let 结果数组: Vec<运行时值> = 任务列表.iter()
                    .map(|t| {
                        if self.任务已完成(*t) { self.取任务结果(*t) }
                        else { 运行时值::空值() }
                    })
                    .collect();
                let aid = 堆分配(堆数据::数组(结果数组));
                if let Some(c) = self.任务表.get_mut(&任务标识) {
                    c.状态 = 任务状态::就绪;
                    c.批量等待超时任务 = None;
                    c.活跃定时器标识 = None;
                }
                self.写任务结果(任务标识, dest, 运行时值::句柄(aid));
                self.任务就绪队列.push_back(任务标识);
            }
        }
    }

    pub(crate) fn 加载任务(&mut self, id: usize) {
        if let Some(c) = self.任务表.get(&id) {
            self.指令指针 = c.保存的指令指针;
            self.局部变量表 = c.保存的局部变量表.clone();
            self.操作数栈 = c.保存的操作数栈.clone();
            self.当前任务标识 = id;
            if let Some(c) = self.任务表.get_mut(&id) { c.状态 = 任务状态::运行中; }
        }
        self.时间片计数器 = 1000;
    }

    pub(crate) fn 恢复主任务_简单(&mut self) {
        if let Some((ip, locals, stack, base)) = self.主任务_保存.take() {
            self.指令指针 = ip;
            self.局部变量表 = locals;
            self.操作数栈 = stack;
            self.局部变量基址 = base;
            self.当前任务标识 = 0;
            self.主任务_就绪 = false;
        }
        self.时间片计数器 = 1000;
    }

    pub(crate) fn 恢复主任务_带结果(&mut self, dest: 操作数源, 结果: 运行时值) {
        if let Some((ip, locals, stack, base)) = self.主任务_保存.take() {
            self.指令指针 = ip;
            self.局部变量表 = locals;
            self.操作数栈 = stack;
            self.局部变量基址 = base;
            self.当前任务标识 = 0;
            self.主任务_就绪 = false;
            self.主程序等待 = None;
            self.主程序批量等待 = None;
            self.主程序批量等待超时 = None;
            self.主程序活跃定时器标识 = None;
            self.写结果(&dest, 结果);
        }
    }

    pub(crate) fn 任务已完成(&self, 索引: usize) -> bool {
        self.任务表.get(&索引).map(|c| c.状态 == 任务状态::完成).unwrap_or(true)
    }

    pub(crate) fn 取任务结果(&mut self, 索引: usize) -> 运行时值 {
        self.任务表.get(&索引).and_then(|c| c.结果.clone()).unwrap_or(运行时值::空值())
    }

    pub(crate) fn 完成任务处理_异常(&mut self, 完成者: usize) {
        let 结果 = 运行时值::空值();
        let mut 唤醒_单个: Vec<(usize, 操作数源)> = vec![];
        let mut 唤醒_批量: Vec<(usize, 操作数源)> = vec![];
        let mut 唤醒_批量超时_待构建: Vec<(usize, 操作数源, Vec<usize>)> = vec![];
        for (&id, c) in self.任务表.iter() {
            if let Some((waiting_id, _)) = &c.等待任务 {
                if *waiting_id == 完成者 && c.状态 == 任务状态::阻塞 {
                    唤醒_单个.push((id, c.等待任务.as_ref().unwrap().1.clone()));
                }
            }
            if let Some((ref task_list, _)) = &c.批量等待任务 {
                if task_list.contains(&完成者) && task_list.iter().all(|t| self.任务已完成(*t)) {
                    唤醒_批量.push((id, c.批量等待任务.as_ref().unwrap().1.clone()));
                }
            }
            if let Some((ref task_list, ref dest)) = &c.批量等待超时任务 {
                if task_list.contains(&完成者) && task_list.iter().all(|t| self.任务已完成(*t)) {
                    唤醒_批量超时_待构建.push((id, dest.clone(), task_list.clone()));
                }
            }
        }
        let mut 唤醒_批量超时: Vec<(usize, 操作数源, 运行时值)> = vec![];
        for (id, dest, task_list) in 唤醒_批量超时_待构建 {
            let 结果数组: Vec<运行时值> = task_list.iter().map(|t| self.取任务结果(*t)).collect();
            let aid = 堆分配(堆数据::数组(结果数组));
            唤醒_批量超时.push((id, dest, 运行时值::句柄(aid)));
        }
        for (id, dest) in 唤醒_单个 {
            if let Some(c) = self.任务表.get_mut(&id) { c.状态 = 任务状态::就绪; c.等待任务 = None; }
            self.写任务结果(id, dest, 结果);
            self.任务就绪队列.push_back(id);
        }
        for (id, dest) in 唤醒_批量 {
            if let Some(c) = self.任务表.get_mut(&id) { c.状态 = 任务状态::就绪; c.批量等待任务 = None; }
            self.写任务结果(id, dest, 结果);
            self.任务就绪队列.push_back(id);
        }
        for (id, dest, 结果数组) in 唤醒_批量超时 {
            let timer_id = self.任务表.get(&id).and_then(|c| c.活跃定时器标识);
            if let Some(tid) = timer_id { self.已取消定时器.insert(tid); }
            if let Some(c) = self.任务表.get_mut(&id) { c.状态 = 任务状态::就绪; c.批量等待超时任务 = None; c.活跃定时器标识 = None; }
            self.写任务结果(id, dest, 结果数组);
            self.任务就绪队列.push_back(id);
        }
        let 唤醒主 = self.主程序等待.as_ref().map(|(t,_)| *t == 完成者).unwrap_or(false);
        let 主批量就绪 = if let Some((ref task_list, _)) = &self.主程序批量等待 {
            task_list.contains(&完成者) && task_list.iter().all(|t| self.任务已完成(*t))
        } else { false };
        let 主批量超时就绪 = if let Some((ref task_list, _)) = &self.主程序批量等待超时 {
            task_list.contains(&完成者) && task_list.iter().all(|t| self.任务已完成(*t))
        } else { false };
        if 唤醒主 {
            let (_, dest) = self.主程序等待.take().unwrap();
            self.恢复主任务_带结果(dest, 结果);
            return;
        }
        if 主批量就绪 {
            let (_, dest) = self.主程序批量等待.take().unwrap();
            self.恢复主任务_带结果(dest, 运行时值::空值());
            return;
        }
        if 主批量超时就绪 {
            if let Some(tid) = self.主程序活跃定时器标识.take() { self.已取消定时器.insert(tid); }
            let (task_list, dest) = self.主程序批量等待超时.take().unwrap();
            let 结果数组: Vec<运行时值> = task_list.iter().map(|t| self.取任务结果(*t)).collect();
            let aid = 堆分配(堆数据::数组(结果数组));
            self.恢复主任务_带结果(dest, 运行时值::句柄(aid));
            return;
        }
        self.调度下一任务();
    }

    pub(crate) fn 完成任务处理(&mut self, 完成者: usize, 结果: 运行时值) {
        if let Some(c) = self.任务表.get_mut(&完成者) {
            c.结果 = Some(结果);
            c.状态 = 任务状态::完成;
        }
        let mut 唤醒_单个: Vec<(usize, 操作数源)> = vec![];
        let mut 唤醒_批量: Vec<(usize, 操作数源)> = vec![];
        let mut 唤醒_批量超时_待构建: Vec<(usize, 操作数源, Vec<usize>)> = vec![];
        for (&id, c) in self.任务表.iter() {
            if let Some((waiting_id, _)) = &c.等待任务 {
                if *waiting_id == 完成者 && c.状态 == 任务状态::阻塞 {
                    唤醒_单个.push((id, c.等待任务.as_ref().unwrap().1.clone()));
                }
            }
            if let Some((ref task_list, _)) = &c.批量等待任务 {
                if task_list.contains(&完成者) && task_list.iter().all(|t| self.任务已完成(*t)) {
                    唤醒_批量.push((id, c.批量等待任务.as_ref().unwrap().1.clone()));
                }
            }
            if let Some((ref task_list, ref dest)) = &c.批量等待超时任务 {
                if task_list.contains(&完成者) && task_list.iter().all(|t| self.任务已完成(*t)) {
                    唤醒_批量超时_待构建.push((id, dest.clone(), task_list.clone()));
                }
            }
        }
        let mut 唤醒_批量超时: Vec<(usize, 操作数源, 运行时值)> = vec![];
        for (id, dest, task_list) in 唤醒_批量超时_待构建 {
            let 结果数组: Vec<运行时值> = task_list.iter().map(|t| self.取任务结果(*t)).collect();
            let aid = 堆分配(堆数据::数组(结果数组));
            唤醒_批量超时.push((id, dest, 运行时值::句柄(aid)));
        }
        for (id, dest) in 唤醒_单个 {
            if let Some(c) = self.任务表.get_mut(&id) { c.状态 = 任务状态::就绪; c.等待任务 = None; }
            self.写任务结果(id, dest, 结果);
            self.任务就绪队列.push_back(id);
        }
        for (id, dest) in 唤醒_批量 {
            if let Some(c) = self.任务表.get_mut(&id) { c.状态 = 任务状态::就绪; c.批量等待任务 = None; }
            self.写任务结果(id, dest, 结果);
            self.任务就绪队列.push_back(id);
        }
        for (id, dest, 结果数组) in 唤醒_批量超时 {
            let timer_id = self.任务表.get(&id).and_then(|c| c.活跃定时器标识);
            if let Some(tid) = timer_id { self.已取消定时器.insert(tid); }
            if let Some(c) = self.任务表.get_mut(&id) { c.状态 = 任务状态::就绪; c.批量等待超时任务 = None; c.活跃定时器标识 = None; }
            self.写任务结果(id, dest, 结果数组);
            self.任务就绪队列.push_back(id);
        }
        let 唤醒主 = self.主程序等待.as_ref().map(|(t,_)| *t == 完成者).unwrap_or(false);
        let 主批量就绪 = if let Some((ref task_list, _)) = &self.主程序批量等待 {
            task_list.contains(&完成者) && task_list.iter().all(|t| self.任务已完成(*t))
        } else { false };
        let 主批量超时就绪 = if let Some((ref task_list, _)) = &self.主程序批量等待超时 {
            task_list.contains(&完成者) && task_list.iter().all(|t| self.任务已完成(*t))
        } else { false };
        if 唤醒主 {
            let (_, dest) = self.主程序等待.take().unwrap();
            self.恢复主任务_带结果(dest, 结果);
            return;
        }
        if 主批量就绪 {
            let (_, dest) = self.主程序批量等待.take().unwrap();
            self.恢复主任务_带结果(dest, 运行时值::空值());
            return;
        }
        if 主批量超时就绪 {
            if let Some(tid) = self.主程序活跃定时器标识.take() { self.已取消定时器.insert(tid); }
            let (task_list, dest) = self.主程序批量等待超时.take().unwrap();
            let 结果数组: Vec<运行时值> = task_list.iter().map(|t| self.取任务结果(*t)).collect();
            let aid = 堆分配(堆数据::数组(结果数组));
            self.恢复主任务_带结果(dest, 运行时值::句柄(aid));
            return;
        }
        self.调度下一任务();
    }

    pub(crate) fn 写任务结果(&mut self, tid: usize, dest: 操作数源, 结果: 运行时值) {
        if let Some(c) = self.任务表.get_mut(&tid) {
            if let 操作数源::寄存器(idx) = dest {
                while c.保存的局部变量表.len() <= idx {
                    c.保存的局部变量表.push(运行时值::空值());
                }
                c.保存的局部变量表[idx] = 结果;
            }
        }
    }

    pub(crate) fn 任务让出(&mut self) { self.任务让出_自愿(); }

    pub(crate) fn 处理阻塞(&mut self, 原因: &阻塞原因) {
        match 原因 {
            阻塞原因::休眠(截止时间) => {
                if std::time::Instant::now() >= *截止时间 { self.任务状态 = 任务运行状态::运行中; self.阻塞原因 = None; }
                else { self.任务让出(); }
            }
            _ => { self.任务让出(); }
        }
    }
}
