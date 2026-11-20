// 文件: 并发运行时/src/多路监听.rs
// 职责: 基于 crossbeam::Select 实现文档 5.7.4 的多路事件监听。
//   - 监听通道(名称, 通道): 注册通道可读事件。
//   - 监听超时(名称, 毫秒): 注册超时事件（毫秒=0 表示非阻塞）。
//   - 监听终止(名称, 终止器): 注册终止器停止信号事件。
//   - 多路监听(选项列表): 任意事件就绪立即返回，返回 {"来源": 名称}。
//
// 超时用 `crossbeam::channel::after` 的定时接收端表达（到点即就绪），统一用 `sel.ready()` 等待。
// 旧实现为每个超时选项起一个计时线程并睡满全程：监听提前返回后线程仍在睡，
// 在循环里带长超时调用会堆积成千上万个睡眠线程。
// 解释模式与编译模式都走此动态 Select API，保证行为一致（编译模式不生成 select! 宏）。

use std::time::{Duration, Instant};

use crossbeam::channel::{after, Receiver, Select};

use super::值类型_::{键值, 拥有值};

/// 监听选项（动态构建，运行时累积）
#[derive(Clone)]
pub enum 监听选项 {
    // 通道可读：携带 数据接收端 与 关闭信号接收端。
    // 二者任一就绪即视为该来源就绪 —— 与 通道::接收() 的 select! 双分支语义一致：
    // 空且未关闭时阻塞等待数据；已关闭时（关闭信号断开）立即可感知，避免死锁。
    通道可读(String, Receiver<拥有值>, Receiver<()>),
    超时(String, u64),
    终止(String, Receiver<()>),
}

impl 监听选项 {
    pub fn 名称(&self) -> String {
        match self {
            监听选项::通道可读(n, _, _) => n.clone(),
            监听选项::超时(n, _) => n.clone(),
            监听选项::终止(n, _) => n.clone(),
        }
    }
}

/// 执行多路监听，返回 {"来源": 触发来源名称}
pub fn 多路监听(选项列表: &[监听选项]) -> 拥有值 {
    let mut sel = Select::new();
    // 注册顺序：sel 的第 i 个注册操作 -> 选项列表中的索引
    // （crossbeam 的 Select 会持有接收端引用，故不能边注册边 push 同一 Vec）
    let mut 注册顺序: Vec<usize> = Vec::new();
    // 超时接收端先暂存，待所有通道/终止注册完毕后再注册
    let mut 超时列表: Vec<(usize, Receiver<Instant>)> = Vec::new();

    for (opt_idx, opt) in 选项列表.iter().enumerate() {
        match opt {
            监听选项::通道可读(_, 数据_rx, 关闭_rx) => {
                // 同时注册"数据可读"与"通道关闭"两个分支，二者任一就绪即触发本来源
                sel.recv(数据_rx);
                注册顺序.push(opt_idx);
                sel.recv(关闭_rx);
                注册顺序.push(opt_idx);
            }
            监听选项::终止(_, sig_rx) => {
                sel.recv(sig_rx);
                注册顺序.push(opt_idx);
            }
            监听选项::超时(_, 毫秒) => {
                超时列表.push((opt_idx, after(Duration::from_millis(*毫秒))));
            }
        }
    }
    // 统一注册超时接收端（此后 超时列表 不再被修改，引用稳定）
    for (opt_idx, rx) in &超时列表 {
        sel.recv(rx);
        注册顺序.push(*opt_idx);
    }

    // 始终用 ready()：超时由定时接收端保证就绪，不会 panic
    let 触发索引 = sel.ready();
    let 真实选项索引 = 注册顺序[触发索引];
    let 名称 = 选项列表[真实选项索引].名称();

    // 返回 映射 {"来源": 名称}
    拥有值::映射(vec![(
        键值::文本("来源".to_string()),
        拥有值::文本(名称),
    )])
}
