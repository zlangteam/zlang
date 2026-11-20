// 文件: 并发运行时/src/通道.rs
// 职责: 基于 crossbeam 通道实现文档 5.7.3 的通道语义。
//   - 数据通道: 携带 `拥有值`，支持有缓冲 / 同步（容量=0）两种模式。
//   - 关闭信号通道: 专用于"监听通道关闭事件"，关闭时接收端立即可感知断开。
//   完全复用 Rust 生态能力，无自建缓冲区、无自建等待队列。

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};

use crossbeam::channel::{bounded, Receiver, Sender};

use super::值类型_::拥有值;

/// 通道：进程内线程安全的通信载体。
/// 通过 `Arc` 共享，可被多个任务持有。
#[derive(Debug)]
pub struct 通道 {
    // ── 数据通道（携带自包含值）──
    数据发送端: Sender<拥有值>,
    数据接收端: Receiver<拥有值>,

    // ── 关闭信号通道（专用于"监听通道关闭事件"）──
    // 用 Option 包裹：关闭时 take() 并 drop，使接收端断开
    关闭信号发送端: Mutex<Option<Sender<()>>>,
    关闭信号接收端: Receiver<()>,

    // ── 已关闭标志（原子，读写为 O(1) 不阻塞）──
    已关闭: Arc<AtomicBool>,
}

impl 通道 {
    /// 创建通道。容量 0 = 同步通道（rendezvous，发送必须等接收方就绪）。
    pub fn 新建(容量: usize) -> Arc<通道> {
        let (数据发送端, 数据接收端) = if 容量 == 0 {
            bounded(0)
        } else {
            bounded(容量)
        };
        // 关闭信号通道：用 bounded(0)，drop 发送端后接收端立即可感知断开
        let (关闭信号发送端, 关闭信号接收端) = bounded(0);
        Arc::new(通道 {
            数据发送端,
            数据接收端,
            关闭信号发送端: Mutex::new(Some(关闭信号发送端)),
            关闭信号接收端,
            已关闭: Arc::new(AtomicBool::new(false)),
        })
    }

    /// 发送数据（调用方需先把具体值转为自包含的 `拥有值`）。
    /// 向已关闭通道发送 -> 返回 Err(原数据)，由上层转为运行时异常。
    pub fn 发送(&self, 数据: 拥有值) -> Result<(), 拥有值> {
        if self.已关闭.load(Ordering::SeqCst) {
            return Err(数据);
        }
        // 快路径：有空位（或同步通道已有接收方在等）直接送达
        let 数据 = match self.数据发送端.try_send(数据) {
            Ok(()) => return Ok(()),
            Err(e) => e.into_inner(),
        };
        // 慢路径：通道已满，等「有空位」或「通道被关闭」。
        // 旧实现直接阻塞在 send 上：通道满时发送方卡住，之后即使通道被关闭也永远不会醒来
        // （数据发送端与接收端都由通道自身持有，不会断开）。
        let mut sel = crossbeam::channel::Select::new();
        let 发送序 = sel.send(&self.数据发送端);
        let _关闭序 = sel.recv(&self.关闭信号接收端);
        let 操作 = sel.select();
        if 操作.index() == 发送序 {
            操作.send(&self.数据发送端, 数据).map_err(|e| e.0)
        } else {
            let _ = 操作.recv(&self.关闭信号接收端);
            Err(数据)
        }
    }

    /// 接收数据，完整匹配文档关闭语义：
    ///   - 有数据 -> 返回数据
    ///   - 无数据且未关闭 -> 阻塞直到有数据或通道关闭
    ///   - 无数据且已关闭 -> 排空残留后返回 None（对应智语言空值）
    pub fn 接收(&self) -> Option<拥有值> {
        crossbeam::select! {
            recv(self.数据接收端) -> 结果 => {
                match 结果 {
                    Ok(值) => Some(值),
                    Err(_) => None, // 所有发送端已断开且无数据
                }
            }
            recv(self.关闭信号接收端) -> _ => {
                // 通道已关闭：最后一次排空缓冲区
                self.数据接收端.try_recv().ok()
            }
        }
    }

    /// 关闭通道，禁止继续发送，但缓冲区残留数据仍可被接收。幂等。
    pub fn 关闭(&self) {
        if !self.已关闭.swap(true, Ordering::SeqCst) {
            // take() 取出并 drop 信号发送端 -> 所有监听"通道关闭事件"的 select 立即触发
            let _ = self.关闭信号发送端.lock().unwrap().take();
            // 注意：数据发送端保留，缓冲区中残留数据仍可被接收
        }
        // 第二次调用：已关闭=true，swap 返回 true，不执行任何操作 -> 幂等
    }

    /// 是否关闭（不阻塞）
    /// 检测条件：已关闭标志 + 信号通道接收端断连（所有发送端被 drop）
    pub fn 是否关闭(&self) -> bool {
        self.已关闭.load(Ordering::SeqCst) || self.信号通道断连()
    }

    /// 检测信号接收端是否已断连（发送端被 drop）
    fn 信号通道断连(&self) -> bool {
        matches!(
            self.关闭信号接收端.try_recv(),
            Err(crossbeam::channel::TryRecvError::Disconnected)
        )
    }

    /// 尝试发送：非阻塞。通道满或已关闭时立即返回 Err（不等待）。
    /// 文档 5.7.3 补充能力：跨线程场景避免任务在发送端卡死。
    pub fn 尝试发送(&self, 数据: 拥有值) -> Result<(), 拥有值> {
        if self.已关闭.load(Ordering::SeqCst) {
            return Err(数据);
        }
        self.数据发送端.try_send(数据).map_err(|e| e.into_inner())
    }

    /// 尝试接收：非阻塞。通道空时立即返回 None（不等待），不区分“空”与“已关闭且无残留”。
    pub fn 尝试接收(&self) -> Option<拥有值> {
        match self.数据接收端.try_recv() {
            Ok(值) => Some(值),
            Err(crossbeam::channel::TryRecvError::Empty) => None,
            Err(crossbeam::channel::TryRecvError::Disconnected) => None,
        }
    }

    /// 暴露数据接收端（供多路监听注册通道可读事件）
    pub fn 数据接收端(&self) -> &Receiver<拥有值> {
        &self.数据接收端
    }

    /// 暴露关闭信号接收端（供多路监听注册"通道关闭事件"）
    pub fn 关闭信号接收端(&self) -> &Receiver<()> {
        &self.关闭信号接收端
    }
}
