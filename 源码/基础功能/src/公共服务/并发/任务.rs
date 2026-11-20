// 文件: 并发运行时/src/任务.rs
// 职责: 基于 std::thread + crossbeam 结果通道实现文档 5.7.2 的任务调度。
//   - 启动任务: 在独立 OS 线程执行闭包，结果通过 bounded(1) 通道回传。
//   - 等待结果: 阻塞接收结果，并缓存以支持多次调用。
//   - 批量等待: 按传入顺序返回各任务结果。
//   - 批量等待(超时): 用 recv_deadline，超时填空值；未完成任务线程继续运行，
//     稍后调用 等待结果() 仍可获取最终结果（符合文档语义）。
//   - 任务让出: 提示 OS 调度器让出当前线程时间片。
//   - 异常隔离: 用 catch_unwind 捕获任务内 panic，转为 拥有值::异常 回传。

use std::panic::{catch_unwind, AssertUnwindSafe};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Condvar, Mutex};
use std::time::{Duration, Instant};

use std::sync::Arc;

use super::值类型_::拥有值;

/// 任务句柄内部状态（被 Arc 包裹，可 Clone）
///
/// 完成状态由任务线程「一次发布」，所有等待者读取同一份：
/// 旧实现用 bounded(1) 通道 + 事后缓存，两个等待者会竞争唯一一条消息，
/// 落败者把「通道断开」当成空值返回并可能覆盖缓存；超时批量等待还完全绕过缓存，
/// 导致「先 等待结果 再 批量等待(超时)」拿到空值。
#[derive(Debug)]
pub struct 任务句柄内部 {
    // 完成状态：None = 尚未完成；Some = 任务最终结果（成功值或 拥有值::异常）
    完成状态: Mutex<Option<拥有值>>,
    // 完成通知：任务线程写入完成状态后 notify_all
    完成通知: Condvar,
    // 线程结束标志：任务体（含 catch_unwind 包装）真正执行完毕后置真。
    已结束: Arc<AtomicBool>,
}

/// 任务句柄（Clone 仅克隆 Arc，底层共享同一任务）
#[derive(Debug, Clone)]
pub struct 任务句柄(pub Arc<任务句柄内部>);

impl 任务句柄 {
    /// 启动任务：在独立 OS 线程执行闭包，结果发布到共享完成状态。
    /// 闭包返回自包含的 `拥有值`，内部 catch_unwind 捕获异常。
    /// 注：使用独立线程（而非 rayon 线程池），避免任务内再等待子任务时
    ///     阻塞 rayon 工作线程导致线程池饥饿死锁。
    /// 线程创建失败（系统资源耗尽）时不 panic：任务直接以 `任务异常` 完成，由 等待结果 重抛。
    pub fn 启动(闭包: impl FnOnce() -> 拥有值 + Send + 'static) -> 任务句柄 {
        let 已结束 = Arc::new(AtomicBool::new(false));
        let 句柄 = 任务句柄(Arc::new(任务句柄内部 {
            完成状态: Mutex::new(None),
            完成通知: Condvar::new(),
            已结束: 已结束.clone(),
        }));
        let 线程侧 = 句柄.clone();
        let 启动结果 = std::thread::Builder::new().spawn(move || {
            let 结果 = catch_unwind(AssertUnwindSafe(闭包));
            let 最终值 = match 结果 {
                Ok(v) => v,
                Err(载荷) => {
                    // 保留 panic 文本，便于定位（旧实现直接丢弃）
                    let 详情 = 载荷
                        .downcast_ref::<String>()
                        .cloned()
                        .or_else(|| 载荷.downcast_ref::<&str>().map(|s| s.to_string()));
                    拥有值::异常(
                        "任务异常".to_string(),
                        match 详情 {
                            Some(文本) => format!("任务执行过程中发生未捕获异常：{}", 文本),
                            None => "任务执行过程中发生未捕获异常".to_string(),
                        },
                    )
                }
            };
            线程侧.发布完成(最终值);
        });
        if let Err(错误) = 启动结果 {
            句柄.发布完成(拥有值::异常(
                "任务异常".to_string(),
                format!("无法创建任务线程：{}", 错误),
            ));
        }
        句柄
    }

    /// 任务线程调用：写入最终结果并唤醒所有等待者（仅首次生效）。
    fn 发布完成(&self, 最终值: 拥有值) {
        // 先置位“已结束”，使 是否完成() 不依赖是否曾 等待结果()。
        self.0.已结束.store(true, Ordering::SeqCst);
        let mut 状态 = self.0.完成状态.lock().unwrap_or_else(|e| e.into_inner());
        if 状态.is_none() {
            *状态 = Some(最终值);
        }
        drop(状态);
        self.0.完成通知.notify_all();
    }

    /// 等待结果：阻塞直到任务完成，返回结果（可被任意多个等待者、任意多次调用）。
    pub fn 等待结果(&self) -> 拥有值 {
        let mut 状态 = self.0.完成状态.lock().unwrap_or_else(|e| e.into_inner());
        loop {
            if let Some(v) = 状态.as_ref() {
                return v.clone();
            }
            状态 = self.0.完成通知.wait(状态).unwrap_or_else(|e| e.into_inner());
        }
    }

    /// 限时等待：截止时间前完成返回 Some(结果)，否则 None（任务继续运行）。
    fn 等待至(&self, 截止时间: Instant) -> Option<拥有值> {
        let mut 状态 = self.0.完成状态.lock().unwrap_or_else(|e| e.into_inner());
        loop {
            if let Some(v) = 状态.as_ref() {
                return Some(v.clone());
            }
            let 现在 = Instant::now();
            if 现在 >= 截止时间 {
                return None;
            }
            let (新状态, _) = self.0.完成通知
                .wait_timeout(状态, 截止时间 - 现在)
                .unwrap_or_else(|e| e.into_inner());
            状态 = 新状态;
        }
    }

    /// 批量等待：按传入顺序返回所有结果
    pub fn 批量等待(句柄列表: &[任务句柄]) -> Vec<拥有值> {
        句柄列表.iter().map(|h| h.等待结果()).collect()
    }

    /// 批量等待（带超时）：超时返回空值，未完成任务继续运行；
    /// 已完成的任务（含此前已被等待过的）总是返回其真实结果。
    pub fn 批量等待超时(句柄列表: &[任务句柄], 超时毫秒: u64) -> Vec<拥有值> {
        let 截止时间 = Instant::now() + Duration::from_millis(超时毫秒);
        句柄列表
            .iter()
            .map(|h| h.等待至(截止时间).unwrap_or(拥有值::空值))
            .collect()
    }

    /// 非阻塞查询任务结果是否已就绪（已被接收并写入缓存）。
    /// 批量等待超时后，已完成的任务该值为 true，超时未完成的为 false，
    /// 调用方借此区分“完成”与“超时”，避免误释放仍在运行的任务句柄。
    pub fn 结果是否就绪(&self) -> bool {
        self.0.完成状态.lock().unwrap_or_else(|e| e.into_inner()).is_some()
    }

    /// 文档 5.7 语义：任务.是否完成() 非阻塞查询。
    /// 查“任务线程是否已真正结束”，与结果是否曾被取走无关——
    /// 即使调用方从未 等待结果()，任务跑完后仍返回真。
    pub fn 是否完成(&self) -> bool {
        self.0.已结束.load(Ordering::SeqCst)
    }
}

/// 任务让出：提示 OS 调度器让出当前线程时间片。
/// 用于长计算任务防止饿死其他任务。
pub fn 任务让出() {
    std::thread::yield_now();
}
