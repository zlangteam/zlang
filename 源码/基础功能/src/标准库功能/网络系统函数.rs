// 文件: 标准库功能/网络系统函数.rs
// 职责: 网络/HTTP/TCP/UDP 系统函数实现（客户端+服务端+协议）+ VM HTTP 钩子。
//       由 分发器.rs::系统调用() 的 121..=154 / 180..=204 分支委托到 处理网络()。
//       主分发器不拆：本文件仅承载网络专用资源表/结构体/辅助与分支体。

use crate::标准库索引;
use crate::值类型::*;
use super::基础辅助::{取参数, 取文本, 取整数};
use std::collections::HashMap;
use std::sync::{Mutex, Arc, LazyLock};
use std::net::{TcpStream, TcpListener, UdpSocket, IpAddr, Ipv4Addr};
use std::io::{Read, Write, BufReader, BufRead, ErrorKind};
use http::Response as HttpResponse;
use rustls::{ClientConnection, ServerConnection, StreamOwned, ServerConfig, ClientConfig, RootCertStore};
use rustls::pki_types::{ServerName, CertificateDer, PrivateKeyDer, PrivatePkcs8KeyDer};
use rustls::client::danger::{ServerCertVerifier, ServerCertVerified, HandshakeSignatureValid};
use rustls::client::WebPkiServerVerifier;
use rustls::DigitallySignedStruct;
use rustls::SignatureScheme;
use rustls::pki_types::UnixTime;
use std::fs;

// 同时实现 Read + Write + Send 的 trait 对象（单个合成 trait，规避 `dyn Read+Write` 双非自动 trait 非法）
trait 网络流: Read + Write + Send {}
impl<T: Read + Write + Send> 网络流 for T {}
use ureq::tls::TlsConfig;

// ═══════════════════════════════════════════════════════════
//  网络模块基础设施 — 类型索引 + 资源表 + 辅助函数
//  与 解释器/src/值表示.rs 常量 严格一致
// ═══════════════════════════════════════════════════════════

// ── 网络结构体类型索引 ──
const 网络连接类型索引: usize    = 2;
const 网络监听器类型索引: usize  = 3;
const 网络数据报类型索引: usize  = 4;
const 网络响应类型索引: usize    = 5;
const 流式响应类型索引: usize    = 6;
const 网络请求类型索引: usize    = 7;
const 网络服务类型索引: usize    = 8;
const 数据报消息类型索引: usize  = 9;

// ── HTTP 线程间通信 ──
pub struct Http待处理请求 {
    pub 方法: String,
    pub 路径: String,
    pub 请求头: HashMap<String, String>,
    pub 内容: String,
    pub 完整网址: String,
    /// 未解码的请求目标（查询参数必须先按 & / = 切分再逐项解码）
    pub 原始网址: String,
    pub 客户端地址: String,
}

// ── 网络槽位 ──
// 预留：各变体携带的 本地/远端地址端口、状态码 等字段当前未被读取（构造时已写入完整信息，
// 供诊断与未来扩展）；显式豁免 dead_code，避免被误判为"死代码"。
#[allow(dead_code)]
enum 网络槽位 {
    Tcp连接 { stream: Arc<Mutex<TcpStream>>, 残余: Arc<Mutex<Vec<u8>>>, 本地地址: String, 本地端口: u16, 远端地址: String, 远端端口: u16 },
    Tcp连接TLS { stream: Arc<Mutex<Box<dyn 网络流>>>, 残余: Arc<Mutex<Vec<u8>>>, 底层: Arc<TcpStream>, 本地地址: String, 本地端口: u16, 远端地址: String, 远端端口: u16 },
    Tcp监听器(Arc<Mutex<TcpListener>>),
    Tcp监听器TLS { config: Arc<ServerConfig>, listener: Arc<Mutex<TcpListener>> },
    UdpSocket(Arc<Mutex<UdpSocket>>),
    流式响应 { reader: Arc<Mutex<Box<dyn BufRead + Send>>>, 状态码: u16, 响应头: HashMap<String,String>, 残余: Arc<Mutex<Vec<u8>>> },
    网络服务 {
        端口: u16,
        服务: Arc<crate::tiny_http::Server>,
        停止: Arc<std::sync::atomic::AtomicBool>,
        完成: Arc<std::sync::atomic::AtomicBool>,
        线程标识: Arc<std::sync::OnceLock<std::thread::ThreadId>>,
    },
    空槽,
}

// ── 全局网络资源表（句柄从5000起，避开并发句柄1-999和堆句柄1000+）──
struct 网络资源表 {
    槽位: Vec<网络槽位>,
    空闲: Vec<usize>,
}

impl 网络资源表 {
    fn 新建() -> Self { Self { 槽位: Vec::new(), 空闲: Vec::new() } }
    fn 分配(&mut self, 资源: 网络槽位) -> u64 {
        if let Some(空闲) = self.空闲.pop() {
            self.槽位[空闲] = 资源;
            (空闲 + 5000) as u64
        } else {
            self.槽位.push(资源);
            (self.槽位.len() - 1 + 5000) as u64
        }
    }
    fn 释放(&mut self, 句柄: u64) {
        // 无效句柄（<5000，例如对空值调用 关闭）直接忽略：旧实现在 debug 下减法溢出 panic。
        if 句柄 < 5000 { return; }
        let i = 句柄 as usize - 5000;
        if i >= self.槽位.len() { return; }
        // 重复关闭必须幂等：旧实现每次都把 i 压入空闲链，重复关闭后两次 分配 会拿到同一槽位，
        // 后一个资源覆盖（并关闭）前一个，两个对象共用同一句柄。
        if matches!(self.槽位[i], 网络槽位::空槽) { return; }
        self.槽位[i] = 网络槽位::空槽;
        self.空闲.push(i);
    }
    fn 读取(&self, 句柄: u64) -> &网络槽位 {
        // 句柄越界清晰诊断（此前 <5000 的悬空句柄会触发 usize 下溢 panic，难以排查）
        // 无效/越界句柄按「已关闭的空槽」处理，由各调用点走既有的「已关闭」分支（抛异常或返回空）。
        // 旧实现在这里 panic，而调用方此时持有全局 网络表 锁 → 锁中毒，此后进程内所有网络调用全部失败。
        static 无效槽: 网络槽位 = 网络槽位::空槽;
        if 句柄 < 5000 { return &无效槽; }
        let i = 句柄 as usize - 5000;
        self.槽位.get(i).unwrap_or(&无效槽)
    }
    /// 预留：与 `读取` 配对的对称可变访问器（当前未调用）；显式豁免 dead_code。
    #[allow(dead_code)]
    fn 读取可变(&mut self, 句柄: u64) -> &mut 网络槽位 { &mut self.槽位[句柄 as usize - 5000] }
}

static 网络表: LazyLock<Mutex<网络资源表>> = LazyLock::new(|| Mutex::new(网络资源表::新建()));

// ── HTTP 服务端统一实现（2026-08-19 阶段二）──
// 原「解释模式请求表 + 主线程每 256 步轮询泵」已删除：147 统一在后台 accept 线程同步调
// 执行后台用户函数（解释器=子VM执行器 / 编译器=回调桥），不再需要全局队列与主线程协作。

/// 供 VM 钩子调用：从运行时堆构造网络请求结构体并返回（随后VM转值→调用处理器）
pub fn HTTP构造网络请求(请求: &Http待处理请求) -> 运行时值 {
    let 方法句柄 = 分配文本(请求.方法.clone());
    let 路径句柄 = 分配文本(请求.路径.clone());
    let 内容句柄 = 分配文本(请求.内容.clone());
    let 网址句柄 = 分配文本(请求.完整网址.clone());
    let 地址句柄 = 分配文本(请求.客户端地址.clone());
    let 请求头句柄 = 分配映射(文本映射转堆条目(&请求.请求头));
    let 查询参数句柄 = 解析查询参数(&请求.原始网址);
    分配结构体(网络请求类型索引, vec![
        运行时值::句柄(方法句柄.句柄), 运行时值::句柄(路径句柄.句柄),
        运行时值::句柄(请求头句柄.句柄), 运行时值::句柄(内容句柄.句柄),
        运行时值::句柄(查询参数句柄.句柄), 运行时值::句柄(网址句柄.句柄),
        运行时值::句柄(地址句柄.句柄),
    ])
}

/// 供 VM 钩子调用：从运行时网络响应结构体提取字段
pub fn HTTP提取响应字段(响应: &运行时值) -> (u16, String, HashMap<String,String>, Vec<(String,String)>) {
    if 响应.标签 != 标签_句柄 { return (500, String::new(), HashMap::new(), Vec::new()); }
    let h = 响应.句柄;
    let 状态码 = 读字段整数(h, 0) as u16;
    let 内容 = 读字段文本(h, 1);
    let mut 响应头 = HashMap::new();
    let 头句柄 = 读字段句柄(h, 2);
    if let Some(堆数据::映射(m)) = 堆读取(头句柄) {
        for (k, v) in &m {
            if let 堆键::文本(key) = k { 响应头.insert(key.clone(), v.转文本()); }
        }
    }
    // 字段3 = 会话凭证映射（网络.设置会话凭证 写入）；此前从未被读取，导致 Set-Cookie 永远不上线
    let mut 凭证条目 = Vec::new();
    let 凭证句柄 = 读字段句柄(h, 3);
    if let Some(堆数据::映射(m)) = 堆读取(凭证句柄) {
        for (k, v) in &m {
            if let 堆键::文本(名) = k { 凭证条目.push((名.clone(), v.转文本())); }
        }
    }
    (状态码, 内容, 响应头, 凭证条目)
}

// ── 结构体字段读取辅助 ──
fn 读字段整数(堆句柄: u64, 字段索引: usize) -> i64 {
    match 堆读取(堆句柄) {
        Some(堆数据::结构体 { 字段值, .. }) => 字段值.get(字段索引).map(|v| v.整数).unwrap_or(0),
        _ => 0,
    }
}
fn 读字段句柄(堆句柄: u64, 字段索引: usize) -> u64 {
    match 堆读取(堆句柄) {
        Some(堆数据::结构体 { 字段值, .. }) =>
            字段值.get(字段索引).and_then(|v| if v.标签 == 标签_句柄 { Some(v.句柄) } else { None }).unwrap_or(0),
        _ => 0,
    }
}
fn 读字段文本(堆句柄: u64, 字段索引: usize) -> String {
    let h = 读字段句柄(堆句柄, 字段索引);
    堆读取文本字符串(h).unwrap_or_default()
}
fn 读字段资源句柄(堆句柄: u64) -> u64 { 读字段整数(堆句柄, 0) as u64 }

// 从参数数组提取结构体句柄（网络资源以结构体形式传入，其句柄指向网络表槽位）
fn 取句柄(参数: &[运行时值], idx: usize) -> u64 {
    if let Some(v) = 参数.get(idx) {
        if v.标签 == 标签_句柄 { return v.句柄; }
    }
    0
}

// ── 抛异常辅助 ──
fn 抛网络异常(消息: String) {
    let t = "网络错误";
    crate::异常运行时::智语言_抛出异常(t.as_ptr(), t.len() as i64, 消息.as_ptr(), 消息.len() as i64);
}

// ── TCP 流读写辅助（统一处理 TcpStream / TLS 流）──
/// 带「回退缓冲」的读者：`读取` 遇到被数据块边界切断的多字节字符时，把不完整的尾部字节
/// 回退到连接的残余缓冲，下一次任意读操作（读取/读取指定/读取一行/读取全部）先消费它。
struct 带残余读者<'a> {
    残余: &'a mut Vec<u8>,
    内: &'a mut dyn Read,
}
impl 带残余读者<'_> {
    fn 回退(&mut self, 字节: &[u8]) {
        // 回退的字节在时间上早于残余里现存的内容，前插以保持字节序。
        let mut 新 = 字节.to_vec();
        新.extend_from_slice(self.残余);
        *self.残余 = 新;
    }
}
impl Read for 带残余读者<'_> {
    fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
        if !self.残余.is_empty() && !buf.is_empty() {
            let n = self.残余.len().min(buf.len());
            buf[..n].copy_from_slice(&self.残余[..n]);
            self.残余.drain(..n);
            return Ok(n);
        }
        self.内.read(buf)
    }
}

fn tcp_读(资源句柄: u64, 操作: &dyn Fn(&mut 带残余读者) -> Option<String>) -> Option<String> {
    enum 流 { 明文(Arc<Mutex<TcpStream>>), 加密(Arc<Mutex<Box<dyn 网络流>>>) }
    let 取: Option<(流, Arc<Mutex<Vec<u8>>>)> = {
        let 表 = 网络表.lock().unwrap();
        match 表.读取(资源句柄) {
            网络槽位::Tcp连接TLS { stream, 残余, .. } => Some((流::加密(stream.clone()), 残余.clone())),
            网络槽位::Tcp连接 { stream, 残余, .. } => Some((流::明文(stream.clone()), 残余.clone())),
            _ => None,
        }
    };
    let (流, 残余) = 取?;
    // 锁顺序固定：先流后残余（残余只在持有流锁期间访问）
    match 流 {
        流::加密(arc) => {
            let mut 守卫 = arc.lock().unwrap();
            let mut 残 = 残余.lock().unwrap();
            操作(&mut 带残余读者 { 残余: &mut 残, 内: &mut **守卫 })
        }
        流::明文(arc) => {
            let mut 守卫 = arc.lock().unwrap();
            let mut 残 = 残余.lock().unwrap();
            操作(&mut 带残余读者 { 残余: &mut 残, 内: &mut *守卫 })
        }
    }
}

    fn tcp_写(资源句柄: u64, 操作: &dyn Fn(&mut dyn Write)) -> bool {
    let arc_tls: Option<Arc<Mutex<Box<dyn 网络流>>>> = {
        let 表 = 网络表.lock().unwrap();
        if let 网络槽位::Tcp连接TLS { ref stream, .. } = *表.读取(资源句柄) { Some(stream.clone()) } else { None }
    };
    if let Some(arc) = arc_tls { 操作(&mut *arc.lock().unwrap()); return true; }
    let arc: Option<Arc<Mutex<TcpStream>>> = {
        let 表 = 网络表.lock().unwrap();
        if let 网络槽位::Tcp连接 { ref stream, .. } = *表.读取(资源句柄) { Some(stream.clone()) } else { None }
    };
    if let Some(arc) = arc { 操作(&mut *arc.lock().unwrap()); return true; }
    false
}

fn tcp_设置选项(资源句柄: u64, 操作: &dyn Fn(&TcpStream)) {
    // 对 TLS 连接，通过保留的底层 TcpStream 设置选项（超时等）
    let arc_tls: Option<Arc<TcpStream>> = {
        let 表 = 网络表.lock().unwrap();
        if let 网络槽位::Tcp连接TLS { ref 底层, .. } = *表.读取(资源句柄) { Some(底层.clone()) } else { None }
    };
    if let Some(底层) = arc_tls { 操作(&*底层); return; }
    let arc: Option<Arc<Mutex<TcpStream>>> = {
        let 表 = 网络表.lock().unwrap();
        if let 网络槽位::Tcp连接 { ref stream, .. } = *表.读取(资源句柄) { Some(stream.clone()) } else { None }
    };
    if let Some(arc) = arc { 操作(&*arc.lock().unwrap()); }
}

// ── 映射条目辅助 ──
fn 文本映射转堆条目(m: &HashMap<String,String>) -> Vec<(堆键, 运行时值)> {
    m.iter().map(|(k,v)| (堆键::文本(k.clone()), 分配文本(v.clone()))).collect()
}

// ── 取映射参数（从运行时值哈希映射提取）──
fn 取映射参数(参数: &[运行时值], idx: usize) -> HashMap<String, String> {
    let mut result = HashMap::new();
    if let Some(v) = 参数.get(idx) {
        if v.标签 == 标签_句柄 {
            if let Some(堆数据::映射(m)) = 堆读取(v.句柄) {
                for (k, val) in &m {
                    if let 堆键::文本(key) = k { result.insert(key.clone(), val.转文本()); }
                }
            }
        }
    }
    result
}

fn 取选项文本(选项: &HashMap<String,String>, 键: &str) -> Option<String> { 选项.get(键).cloned() }
fn 取选项整数(选项: &HashMap<String,String>, 键: &str, 默认: i64) -> i64 {
    选项.get(键).and_then(|v| v.parse().ok()).unwrap_or(默认)
}
fn 取选项逻辑(选项: &HashMap<String,String>, 键: &str) -> bool {
    // 缺失键默认 false（如 TLS/验证证书）；跟随重定向 例外默认 true（见 构建HTTP代理 处显式 unwrap_or(true)）。
    选项.get(键).map(|v| v != "假" && v != "false" && v != "否").unwrap_or(false)
}

/// 带超时的 TCP 连接（零依赖实现）。
/// 在子线程中执行阻塞 connect，主线程按 超时毫秒 等待；超时则放弃该连接并返回错误。
/// 超时=0 时退化为标准库无界 connect（保持原有行为）。
/// 用途：消除 VM 协作式步数保护被原生阻塞调用绕过的架构短板（见记忆「VM 系统函数无限循环」）。
fn 连接带超时(地址: &str, 超时毫秒: i64) -> std::io::Result<TcpStream> {
    if 超时毫秒 <= 0 {
        return TcpStream::connect(地址);
    }
    let 地址克隆 = 地址.to_string();
    let (发送端, 接收端) = std::sync::mpsc::channel::<std::io::Result<TcpStream>>();
    let _句柄 = std::thread::spawn(move || {
        let 结果 = TcpStream::connect(&地址克隆);
        let _ = 发送端.send(结果);
    });
    match 接收端.recv_timeout(std::time::Duration::from_millis(超时毫秒 as u64)) {
        Ok(结果) => 结果,
        Err(_) => Err(std::io::Error::new(
            std::io::ErrorKind::TimedOut,
            format!("网络连接超时（{} 毫秒）", 超时毫秒),
        )),
    }
}

// ── TLS（rustls）辅助 ──
// 全接受验证器：验证证书=假 时跳过证书校验（规范 §5.3.5 要求逐请求可控）
#[derive(Debug)]
struct 全接受验证器;
impl ServerCertVerifier for 全接受验证器 {
    fn verify_server_cert(
        &self,
        _端实体: &CertificateDer,
        _中间: &[CertificateDer],
        _服务器名: &ServerName,
        _ocsp: &[u8],
        _时间: UnixTime,
    ) -> Result<ServerCertVerified, rustls::Error> {
        Ok(ServerCertVerified::assertion())
    }
    fn verify_tls12_signature(
        &self,
        _消息: &[u8],
        _证书: &CertificateDer,
        _签名: &DigitallySignedStruct,
    ) -> Result<HandshakeSignatureValid, rustls::Error> {
        Ok(HandshakeSignatureValid::assertion())
    }
    fn verify_tls13_signature(
        &self,
        _消息: &[u8],
        _证书: &CertificateDer,
        _签名: &DigitallySignedStruct,
    ) -> Result<HandshakeSignatureValid, rustls::Error> {
        Ok(HandshakeSignatureValid::assertion())
    }
    fn supported_verify_schemes(&self) -> Vec<SignatureScheme> {
        vec![
            SignatureScheme::RSA_PKCS1_SHA256,
            SignatureScheme::RSA_PSS_SHA256,
            SignatureScheme::ECDSA_NISTP256_SHA256,
            SignatureScheme::ED25519,
        ]
    }
}

// 解析服务器名：裸 IP → IpAddress，域名 → DNS 名称（rustls 要求区分）。
// DNS 名需 'static（连接存入全局网络表），故对域名字符串执行 Box::leak 提升为 'static。
fn 解析服务器名(地址: &str) -> ServerName<'static> {
    if let Ok(ip) = 地址.parse::<IpAddr>() {
        ServerName::IpAddress(ip.into())
    } else {
        let 静态: &'static str = Box::leak(地址.to_string().into_boxed_str());
        ServerName::try_from(静态).unwrap_or_else(|_| ServerName::IpAddress(Ipv4Addr::LOCALHOST.into()))
    }
}

// 最小 base64 解码（忽略空白/换行，遇 '=' 停止填充）— 避免引入 base64 外部依赖
fn 解码base64(输入: &[u8]) -> Option<Vec<u8>> {
    let mut 缓存: u32 = 0;
    let mut 位数: u32 = 0;
    let mut 输出: Vec<u8> = Vec::new();
    for &b in 输入 {
        let 值 = match b {
            b'A'..=b'Z' => b - b'A',
            b'a'..=b'z' => b - b'a' + 26,
            b'0'..=b'9' => b - b'0' + 52,
            b'+' => 62,
            b'/' => 63,
            b'=' => break,
            _ => continue,
        } as u32;
        缓存 = (缓存 << 6) | 值;
        位数 += 6;
        if 位数 >= 8 {
            位数 -= 8;
            输出.push((缓存 >> 位数) as u8);
        }
    }
    Some(输出)
}

// 从 PEM 中取出所有指定标签的 DER 块（标签含 子串匹配，如 "CERTIFICATE"/"PRIVATE KEY"）
fn 解析PEM(字节: &[u8], 标签含: &str) -> Vec<Vec<u8>> {
    let 文本 = String::from_utf8_lossy(字节);
    let mut 结果: Vec<Vec<u8>> = Vec::new();
    let mut 收集 = String::new();
    let mut 采集中 = false;
    for 行 in 文本.lines() {
        if 行.starts_with("-----BEGIN ") {
            采集中 = 行.contains(标签含);
            收集 = String::new();
        } else if 行.starts_with("-----END ") {
            if 采集中 {
                if let Some(d) = 解码base64(收集.as_bytes()) { 结果.push(d); }
            }
            采集中 = false;
        } else if 采集中 {
            收集.push_str(行.trim());
        }
    }
    结果
}

// 从 PEM 文件加载证书链 + 私钥（未加密 PEM；不支持 P12/加密私钥）
fn 加载服务端TLS配置(证书路径: &str, 密钥路径: &str) -> Option<Arc<ServerConfig>> {
    let 证书 = fs::read(证书路径).ok()?;
    let 密钥 = fs::read(密钥路径).ok()?;
    let 链: Vec<CertificateDer> = 解析PEM(&证书, "CERTIFICATE").into_iter().map(CertificateDer::from).collect();
    let 私钥字节 = 解析PEM(&密钥, "PRIVATE KEY").into_iter().next()?;
    if 链.is_empty() { return None; }
    let 私钥 = PrivateKeyDer::Pkcs8(PrivatePkcs8KeyDer::from(私钥字节));
    ServerConfig::builder().with_no_client_auth().with_single_cert(链, 私钥).ok().map(Arc::new)
}

// 构造客户端 TLS 配置：统一验证证书开关 + 可选客户端证书
fn 构造客户端TLS配置(选项: &HashMap<String, String>) -> Arc<ClientConfig> {
    // 验证证书默认 真（安全默认：对齐浏览器/curl 等业界惯例；自签证书场景须显式传 假）
    let 验证证书 = match 选项.get("验证证书") {
        Some(v) => v != "假" && v != "false" && v != "否",
        None => true,
    };
    let builder = if 验证证书 {
        // 默认用 Mozilla 根证书（webpki-roots，纯 Rust，已 vendored）验证公开 CA
        let 根库 = RootCertStore { roots: webpki_roots::TLS_SERVER_ROOTS.to_vec() };
        let verifier = WebPkiServerVerifier::builder(Arc::new(根库)).build().unwrap();
        ClientConfig::builder().with_webpki_verifier(verifier)
    } else {
        ClientConfig::builder().dangerous().with_custom_certificate_verifier(Arc::new(全接受验证器))
    };
    // 客户端证书（证书/密钥 选项）
    if let (Some(证书路径), Some(密钥路径)) = (取选项文本(选项, "证书"), 取选项文本(选项, "密钥")) {
        if let (Some(证书), Some(密钥)) = (fs::read(&证书路径).ok(), fs::read(&密钥路径).ok()) {
            let 链: Vec<CertificateDer> = 解析PEM(&证书, "CERTIFICATE").into_iter().map(CertificateDer::from).collect();
            if let Some(私钥字节) = 解析PEM(&密钥, "PRIVATE KEY").into_iter().next() {
                let 私钥 = PrivateKeyDer::Pkcs8(PrivatePkcs8KeyDer::from(私钥字节));
                if let Ok(配置) = builder.clone().with_client_auth_cert(链, 私钥) {
                    return Arc::new(配置);
                }
            }
        }
    }
    Arc::new(builder.with_no_client_auth())
}

// ── 嵌套选项子映射提取（如 选项.请求头 / 选项.会话凭证）──
// 从选项映射中取出指定键的子映射（其值为 映射），展平为 文本→文本 表。
fn 取子映射文本(选项值: &运行时值, 子键: &str) -> HashMap<String, String> {
    let mut r = HashMap::new();
    if 选项值.标签 == 标签_句柄 {
        if let Some(堆数据::映射(m)) = 堆读取(选项值.句柄) {
            for (k, v) in &m {
                if let 堆键::文本(key) = k {
                    if key == 子键 && v.标签 == 标签_句柄 {
                        if let Some(堆数据::映射(sub)) = 堆读取(v.句柄) {
                            for (sk, sv) in &sub {
                                if let 堆键::文本(skey) = sk { r.insert(skey.clone(), sv.转文本()); }
                            }
                        }
                    }
                }
            }
        }
    }
    r
}

// ── URL 查询参数解析：把 "?a=1&b=2" 解析为映射 ──
fn 解析查询参数(网址: &str) -> 运行时值 {
    let q = 网址.split_once('?').map(|(_, q)| q).unwrap_or("");
    let mut 条目 = Vec::new();
    for pair in q.split('&') {
        if pair.is_empty() { continue; }
        if let Some((k, v)) = pair.split_once('=') {
            if !k.is_empty() {
                // 先切分后解码：旧实现对整条网址解码后再切，值里的 %26(&) / %3D(=) 会被当成分隔符
                条目.push((堆键::文本(解码网址路径(k)), 分配文本(解码网址路径(v))));
            }
        }
    }
    分配映射(条目)
}

// ── 统一构造 网络响应 结构体（字段: 状态码/内容/响应头/Cookie）──
fn 构建网络响应结构体(状态码: u16, 内容: String, 头条目: Vec<(堆键, 运行时值)>, cookie条目: Vec<(堆键, 运行时值)>) -> 运行时值 {
    分配结构体(网络响应类型索引, vec![
        运行时值::整数(状态码 as i64),
        运行时值::句柄(分配文本(内容).句柄),
        运行时值::句柄(分配映射(头条目).句柄),
        运行时值::句柄(分配映射(cookie条目).句柄),
    ])
}

// ── 根据扁平选项构建 ureq Agent（含超时/重定向/证书校验）──
fn 构建HTTP代理(选项: &HashMap<String, String>) -> ureq::Agent {
    let 超时 = 取选项整数(选项, "超时", 30000);
    // 跟随重定向默认 true：规范 §5.3.3 以「禁止自动跟随重定向」描述 假 的语义，
    // 且「最大重定向次数」默认 10 仅在默认跟随时才有意义（对齐 ureq 默认行为）。
    let 跟随重定向 = match 选项.get("跟随重定向") {
        Some(v) => v != "假" && v != "false" && v != "否",
        None => true,
    };
    let 最大重定向 = 取选项整数(选项, "最大重定向次数", 10);
    // 验证证书默认 真（安全默认；自签证书场景须显式传 假）
    let 验证证书 = match 选项.get("验证证书") {
        Some(v) => v != "假" && v != "false" && v != "否",
        None => true,
    };
    let mut builder = ureq::Agent::config_builder()
        .timeout_global(Some(std::time::Duration::from_millis(超时 as u64)))
        .max_redirects(if 跟随重定向 { 最大重定向.max(0) as u32 } else { 0 })
        // 4xx/5xx 作为正常响应返回（而非 call() 抛 Err），使 网络.获取 能读 404 等状态码
        .http_status_as_error(false);
    if !验证证书 {
        builder = builder.tls_config(TlsConfig::builder().disable_verification(true).build());
    }
    builder.build().new_agent()
}

// ── 向 RequestBuilder 应用 请求头 与 Cookie（按值传入并返回）──
fn 应用请求头<B>(mut req: ureq::RequestBuilder<B>, 选项值: &运行时值, cookie串: &str) -> ureq::RequestBuilder<B> {
    for (k, v) in 取子映射文本(选项值, "请求头") {
        if let Err(消息) = 校验HTTP头(k.as_str(), v.as_str()) { 抛网络异常(消息); }
        req = req.header(k.as_str(), v.as_str());
    }
    if !cookie串.is_empty() { req = req.header("Cookie", cookie串); }
    req
}

// ── 由 会话凭证 子映射拼装 Cookie 请求头字符串 ──
fn 构建Cookie串(cookie: &HashMap<String, String>) -> String {
    // 凭证名/值 percent-encode 成 ASCII（Cookie 头值须 ASCII；服务端 收集请求头 处对称解码还原纯中文）
    cookie.iter().map(|(k,v)| format!("{}={}", 编码凭证分量(k), 编码凭证分量(v)))
        .collect::<Vec<_>>().join("; ")
}

// ── 从 ureq 响应头收集 头条目 与 Set-Cookie 条目 ──
fn 收集响应头与Cookie<B>(resp: &HttpResponse<B>) -> (Vec<(堆键, 运行时值)>, Vec<(堆键, 运行时值)>) {
    let mut 头条目 = Vec::new();
    let mut cookie条目 = Vec::new();
    for (名, 值) in resp.headers() {
        // 线格式 ASCII 头名 → 智语言中文语义键（与 服务端发送映射 对称）
        let 映射 = 线格式头名到中文(&名.as_str().to_ascii_lowercase());
        let 名串 = if !映射.is_empty() { 映射.to_string() } else { 名.as_str().to_string() };
        let 原始值 = 值.to_str().unwrap_or("").to_string();
        // 「位置」是 URL：服务端按 RFC 3986 对中文做了 percent-encode，这里对称解码还原给用户
        //（规范 §5.3.3/§5.3.4 示例直接用 `响应头().获取("位置")` 读可读目标）
        let 值串 = if 名串 == "位置" { 解码网址路径(&原始值) } else { 原始值 };
        头条目.push((堆键::文本(名串.clone()), 分配文本(值串.clone())));
        if 名串.eq_ignore_ascii_case("set-cookie") {
            // 解析 "name=value; path=...; ..." 取首个 name=value；名/值经 解码凭证分量 还原为纯中文
            if let Some(部分) = 值串.split(';').next() {
                if let Some((ck, cv)) = 部分.split_once('=') {
                    let 凭证名 = 解码凭证分量(ck.trim());
                    let 凭证值 = 解码凭证分量(cv.trim());
                    cookie条目.push((堆键::文本(凭证名), 分配文本(凭证值)));
                }
            }
        }
    }
    (头条目, cookie条目)
}

// ── URL 中文路径支持（2026-08-19）──
// ureq 的 http::Uri 不接受非 ASCII 路径（中文 URL → "io: Peer disconnected"）。
// 客户端发送前对非 ASCII 字符做 UTF-8 percent-encoding；服务端接收后解码还原路径。
// 双模共享（base 网络层），智语言源码 URL 可直接写中文路径。
fn 编码网址中文路径(url: &str) -> String {
    let mut 结果 = String::new();
    for c in url.chars() {
        if c.is_ascii() {
            结果.push(c);
        } else {
            let mut 缓冲 = [0u8; 4];
            for b in c.encode_utf8(&mut 缓冲).as_bytes() {
                结果.push_str(&format!("%{:02X}", b));
            }
        }
    }
    结果
}

fn 解码网址路径(路径: &str) -> String {
    let bytes = 路径.as_bytes();
    let mut 结果: Vec<u8> = Vec::new();
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'%' && i + 2 < bytes.len() {
            let h = (bytes[i + 1] as char).to_digit(16);
            let l = (bytes[i + 2] as char).to_digit(16);
            if let (Some(h), Some(l)) = (h, l) {
                结果.push((h * 16 + l) as u8);
                i += 3;
                continue;
            }
        }
        结果.push(bytes[i]);
        i += 1;
    }
    String::from_utf8_lossy(&结果).into_owned()
}

// ── 会话凭证线格式编解码（2026-09-13）──
// Set-Cookie 头名/值须 ASCII（RFC 9110 / tiny_http 校验拒绝非 ASCII），而规范 §5.3.4 示例用中文凭证名（如「会话标识」）。
// 约定：服务端发送前对凭证名/值做 UTF-8 percent-encoding，客户端收集 set-cookie 时解码还原；
// 智语言 API 层始终以纯中文进出，线格式仅 ASCII。
fn 编码凭证分量(s: &str) -> String {
    let mut out = String::new();
    for b in s.as_bytes() {
        if b.is_ascii_graphic() && *b != b'%' && *b != b';' && *b != b'=' {
            out.push(*b as char);
        } else {
            out.push_str(&format!("%{:02X}", b));
        }
    }
    out
}

fn 解码凭证分量(s: &str) -> String {
    let bytes = s.as_bytes();
    let mut 结果: Vec<u8> = Vec::new();
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'%' && i + 2 < bytes.len() {
            let h = (bytes[i + 1] as char).to_digit(16);
            let l = (bytes[i + 2] as char).to_digit(16);
            if let (Some(h), Some(l)) = (h, l) {
                结果.push((h * 16 + l) as u8);
                i += 3;
                continue;
            }
        }
        结果.push(bytes[i]);
        i += 1;
    }
    String::from_utf8_lossy(&结果).into_owned()
}

// Cookie 头值整体解码：按 "名=值; 名=值" 拆分，各分量 解码凭证分量 还原纯中文（客户端 构建Cookie串 编码的对称操作）
fn 解码Cookie值(s: &str) -> String {
    s.split(';').map(|对| {
        match 对.split_once('=') {
            Some((k, v)) => format!("{}={}", 解码凭证分量(k.trim()), 解码凭证分量(v.trim())),
            None => 解码凭证分量(对.trim()),
        }
    }).collect::<Vec<_>>().join("; ")
}

// 中文语义头名 ↔ 线格式 ASCII 头名 映射表（与 位置↔Location 同构；规范 §5.3.4 示例即用 内容类型）
fn 中文头名到线格式(名: &str) -> &'static str {
    match 名 {
        "位置" => "Location",
        "内容类型" => "Content-Type",
        "内容长度" => "Content-Length",
        "缓存控制" => "Cache-Control",
        "内容编码" => "Content-Encoding",
        "连接" => "Connection",
        _ => "",
    }
}

fn 线格式头名到中文(名: &str) -> &'static str {
    match 名 {
        "location" => "位置",
        "content-type" => "内容类型",
        "content-length" => "内容长度",
        "cache-control" => "缓存控制",
        "content-encoding" => "内容编码",
        _ => "",
    }
}

// HTTP 头名/值校验（RFC 9110）：头名须 ASCII token，或命中文语义头名映射表（发送时转 ASCII 线格式名）；
// 头值须 ASCII 可打印。违规抛清晰网络异常（此前被 tiny_http/ureq 静默丢弃，极难排查）。
fn 校验HTTP头(名: &str, 值: &str) -> Result<(), String> {
    let 名合法 = !中文头名到线格式(名).is_empty()
        || (!名.is_empty() && 名.bytes().all(|b| b.is_ascii_graphic() && b != b':'));
    let 值合法 = 值.bytes().all(|b| (0x20..=0x7e).contains(&b) || b == b'\t');
    if !名合法 {
        return Err(format!("HTTP 头名非法：「{}」（线格式头名须为 ASCII；可用语义键：位置/内容类型/缓存控制/内容编码；应用数据中文请放响应体或会话凭证）", 名));
    }
    if !值合法 {
        return Err(format!("HTTP 头值非法：头「{}」的值含非 ASCII 字符（线格式限制）。应用数据中文请放响应体或会话凭证", 名));
    }
    Ok(())
}

/// 发送前统一校验 `选项.请求头`。
/// ⚠ 必须在**发起请求之前**短路：`抛网络异常` 只是设置异常标志、不会中断当前函数，
///   若继续发送，非法头会被底层库拒绝，最终报出误导性的「Peer disconnected」，掩盖真实原因。
fn 校验请求头选项(选项值: &运行时值) -> Result<(), String> {
    for (k, v) in 取子映射文本(选项值, "请求头") {
        校验HTTP头(k.as_str(), v.as_str())?;
    }
    Ok(())
}

// ── URL 解析辅助 ──
fn 解析网址(url: &str) -> (String, u16, String) {
    let (无协议, 默认端口) = if url.starts_with("https://") {
        (&url[8..], 443u16)
    } else if url.starts_with("http://") {
        (&url[7..], 80u16)
    } else {
        (url, 80u16)
    };
    let slash = 无协议.find('/').unwrap_or(无协议.len());
    let hostpart = &无协议[..slash];
    let path = if slash < 无协议.len() { &无协议[slash..] } else { "/" };
    if let Some(colon) = hostpart.find(':') {
        let port = hostpart[colon+1..].parse::<u16>().unwrap_or(默认端口);
        (hostpart[..colon].to_string(), port, path.to_string())
    } else {
        (hostpart.to_string(), 默认端口, path.to_string())
    }
}

// ============================================================
//  网络分支分发（由 系统调用() 委托，索引 121..=154 / 180..=204）
// ============================================================
pub(crate) fn 处理网络(索引: usize, 参数: &[运行时值]) -> 运行时值 {
    match 索引 {
        // ═══════════════════════════════════════════════
        // 网络模块 (121-154) — 双模式统一实现，底层复用 Rust std::net + ureq + tiny_http
        // ═══════════════════════════════════════════════
        
        // ── HTTP 客户端 (121-123) — 返回网络响应结构体 ──
        标准库索引::网络_获取 => { // 网络.获取(网址, 选项)
            let 网址 = 编码网址中文路径(&取文本(参数, 0));
            if 网址.is_empty() {
                // 离线安全：空 URL 不发起请求，直接返回空响应
                return 构建网络响应结构体(0, String::new(), Vec::new(), Vec::new());
            }
            let 选项值 = 取参数(参数, 1); let 选项 = 取映射参数(参数, 1);
            if let Err(消息) = 校验请求头选项(&选项值) { 抛网络异常(消息); return 运行时值::空值(); }
            let agent = 构建HTTP代理(&选项);
            let req = 应用请求头(agent.get(&网址), &选项值, &构建Cookie串(&取子映射文本(&选项值, "会话凭证")));
            match req.call() {
                Ok(resp) => {
                    let 状态码 = resp.status().as_u16();
                    let (头条目, cookie条目) = 收集响应头与Cookie(&resp);
                    let 内容 = match resp.into_body().read_to_string() {
                        Ok(s) => s,
                        // 旧实现 unwrap_or_default：响应体读取失败（超限/连接中断/编码错误）时
                        // 静默返回空内容且状态码照常 200，调用方无从分辨。
                        Err(e) => { 抛网络异常(format!("HTTP响应体读取失败: {}", e)); return 运行时值::空值(); }
                    };
                    构建网络响应结构体(状态码, 内容, 头条目, cookie条目)
                }
                Err(e) => { 抛网络异常(format!("HTTP GET失败: {}", e)); 运行时值::空值() }
            }
        }
        标准库索引::网络_发送 => { // 网络.发送(网址, 数据, 选项)  — 文本直接请求体，映射自动转JSON
            let 网址 = 编码网址中文路径(&取文本(参数, 0));
            if 网址.is_empty() {
                return 构建网络响应结构体(0, String::new(), Vec::new(), Vec::new());
            }
            let 选项值 = 取参数(参数, 2); let 选项 = 取映射参数(参数, 2);
            if let Err(消息) = 校验请求头选项(&选项值) { 抛网络异常(消息); return 运行时值::空值(); }
            // 映射→JSON 自动转换（文档规范）
            let 数据文本 = if let Some(v) = 参数.get(1) {
                if v.标签 == 标签_句柄 {
                    if let Some(堆数据::映射(m)) = 堆读取(v.句柄) {
                        let mut obj = serde_json::Map::new();
                        for (k, val) in &m {
                            let key = match k { 堆键::文本(s) => s.clone(), _ => continue };
                            obj.insert(key, serde_json::Value::String(val.转文本()));
                        }
                        serde_json::Value::Object(obj).to_string()
                    } else { v.转文本() }
                } else { v.转文本() }
            } else { String::new() };
            let agent = 构建HTTP代理(&选项);
            let req = 应用请求头(agent.post(&网址), &选项值, &构建Cookie串(&取子映射文本(&选项值, "会话凭证")));
            match req.send(数据文本.as_str()) {
                Ok(resp) => {
                    let 状态码 = resp.status().as_u16();
                    let (头条目, cookie条目) = 收集响应头与Cookie(&resp);
                    let 内容 = match resp.into_body().read_to_string() {
                        Ok(s) => s,
                        // 旧实现 unwrap_or_default：响应体读取失败（超限/连接中断/编码错误）时
                        // 静默返回空内容且状态码照常 200，调用方无从分辨。
                        Err(e) => { 抛网络异常(format!("HTTP响应体读取失败: {}", e)); return 运行时值::空值(); }
                    };
                    构建网络响应结构体(状态码, 内容, 头条目, cookie条目)
                }
                Err(e) => { 抛网络异常(format!("HTTP POST失败: {}", e)); 运行时值::空值() }
            }
        }
        标准库索引::网络_请求 => { // 网络.请求(方法, 网址, 数据, 选项) — 数据映射自动转JSON，保留响应头
            let 方法 = 取文本(参数, 0); let 网址 = 编码网址中文路径(&取文本(参数, 1));
            if 网址.is_empty() {
                return 构建网络响应结构体(0, String::new(), Vec::new(), Vec::new());
            }
            let 选项值 = 取参数(参数, 3); let 选项 = 取映射参数(参数, 3);
            if let Err(消息) = 校验请求头选项(&选项值) { 抛网络异常(消息); return 运行时值::空值(); }
            let cookie串 = 构建Cookie串(&取子映射文本(&选项值, "会话凭证"));
            let 数据文本 = if let Some(v) = 参数.get(2) {
                if v.标签 == 标签_句柄 {
                    if let Some(堆数据::映射(m)) = 堆读取(v.句柄) {
                        let mut obj = serde_json::Map::new();
                        for (k, val) in &m {
                            let key = match k { 堆键::文本(s) => s.clone(), _ => continue };
                            obj.insert(key, serde_json::Value::String(val.转文本()));
                        }
                        serde_json::Value::Object(obj).to_string()
                    } else { v.转文本() }
                } else { v.转文本() }
            } else { String::new() };
            let agent = 构建HTTP代理(&选项);
            let call_result = match 方法.as_str() {
                "POST" => {
                    let req = 应用请求头(agent.post(&网址), &选项值, &cookie串);
                    req.send(数据文本.as_str())
                }
                "PUT" => {
                    let req = 应用请求头(agent.put(&网址), &选项值, &cookie串);
                    req.send(数据文本.as_str())
                }
                "DELETE" => {
                    let req = 应用请求头(agent.delete(&网址), &选项值, &cookie串);
                    if 数据文本.is_empty() { req.call() } else { req.force_send_body().send(数据文本.as_str()) }
                }
                _ => { // GET
                    let req = 应用请求头(agent.get(&网址), &选项值, &cookie串);
                    if 数据文本.is_empty() { req.call() } else { req.force_send_body().send(数据文本.as_str()) }
                }
            };
            match call_result {
                Ok(resp) => {
                    let 状态码 = resp.status().as_u16();
                    let (头条目, cookie条目) = 收集响应头与Cookie(&resp);
                    let 内容 = match resp.into_body().read_to_string() {
                        Ok(s) => s,
                        // 旧实现 unwrap_or_default：响应体读取失败（超限/连接中断/编码错误）时
                        // 静默返回空内容且状态码照常 200，调用方无从分辨。
                        Err(e) => { 抛网络异常(format!("HTTP响应体读取失败: {}", e)); return 运行时值::空值(); }
                    };
                    构建网络响应结构体(状态码, 内容, 头条目, cookie条目)
                }
                Err(e) => { 抛网络异常(format!("HTTP {}失败: {}", 方法, e)); 运行时值::空值() }
            }
        }

        // ── 流式下载 (124-126) ──
        标准库索引::网络_流式获取 => { // 网络.流式获取(网址, 选项) — 真 HTTP 流式下载（HTTP/1.0，连接关闭即结束体）
            let 网址 = 取文本(参数, 0); let 选项 = 取映射参数(参数, 1);
            let 超时 = 取选项整数(&选项, "超时", 30000);
            let (主机, 端口, 路径) = 解析网址(&网址);
            let mut stream = match TcpStream::connect(format!("{}:{}", 主机, 端口)) {
                Ok(s) => s,
                Err(e) => { 抛网络异常(format!("流式连接失败: {}", e)); return 运行时值::空值(); }
            };
            stream.set_read_timeout(Some(std::time::Duration::from_millis(超时 as u64))).ok();
            // 发送请求（先写后包读），https 走 TLS 握手
            // 请求行路径必须 percent-encode（HTTP/1.1 请求目标须 ASCII；中文路径裸发会被服务端拒绝致连接关闭）
            let 请求行 = format!("GET {} HTTP/1.0\r\nHost: {}\r\nAccept: */*\r\nConnection: close\r\n\r\n", 编码网址中文路径(&路径), 主机);
            let mut reader: Box<dyn BufRead + Send> = if 网址.starts_with("https://") {
                let 配置 = 构造客户端TLS配置(&选项);
                let 服务器名 = 解析服务器名(&主机);
                match ClientConnection::new(配置, 服务器名) {
                    Ok(conn) => {
                        let mut tls = StreamOwned::new(conn, stream);
                        let _ = tls.write_all(请求行.as_bytes());
                        Box::new(BufReader::new(tls))
                    }
                    Err(e) => { 抛网络异常(format!("TLS握手失败: {}", e)); return 运行时值::空值(); }
                }
            } else {
                let _ = stream.write_all(请求行.as_bytes()); Box::new(BufReader::new(stream))
            };
            // 读取状态行
            let mut 状态行 = String::new();
            if reader.read_line(&mut 状态行).unwrap_or(0) == 0 {
                抛网络异常("流式响应为空".into()); return 运行时值::空值();
            }
            let 状态码 = 状态行.split_whitespace().nth(1)
                .and_then(|s| s.parse::<u16>().ok()).unwrap_or(200);
            // 读取响应头直到空行
            let mut 响应头 = HashMap::new();
            loop {
                let mut line = String::new();
                if reader.read_line(&mut line).unwrap_or(0) == 0 { break; }
                let line = line.trim_end_matches(['\r','\n']).to_string();
                if line.is_empty() { break; }
                if let Some((k, v)) = line.split_once(':') {
                    响应头.insert(k.trim().to_string(), v.trim().to_string());
                }
            }
            let 资源 = 网络表.lock().unwrap().分配(网络槽位::流式响应 {
                reader: Arc::new(Mutex::new(reader)), 状态码,
                响应头,
                残余: Arc::new(Mutex::new(Vec::new())),
            });
            let 头克隆 = { let 表 = 网络表.lock().unwrap();
                if let 网络槽位::流式响应 { ref 响应头, .. } = *表.读取(资源) { 响应头.clone() } else { HashMap::new() }
            };
            let 头句柄 = 分配映射(文本映射转堆条目(&头克隆));
            分配结构体(流式响应类型索引, vec![
                运行时值::整数(资源 as i64),
                运行时值::整数(状态码 as i64),
                运行时值::句柄(头句柄.句柄),
            ])
        }
        标准库索引::网络_流式读取 => { // 流式响应.读取(字节数) — UTF-8 安全：多字节字符跨块不撕裂
            let 流句柄 = 取句柄(参数, 0);
            let 字节数 = 取整数(参数, 1) as usize;
            let 资源句柄 = 读字段资源句柄(流句柄);
            let 配对 = { let 表 = 网络表.lock().unwrap();
                match *表.读取(资源句柄) {
                    网络槽位::流式响应 { ref reader, ref 残余, .. } => Some((reader.clone(), 残余.clone())),
                    _ => None,
                }
            };
            if let Some((读arc, 残余arc)) = 配对 {
                let mut reader = 读arc.lock().unwrap();
                let mut 残余字节 = 残余arc.lock().unwrap().clone();
                let mut buf = vec![0u8; 字节数.max(1).min(65536)];
                loop {
                    match reader.read(&mut buf) {
                        Ok(0) => {
                            // EOF：冲刷残余（最后一次可能仍是不完整字节），下一次调用返回空串（EOF 信号）
                            let 冲 = String::from_utf8_lossy(&残余字节).into_owned();
                            残余字节.clear();
                            *残余arc.lock().unwrap() = 残余字节;
                            return 分配文本(冲);
                        }
                        Ok(n) => {
                            残余字节.extend_from_slice(&buf[..n]);
                            match std::str::from_utf8(&残余字节) {
                                Ok(s) => {
                                    let 文本 = s.to_string();
                                    残余字节.clear();
                                    *残余arc.lock().unwrap() = 残余字节;
                                    return 分配文本(文本);
                                }
                                Err(e) => {
                                    let 切 = e.valid_up_to();
                                    if 切 > 0 {
                                        // 解码最长合法 UTF-8 前缀，尾部不完整字节留待下一块
                                        let 文本 = String::from_utf8_lossy(&残余字节[..切]).into_owned();
                                        let 剩余 = 残余字节[切..].to_vec();
                                        *残余arc.lock().unwrap() = 剩余;
                                        return 分配文本(文本);
                                    }
                                    // 整块都是不完整多字节字符前缀 → 继续读
                                }
                            }
                        }
                        Err(e) if e.kind() == ErrorKind::TimedOut || e.kind() == ErrorKind::WouldBlock => {
                            *残余arc.lock().unwrap() = 残余字节;
                            抛网络异常("流式读取超时".into());
                            return 运行时值::空值();
                        }
                        Err(_) => {
                            // 读错误视作连接关闭：冲刷残余
                            let 冲 = String::from_utf8_lossy(&残余字节).into_owned();
                            残余字节.clear();
                            *残余arc.lock().unwrap() = 残余字节;
                            return 分配文本(冲);
                        }
                    }
                }
            } else { 运行时值::空值() }
        }
        标准库索引::网络_流式关闭 => { // 流式响应.关闭()
            let 流句柄 = 取句柄(参数, 0);
            let 资源句柄 = 读字段资源句柄(流句柄);
            网络表.lock().unwrap().释放(资源句柄);
            运行时值::空值()
        }

        // ── TCP 连接 (127-136) ──
        标准库索引::网络_连接 => { // 网络.连接(地址, 端口, 选项) — 支持 TLS/证书/密钥/验证证书
            let 地址 = 取文本(参数, 0); let 端口 = 取整数(参数, 1) as u16;
            let 选项 = 取映射参数(参数, 2);
            let 启用TLS = 取选项逻辑(&选项, "TLS");
            if 启用TLS {
                let 配置 = 构造客户端TLS配置(&选项);
                let 服务器名 = 解析服务器名(&地址);
                let addr = format!("{}:{}", 地址, 端口);
                let 连接超时 = 取选项整数(&选项, "超时", 30000);
                match 连接带超时(&addr, 连接超时) {
                    Ok(stream) => {
                        match ClientConnection::new(配置, 服务器名) {
                            Ok(conn) => {
                                let tls = StreamOwned::new(conn, stream);
                                let 底层 = Arc::new(tls.get_ref().try_clone().unwrap());
                                let 本地 = 底层.local_addr().unwrap();
                                let 远端 = 底层.peer_addr().unwrap();
                                let 资源 = 网络表.lock().unwrap().分配(网络槽位::Tcp连接TLS {
                                    stream: Arc::new(Mutex::new(Box::new(tls))),
                                    残余: Arc::new(Mutex::new(Vec::new())),
                                    底层,
                                    本地地址: 本地.ip().to_string(), 本地端口: 本地.port(),
                                    远端地址: 远端.ip().to_string(), 远端端口: 远端.port(),
                                });
                                return 分配结构体(网络连接类型索引, vec![
                                    运行时值::整数(资源 as i64),
                                    运行时值::句柄(分配文本(本地.ip().to_string()).句柄),
                                    运行时值::整数(本地.port() as i64),
                                    运行时值::句柄(分配文本(远端.ip().to_string()).句柄),
                                    运行时值::整数(远端.port() as i64),
                                ]);
                            }
                            Err(e) => { 抛网络异常(format!("TLS握手失败: {}", e)); return 运行时值::空值(); }
                        }
                    }
                    Err(e) => { 抛网络异常(format!("TCP连接失败: {}", e)); return 运行时值::空值(); }
                }
            }
            // 非TLS路径
            let 连接超时 = 取选项整数(&选项, "超时", 30000);
            match 连接带超时(&format!("{}:{}", 地址, 端口), 连接超时) {
                Ok(stream) => {
                    let 本地 = stream.local_addr().unwrap();
                    let 远端 = stream.peer_addr().unwrap();
                    let 资源 = 网络表.lock().unwrap().分配(网络槽位::Tcp连接 {
                        stream: Arc::new(Mutex::new(stream)),
                        残余: Arc::new(Mutex::new(Vec::new())),
                        本地地址: 本地.ip().to_string(), 本地端口: 本地.port(),
                        远端地址: 远端.ip().to_string(), 远端端口: 远端.port(),
                    });
                    分配结构体(网络连接类型索引, vec![
                        运行时值::整数(资源 as i64),
                        运行时值::句柄(分配文本(本地.ip().to_string()).句柄),
                        运行时值::整数(本地.port() as i64),
                        运行时值::句柄(分配文本(远端.ip().to_string()).句柄),
                        运行时值::整数(远端.port() as i64),
                    ])
                }
                Err(e) => { 抛网络异常(format!("TCP连接失败: {}", e)); 运行时值::空值() }
            }
        }
        标准库索引::网络_连接读取 => { // 连接.读取(字节数) — 支持明文/TLS；超时抛异常
            let 连接句柄 = 取句柄(参数, 0); let 字节数 = 取整数(参数, 1) as usize;
            let 资源句柄 = 读字段资源句柄(连接句柄);
            let result = tcp_读(资源句柄, &|reader| {
                // UTF-8 安全：数据块边界切断的多字节字符不再被替换成 U+FFFD。
                // 不完整的尾部字节回退到连接残余，留给下一次读；若整块只有一个不完整前缀则继续读
                // （不能返回空串——空串是「对端已关闭」的信号）。
                if 字节数 == 0 { return Some(String::new()); }
                let mut buf = vec![0u8; 字节数.min(65536)];
                let mut 累积: Vec<u8> = Vec::new();
                loop {
                    match reader.read(&mut buf) {
                        Ok(0) => return Some(String::from_utf8_lossy(&累积).into_owned()),
                        Ok(n) => {
                            累积.extend_from_slice(&buf[..n]);
                            match std::str::from_utf8(&累积) {
                                Ok(s) => return Some(s.to_string()),
                                // 真正非法的字节序列（不是被切断）：保持原有的替换字符语义
                                Err(e) if e.error_len().is_some() => return Some(String::from_utf8_lossy(&累积).into_owned()),
                                Err(e) => {
                                    let 切 = e.valid_up_to();
                                    if 切 > 0 {
                                        reader.回退(&累积[切..]);
                                        return Some(String::from_utf8_lossy(&累积[..切]).into_owned());
                                    }
                                }
                            }
                        }
                        Err(e) if e.kind() == ErrorKind::TimedOut || e.kind() == ErrorKind::WouldBlock => {
                            reader.回退(&累积);
                            抛网络异常("网络读取超时".into());
                            return None;
                        }
                        Err(e) if e.kind() == ErrorKind::Interrupted => continue,
                        Err(_) => return Some(String::from_utf8_lossy(&累积).into_owned()),
                    }
                }
            });
            分配文本(result.unwrap_or_default())
        }
        标准库索引::网络_连接读取指定 => { // 连接.读取指定(字节数)；超时抛异常，连接关闭返回已读部分
            let 连接句柄 = 取句柄(参数, 0); let 字节数 = 取整数(参数, 1) as usize;
            let 资源句柄 = 读字段资源句柄(连接句柄);
            let result = tcp_读(资源句柄, &|reader| {
                // ⚠ 规范 §5.3.1：「不足时阻塞等待直到读满或连接关闭。连接关闭时返回**已读取的部分**」
                //   ⇒ 不能用 read_exact（EOF 时整个 buf 仍是未填充的 0 字节，会把 0 当成数据返回）。
                let mut buf = vec![0u8; 字节数];
                let mut 已读 = 0usize;
                while 已读 < 字节数 {
                    match reader.read(&mut buf[已读..]) {
                        Ok(0) => break,   // 对端关闭 ⇒ 返回已读取的部分
                        Ok(n) => 已读 += n,
                        Err(e) if e.kind() == ErrorKind::TimedOut || e.kind() == ErrorKind::WouldBlock => {
                            抛网络异常("网络读取超时".into());
                            return None;
                        }
                        Err(e) if e.kind() == ErrorKind::Interrupted => continue,
                        Err(_) => break,  // 其它错误视作连接关闭 ⇒ 返回已读取的部分
                    }
                }
                Some(String::from_utf8_lossy(&buf[..已读]).to_string())
            });
            分配文本(result.unwrap_or_default())
        }
        标准库索引::网络_连接读取一行 => { // 连接.读取一行(分隔符)；超时抛异常
            let 连接句柄 = 取句柄(参数, 0);
            let 分隔符 = if 参数.len() > 1 { 取文本(参数, 1) } else { "\n".to_string() };
            let 资源句柄 = 读字段资源句柄(连接句柄);
            let result = tcp_读(资源句柄, &|reader| {
                let sep = 分隔符.as_bytes(); let mut r = Vec::new(); let mut b = [0u8; 1];
                loop {
                    match reader.read(&mut b) {
                        Ok(0) => break,
                        Ok(1) => { r.push(b[0]); if r.len()>=sep.len() && &r[r.len()-sep.len()..]==sep { r.truncate(r.len()-sep.len()); break; } }
                        Err(e) if e.kind() == ErrorKind::TimedOut || e.kind() == ErrorKind::WouldBlock => { 抛网络异常("网络读取超时".into()); return None; }
                        Err(_) => break,
                        _ => {}
                    }
                }
                Some(String::from_utf8_lossy(&r).to_string())
            });
            分配文本(result.unwrap_or_default())
        }
        标准库索引::网络_连接读取全部 => { // 连接.读取全部()；超时抛异常
            let 连接句柄 = 取句柄(参数, 0); let 资源句柄 = 读字段资源句柄(连接句柄);
            let result = tcp_读(资源句柄, &|reader| {
                let mut buf = String::new();
                match reader.read_to_string(&mut buf) {
                    Ok(_) => Some(buf),
                    Err(e) if e.kind() == ErrorKind::TimedOut || e.kind() == ErrorKind::WouldBlock => { 抛网络异常("网络读取超时".into()); None }
                    Err(_) => Some(buf),
                }
            });
            分配文本(result.unwrap_or_default())
        }
        标准库索引::网络_连接写入 => { // 连接.写入(数据)
            let 连接句柄 = 取句柄(参数, 0); let 数据 = 取文本(参数, 1);
            let 资源句柄 = 读字段资源句柄(连接句柄);
            if !tcp_写(资源句柄, &|writer| {
                if let Err(e) = writer.write_all(数据.as_bytes()) {
                    抛网络异常(format!("TCP写入失败: {}", e));
                }
            }) { 抛网络异常("TCP连接已关闭".into()); }
            运行时值::空值()
        }
        标准库索引::网络_连接写入一行 => { // 连接.写入一行(数据)
            let 连接句柄 = 取句柄(参数, 0); let 数据 = 取文本(参数, 1);
            let 资源句柄 = 读字段资源句柄(连接句柄);
            let line = format!("{}\n", 数据);
            if !tcp_写(资源句柄, &|writer| {
                if let Err(e) = writer.write_all(line.as_bytes()) {
                    抛网络异常(format!("TCP写入一行失败: {}", e));
                }
            }) { 抛网络异常("TCP连接已关闭".into()); }
            运行时值::空值()
        }
        标准库索引::网络_连接设置超时 => { // 连接.设置超时(毫秒), 0=无超时
            let 连接句柄 = 取句柄(参数, 0); let 毫秒 = 取整数(参数, 1);
            let 资源句柄 = 读字段资源句柄(连接句柄);
            let dur = if 毫秒 > 0 { Some(std::time::Duration::from_millis(毫秒 as u64)) } else { None };
            tcp_设置选项(资源句柄, &|s| { s.set_read_timeout(dur).ok(); s.set_write_timeout(dur).ok(); });
            运行时值::空值()
        }
        标准库索引::网络_连接关闭写入 => { // 连接.关闭写入() — 支持 TLS
            let 连接句柄 = 取句柄(参数, 0); let 资源句柄 = 读字段资源句柄(连接句柄);
            tcp_设置选项(资源句柄, &|s| { s.shutdown(std::net::Shutdown::Write).ok(); });
            运行时值::空值()
        }
        标准库索引::网络_连接关闭 => { // 连接.关闭()
            let 连接句柄 = 取句柄(参数, 0);
            网络表.lock().unwrap().释放(读字段资源句柄(连接句柄));
            运行时值::空值()
        }

        // ── TCP 监听器 (137-139) ──
        标准库索引::网络_监听 => { // 网络.监听(端口, 选项) — 支持 TLS/证书/密钥
            let 端口 = 取整数(参数, 0) as u16;
            let 选项 = 取映射参数(参数, 1);
            let 启用TLS = 取选项逻辑(&选项, "TLS");
            if 启用TLS {
                let 证书路径 = 取选项文本(&选项, "证书").unwrap_or_default();
                let 密钥路径 = 取选项文本(&选项, "密钥").unwrap_or_default();
                match 加载服务端TLS配置(&证书路径, &密钥路径) {
                    Some(config) => {
                        match TcpListener::bind(format!("0.0.0.0:{}", 端口)) {
                            Ok(listener) => {
                                // 端口 0 时由系统分配：本地端口 必须返回实际绑定端口（与 数据报 一致，规范「监听器绑定的端口号」）
                                let 实际端口 = listener.local_addr().map(|a| a.port()).unwrap_or(端口);
                                let 资源 = 网络表.lock().unwrap().分配(网络槽位::Tcp监听器TLS {
                                    config: config.clone(), listener: Arc::new(Mutex::new(listener)),
                                });
                                return 分配结构体(网络监听器类型索引, vec![运行时值::整数(资源 as i64), 运行时值::整数(实际端口 as i64)]);
                            }
                            Err(e) => { 抛网络异常(format!("TLS监听绑定失败: {}", e)); return 运行时值::空值(); }
                        }
                    }
                    None => { 抛网络异常("证书/密钥加载或解析失败（须为未加密 PEM）".into()); return 运行时值::空值(); }
                }
            }
            // 非TLS路径
            match TcpListener::bind(format!("0.0.0.0:{}", 端口)) {
                Ok(listener) => {
                    // 端口 0 时由系统分配：本地端口 必须返回实际绑定端口（与 数据报 一致）
                    let 实际端口 = listener.local_addr().map(|a| a.port()).unwrap_or(端口);
                    let 资源 = 网络表.lock().unwrap().分配(网络槽位::Tcp监听器(Arc::new(Mutex::new(listener))));
                    分配结构体(网络监听器类型索引, vec![运行时值::整数(资源 as i64), 运行时值::整数(实际端口 as i64)])
                }
                Err(e) => { 抛网络异常(format!("TCP监听失败: {}", e)); 运行时值::空值() }
            }
        }
        标准库索引::网络_监听接受 => { // 监听器.接受() — 支持 TLS
            let 监听句柄 = 取句柄(参数, 0); let 资源句柄 = 读字段资源句柄(监听句柄);
            let arc_result: Option<(Arc<Mutex<TcpListener>>, Option<Arc<ServerConfig>>)> = {
                let 表 = 网络表.lock().unwrap();
                match 表.读取(资源句柄) {
                    网络槽位::Tcp监听器(ref l) => Some((l.clone(), None)),
                    网络槽位::Tcp监听器TLS { ref config, ref listener } => Some((listener.clone(), Some(config.clone()))),
                    _ => None,
                }
            };
            if let Some((arc, tls)) = arc_result {
                match arc.lock().unwrap().accept() {
                    Ok((stream, peer)) => {
                        let 本地 = stream.local_addr().unwrap();
                        if let Some(config) = tls {
                            // TLS 握手
                            match ServerConnection::new(config.clone()) {
                                Ok(conn) => {
                                    let tls_stream = StreamOwned::new(conn, stream);
                                    let 底层 = Arc::new(tls_stream.get_ref().try_clone().unwrap());
                                    let tls_本地端口 = 底层.local_addr().map(|a| a.port()).unwrap_or(本地.port());
                                    let 资源 = 网络表.lock().unwrap().分配(网络槽位::Tcp连接TLS {
                                        stream: Arc::new(Mutex::new(Box::new(tls_stream))),
                                        残余: Arc::new(Mutex::new(Vec::new())),
                                        底层,
                                        本地地址: 本地.ip().to_string(), 本地端口: tls_本地端口,
                                        远端地址: peer.ip().to_string(), 远端端口: peer.port(),
                                    });
                                    return 分配结构体(网络连接类型索引, vec![
                                        运行时值::整数(资源 as i64),
                                        运行时值::句柄(分配文本(本地.ip().to_string()).句柄),
                                        运行时值::整数(本地.port() as i64),
                                        运行时值::句柄(分配文本(peer.ip().to_string()).句柄),
                                        运行时值::整数(peer.port() as i64),
                                    ]);
                                }
                                Err(e) => { 抛网络异常(format!("TLS握手失败: {}", e)); return 运行时值::空值(); }
                            }
                        }
                        let 资源 = 网络表.lock().unwrap().分配(网络槽位::Tcp连接 {
                            stream: Arc::new(Mutex::new(stream)),
                            残余: Arc::new(Mutex::new(Vec::new())),
                            本地地址: 本地.ip().to_string(), 本地端口: 本地.port(),
                            远端地址: peer.ip().to_string(), 远端端口: peer.port(),
                        });
                        分配结构体(网络连接类型索引, vec![
                            运行时值::整数(资源 as i64),
                            运行时值::句柄(分配文本(本地.ip().to_string()).句柄),
                            运行时值::整数(本地.port() as i64),
                            运行时值::句柄(分配文本(peer.ip().to_string()).句柄),
                            运行时值::整数(peer.port() as i64),
                        ])
                    }
                    Err(e) => { 抛网络异常(format!("接受连接失败: {}", e)); 运行时值::空值() }
                }
            } else {
                // ⚠ 规范 §5.3.1：「`网络监听器.接受()` 阻塞等待客户端连接…**失败时抛出异常**」
                //   槽位缺失 = 监听器已被关闭/无效 ⇒ 必须抛异常，不能静默返回空值
                //   （静默返回空值会让调用方拿到空值后继续调用方法，错误被推迟且难以定位）。
                抛网络异常("接受连接失败: 监听器已关闭或无效".into());
                运行时值::空值()
            }
        }
        标准库索引::网络_监听关闭 => { // 监听器.关闭()
            let 监听句柄 = 取句柄(参数, 0);
            网络表.lock().unwrap().释放(读字段资源句柄(监听句柄));
            运行时值::空值()
        }

        // ── UDP 数据报 (140-146) ──
        标准库索引::网络_数据报 => { // 网络.数据报(端口)
            let 端口 = 取整数(参数, 0) as u16;
            match UdpSocket::bind(format!("0.0.0.0:{}", 端口)) {
                Ok(socket) => {
                    let 实际端口 = socket.local_addr().map(|a| a.port()).unwrap_or(端口);
                    let 资源 = 网络表.lock().unwrap().分配(网络槽位::UdpSocket(Arc::new(Mutex::new(socket))));
                    分配结构体(网络数据报类型索引, vec![运行时值::整数(资源 as i64), 运行时值::整数(实际端口 as i64)])
                }
                Err(e) => {
                    // 🆕 方案C：UDP 离线/沙箱环境优雅诊断。
                    // 沙箱禁回环 UDP 时绑定失败，异常信息已清晰含端口与底层错误（与 TCP 离线门禁同构），
                    // 调用方 捕获: 后可读到明确报告，避免「不打印」的静默失败。
                    抛网络异常(format!("UDP创建失败（离线环境网络错误清晰报告）：端口 {} {}", 端口, e));
                    运行时值::空值()
                }
            }
        }
        标准库索引::网络_数据报发送 => { // 数据报.发送(数据, 地址, 端口)
            let 数据报句柄 = 取句柄(参数, 0); let 数据 = 取文本(参数, 1);
            let 地址 = 取文本(参数, 2); let 端口 = 取整数(参数, 3) as u16;
            let 资源句柄 = 读字段资源句柄(数据报句柄);
            let arc = { let 表 = 网络表.lock().unwrap();
                if let 网络槽位::UdpSocket(ref s) = *表.读取(资源句柄) { Some(s.clone()) } else { None }
            };
            if let Some(arc) = arc {
                if let Err(e) = arc.lock().unwrap().send_to(数据.as_bytes(), format!("{}:{}", 地址, 端口)) {
                    抛网络异常(format!("UDP发送失败: {}", e));
                }
            }
            运行时值::空值()
        }
        标准库索引::网络_数据报接收 => { // 数据报.接收(超时毫秒)
            let 数据报句柄 = 取句柄(参数, 0); let 超时 = 取整数(参数, 1);
            let 资源句柄 = 读字段资源句柄(数据报句柄);
            let arc = { let 表 = 网络表.lock().unwrap();
                if let 网络槽位::UdpSocket(ref s) = *表.读取(资源句柄) { Some(s.clone()) } else { None }
            };
            if let Some(arc) = arc {
                let socket = arc.lock().unwrap();
                socket.set_read_timeout(Some(std::time::Duration::from_millis(超时 as u64))).ok();
                let mut buf = [0u8; 65536];
                match socket.recv_from(&mut buf) {
                    Ok((n, src)) => {
                        let 数据文本 = String::from_utf8_lossy(&buf[..n]).to_string();
                        分配结构体(数据报消息类型索引, vec![
                            运行时值::句柄(分配文本(数据文本).句柄),
                            运行时值::句柄(分配文本(src.ip().to_string()).句柄),
                            运行时值::整数(src.port() as i64),
                        ])
                    }
                    Err(e) if e.kind() == std::io::ErrorKind::WouldBlock
                            || e.kind() == std::io::ErrorKind::TimedOut => 运行时值::空值(),
                    Err(e) => { 抛网络异常(format!("UDP接收失败: {}", e)); 运行时值::空值() }
                }
            } else { 运行时值::空值() }
        }
        标准库索引::网络_数据报启用广播 => { // 数据报.启用广播()
            let 数据报句柄 = 取句柄(参数, 0); let 资源句柄 = 读字段资源句柄(数据报句柄);
            let arc = { let 表 = 网络表.lock().unwrap();
                if let 网络槽位::UdpSocket(ref s) = *表.读取(资源句柄) { Some(s.clone()) } else { None }
            };
            if let Some(arc) = arc { arc.lock().unwrap().set_broadcast(true).ok(); }
            运行时值::空值()
        }
        标准库索引::网络_数据报加入组播 => { // 数据报.加入组播组(组地址)
            let 数据报句柄 = 取句柄(参数, 0); let 组 = 取文本(参数, 1);
            let 资源句柄 = 读字段资源句柄(数据报句柄);
            let arc = { let 表 = 网络表.lock().unwrap();
                if let 网络槽位::UdpSocket(ref s) = *表.读取(资源句柄) { Some(s.clone()) } else { None }
            };
            if let Some(arc) = arc {
                if let Ok(addr) = 组.parse::<std::net::Ipv4Addr>() {
                    arc.lock().unwrap().join_multicast_v4(&addr, &std::net::Ipv4Addr::UNSPECIFIED).ok();
                }
            }
            运行时值::空值()
        }
        标准库索引::网络_数据报离开组播 => { // 数据报.离开组播组(组地址)
            let 数据报句柄 = 取句柄(参数, 0); let 组 = 取文本(参数, 1);
            let 资源句柄 = 读字段资源句柄(数据报句柄);
            let arc = { let 表 = 网络表.lock().unwrap();
                if let 网络槽位::UdpSocket(ref s) = *表.读取(资源句柄) { Some(s.clone()) } else { None }
            };
            if let Some(arc) = arc {
                if let Ok(addr) = 组.parse::<std::net::Ipv4Addr>() {
                    arc.lock().unwrap().leave_multicast_v4(&addr, &std::net::Ipv4Addr::UNSPECIFIED).ok();
                }
            }
            运行时值::空值()
        }
        标准库索引::网络_数据报关闭 => { // 数据报.关闭()
            let 数据报句柄 = 取句柄(参数, 0);
            网络表.lock().unwrap().释放(读字段资源句柄(数据报句柄));
            运行时值::空值()
        }

        // ── HTTP 服务端 (147-154) ──
        标准库索引::网络_启动服务 => { // 网络.启动服务(端口, 处理器, 选项) —— ⚡统一实现⚡（2026-08-19 阶段二）
            let 端口 = 取整数(参数, 0) as u16;
            // 处理器：统一解码为 函数索引（标签_函数指针 负编码 / 闭包环境 句柄），双模式一致。
            // 编译模式调用点恒发 标签_函数指针（翻译执行器 函数引用 分支，阶段二已删 thunk 替换）；
            // 解释模式 VM 加载 函数引用 亦负编码。后台线程经 执行后台用户函数 调度执行处理器。
            let 处理器索引 = match 参数.get(1).and_then(|v| v.取函数索引()) {
                Some(索引) => 索引,
                None => {
                    抛网络异常("网络.启动服务 处理器参数不是函数".into());
                    return 运行时值::空值();
                }
            };
            let 选项 = 取映射参数(参数, 2);
            // TLS 配置：由基础功能侧 rustls 0.23 统一加载（单一真相源），注入 tiny_http 网络模块
            let tls配置: Option<Arc<ServerConfig>> = if 取选项逻辑(&选项, "TLS") {
                // 规范 §5.3.5 键为 证书/密钥；证书路径/密钥路径 为旧写法兼容回退
                let 证书 = 取选项文本(&选项, "证书").or_else(|| 取选项文本(&选项, "证书路径")).unwrap_or_default();
                let 密钥 = 取选项文本(&选项, "密钥").or_else(|| 取选项文本(&选项, "密钥路径")).unwrap_or_default();
                加载服务端TLS配置(&证书, &密钥)
            } else { None };
            if 取选项逻辑(&选项, "TLS") && tls配置.is_none() {
                抛网络异常("HTTP服务端TLS启动失败：证书/密钥加载错误（需未加密 PEM）".into());
                return 运行时值::空值();
            }
            // 绑定在调用线程同步完成：旧实现放在后台线程里且忽略失败，端口被占用时
            // 启动服务 照样返回一个「成功」的服务对象，之后所有请求都连不上且没有任何报错。
            let 启动结果 = match &tls配置 {
                Some(cfg) => crate::tiny_http::Server::https(format!("0.0.0.0:{}", 端口), cfg.clone()),
                None => crate::tiny_http::Server::http(format!("0.0.0.0:{}", 端口)),
            };
            let server = match 启动结果 {
                Ok(s) => Arc::new(s),
                Err(e) => {
                    抛网络异常(format!("HTTP服务启动失败（端口 {}）: {}", 端口, e));
                    return 运行时值::空值();
                }
            };
            // 端口 0 由系统分配：返回实际绑定端口（与 监听/数据报 一致）
            let 端口 = server.server_addr().to_ip().map(|a| a.port()).unwrap_or(端口);
            let 停止 = Arc::new(std::sync::atomic::AtomicBool::new(false));
            let 完成 = Arc::new(std::sync::atomic::AtomicBool::new(false));
            let 线程标识 = Arc::new(std::sync::OnceLock::new());
            let 资源 = 网络表.lock().unwrap().分配(网络槽位::网络服务 {
                端口, 服务: server.clone(), 停止: 停止.clone(), 完成: 完成.clone(), 线程标识: 线程标识.clone(),
            });
            std::thread::spawn(move || {
                let _ = 线程标识.set(std::thread::current().id());
                {
                    // 服务关闭 置位 停止 并 unblock；带超时轮询是兜底（unblock 只唤醒一次）。
                    loop {
                        if 停止.load(std::sync::atomic::Ordering::SeqCst) { break; }
                        let mut req = match server.recv_timeout(std::time::Duration::from_millis(200)) {
                            Ok(Some(r)) => r,
                            Ok(None) => continue,
                            Err(_) => break,
                        };
                        let mut 请求头 = HashMap::new();
                        for h in req.headers() {
                            let 头名 = h.field.to_string();
                            let 头值 = if 头名.eq_ignore_ascii_case("cookie") {
                                // Cookie 头中的凭证名/值是 percent-encoding 的（客户端 构建Cookie串 编码），解码还原纯中文
                                解码Cookie值(h.value.as_str())
                            } else { h.value.to_string() };
                            请求头.insert(头名, 头值);
                        }
                        let url = req.url().to_string();
                        let 路径 = url.find('?').map(|p| url[..p].to_string()).unwrap_or_else(|| url.clone());
                        let mut 内容 = String::new();
                        let _ = req.as_reader().read_to_string(&mut 内容);
                        // 组装 请求 结构（响应通道字段已删除：统一实现直接 respond，不走队列）
                        // 路径/完整网址 解码：客户端可能发送 percent-encoded 的中文路径（编码网址中文路径），
                        // 此处还原为原始 UTF-8 路径，使 请求.路径() 与源码中写的中文路径一致。
                        let 请求 = Http待处理请求 {
                            方法: req.method().to_string(), 路径: 解码网址路径(&路径), 请求头: 请求头.clone(),
                            内容: 内容.clone(), 完整网址: 解码网址路径(&url), 原始网址: url.clone(),
                            客户端地址: req.remote_addr().map(|a| a.ip().to_string()).unwrap_or_default(),
                        };
                        // 构造 网络请求 运行时值（base 全局堆）并同步调用处理器（后台线程）
                        let 请求值 = HTTP构造网络请求(&请求);
                        let mut 响应结果 = 运行时值::空值();
                        crate::外部调用注册::执行后台用户函数(
                            处理器索引, &请求值 as *const 运行时值, 1, &mut 响应结果 as *mut 运行时值);
                        // 提取响应字段（非句柄 → 500 默认响应，处理器异常安全降级）
                        let (状态码, 响应内容, 响应头, 凭证条目) = HTTP提取响应字段(&响应结果);
                        use crate::tiny_http::{Response, Header, StatusCode};
                        let mut r = Response::from_string(&响应内容)
                            .with_status_code(StatusCode::from(状态码));
                        for (k, v) in &响应头 {
                            // 智语言中文语义键 → 线格式 ASCII 头名（RFC 9110：Header Name 须 ASCII）
                            let 线头名 = 中文头名到线格式(k);
                            let 线头名 = if 线头名.is_empty() { k.as_str() } else { 线头名 };
                            // 「位置」的值是 URL，允许写中文（用户视角）⇒ 线格式按 RFC 3986 percent-encode，
                            // 客户端收集响应头时对称解码还原（否则 Header::from_bytes 失败会**静默丢弃**该头，
                            // 客户端跟随重定向时报误导性的 "missing a location header"）。
                            let 线值 = if 线头名.eq_ignore_ascii_case("Location") {
                                编码网址中文路径(v)
                            } else { v.clone() };
                            // 纵深防御：线格式头值含 CR/LF/NUL 一律丢弃（响应拆分），无论它来自哪个构造入口。
                            if 线值.bytes().any(|b| b == b'\r' || b == b'\n' || b == 0) {
                                eprintln!("[网络服务] 已丢弃含控制字符的响应头：{}", 线头名);
                                continue;
                            }
                            match Header::from_bytes(线头名.as_bytes(), 线值.as_bytes()) {
                                Ok(hdr) => { r = r.with_header(hdr); }
                                // 这里是服务后台线程：不能 抛网络异常（线程局部异常标志无人清除，
                                // 会残留并污染之后所有请求的异常语义），只记录并跳过该头。
                                Err(_) => { eprintln!("[网络服务] 响应头写回失败：{}", 线头名); }
                            }
                        }
                        // 会话凭证 → 线格式 Set-Cookie（§5.3.4 设置会话凭证 闭环）；
                        // 名/值 percent-encode 以过 ASCII 头校验，客户端 set-cookie 收集处对称解码
                        for (名, 值) in &凭证条目 {
                            let 凭证行 = format!("{}={}", 编码凭证分量(名), 编码凭证分量(值));
                            if let Ok(hdr) = Header::from_bytes("Set-Cookie".as_bytes(), 凭证行.as_bytes()) { r = r.with_header(hdr); }
                        }
                        req.respond(r).ok();
                    }
                }
                // 最后一个 Arc 在此释放 ⇒ Server::drop 关闭监听套接字，端口归还系统
                drop(server);
                完成.store(true, std::sync::atomic::Ordering::SeqCst);
            });
            return 分配结构体(网络服务类型索引, vec![运行时值::整数(资源 as i64), 运行时值::整数(端口 as i64)]);
        }
        标准库索引::网络_服务关闭 => { // 网络服务.关闭()
            // 旧实现只清句柄表槽位，后台线程与监听套接字永远存活：端口不释放、请求照常被处理。
            let 服务句柄 = 取句柄(参数, 0);
            let 资源句柄 = 读字段资源句柄(服务句柄);
            let 控制 = {
                let mut 表 = 网络表.lock().unwrap();
                let 取 = match 表.读取(资源句柄) {
                    网络槽位::网络服务 { 服务, 停止, 完成, 线程标识, .. } =>
                        Some((服务.clone(), 停止.clone(), 完成.clone(), 线程标识.clone())),
                    _ => None,
                };
                表.释放(资源句柄);
                取
            };
            if let Some((服务, 停止, 完成, 线程标识)) = 控制 {
                停止.store(true, std::sync::atomic::Ordering::SeqCst);
                服务.unblock();
                drop(服务);
                // 等后台线程退出（端口真正释放）再返回，便于「关闭后立刻在同端口重启」。
                // 在处理器内部关闭自身服务时不能等（会等自己），由线程处理完当前请求后自行退出。
                let 自身线程 = 线程标识.get().map(|t| *t == std::thread::current().id()).unwrap_or(false);
                if !自身线程 {
                    let 截止 = std::time::Instant::now() + std::time::Duration::from_secs(2);
                    while !完成.load(std::sync::atomic::Ordering::SeqCst) && std::time::Instant::now() < 截止 {
                        std::thread::sleep(std::time::Duration::from_millis(2));
                    }
                }
            }
            运行时值::空值()
        }
        标准库索引::网络_响应成功 => { // 网络.响应成功(内容, 响应头={})
            let 内容 = 取文本(参数, 0);
            let 头条目 = if 参数.len() > 1 { 取映射参数(参数, 1) } else { HashMap::new() };
            for (k, v) in &头条目 { if let Err(消息) = 校验HTTP头(k, v) { 抛网络异常(消息); } }
            分配结构体(网络响应类型索引, vec![
                运行时值::整数(200),
                运行时值::句柄(分配文本(内容).句柄),
                运行时值::句柄(分配映射(文本映射转堆条目(&头条目)).句柄),
                运行时值::句柄(分配映射(Vec::new()).句柄),
            ])
        }
        标准库索引::网络_响应创建 => { // 网络.响应创建(状态码, 内容, 响应头={})
            let 状态码 = 取整数(参数, 0); let 内容 = 取文本(参数, 1);
            let 头条目 = if 参数.len() > 2 { 取映射参数(参数, 2) } else { HashMap::new() };
            for (k, v) in &头条目 { if let Err(消息) = 校验HTTP头(k, v) { 抛网络异常(消息); } }
            分配结构体(网络响应类型索引, vec![
                运行时值::整数(状态码), 运行时值::句柄(分配文本(内容).句柄),
                运行时值::句柄(分配映射(文本映射转堆条目(&头条目)).句柄),
                运行时值::句柄(分配映射(Vec::new()).句柄),
            ])
        }
        标准库索引::网络_响应未找到 => { // 网络.响应未找到()
            分配结构体(网络响应类型索引, vec![
                运行时值::整数(404), 运行时值::句柄(分配文本("未找到".into()).句柄),
                运行时值::句柄(分配映射(Vec::new()).句柄), 运行时值::句柄(分配映射(Vec::new()).句柄),
            ])
        }
        标准库索引::网络_响应错误 => { // 网络.响应错误(消息)
            let 消息 = 取文本(参数, 0);
            分配结构体(网络响应类型索引, vec![
                运行时值::整数(500), 运行时值::句柄(分配文本(消息).句柄),
                运行时值::句柄(分配映射(Vec::new()).句柄), 运行时值::句柄(分配映射(Vec::new()).句柄),
            ])
        }
        标准库索引::网络_响应重定向 => { // 网络.响应重定向(目标网址)
            let 目标 = 取文本(参数, 0);
            // 目标含控制字符（CR/LF/NUL 等）即拒绝：否则经 Location 头原样上线，构成 HTTP 响应拆分
            // （可注入任意响应头，如 Set-Cookie）。中文等非 ASCII 字符合法，发送时会做百分号编码。
            if 目标.chars().any(|c| c.is_control()) {
                抛网络异常("响应重定向的目标网址含控制字符（回车/换行等），已拒绝".to_string());
                return 运行时值::空值();
            }
            // 智语言 API 语义键为中文「位置」；发送时由 147 映射为线格式 "Location"（ASCII 标准），
            // 客户端接收时由 收集响应头与Cookie 再映射回「位置」。中文头名不能直接上线（RFC 9110）。
            let 头条目 = vec![(堆键::文本("位置".into()), 分配文本(目标))];
            分配结构体(网络响应类型索引, vec![
                运行时值::整数(302), 运行时值::句柄(分配文本(String::new()).句柄),
                运行时值::句柄(分配映射(头条目).句柄), 运行时值::句柄(分配映射(Vec::new()).句柄),
            ])
        }
        标准库索引::网络_设置会话凭证 => { // 网络.设置会话凭证(名称, 值, 选项)
            let 名称 = 取文本(参数, 0); let 值 = 取文本(参数, 1);
            let 条目 = vec![(堆键::文本(名称), 分配文本(值))];
            分配结构体(网络响应类型索引, vec![
                运行时值::整数(200), 运行时值::句柄(分配文本(String::new()).句柄),
                运行时值::句柄(分配映射(Vec::new()).句柄), 运行时值::句柄(分配映射(条目).句柄),
            ])
        }
        // ═══════════════════════════════════════════════
        // 网络结构体访问器 (180-204)
        // ═══════════════════════════════════════════════

        // ── 网络连接查询 (180-184) ──
        标准库索引::网络连接_本地地址 => { // 网络连接.本地地址()
            let h = 读字段句柄(取句柄(参数,0), 1);
            match 堆读取文本字符串(h) { Some(s) => 分配文本(s), None => 分配文本(String::new()) }
        }
        标准库索引::网络连接_本地端口 => 运行时值::整数(读字段整数(取句柄(参数,0), 2)),    // 网络连接.本地端口()
        标准库索引::网络连接_远端地址 => { // 网络连接.远端地址()
            let h = 读字段句柄(取句柄(参数,0), 3);
            match 堆读取文本字符串(h) { Some(s) => 分配文本(s), None => 分配文本(String::new()) }
        }
        标准库索引::网络连接_远端端口 => 运行时值::整数(读字段整数(取句柄(参数,0), 4)),    // 网络连接.远端端口()
        标准库索引::网络_连接设置无延迟 => { // 网络连接.设置无延迟(逻辑) — 支持 TLS
            let 连接句柄 = 取句柄(参数, 0); let 启用 = 取整数(参数, 1) != 0;
            let 资源句柄 = 读字段资源句柄(连接句柄);
            tcp_设置选项(资源句柄, &|s| { s.set_nodelay(启用).ok(); });
            运行时值::空值()
        }

        // ── 网络监听器查询 (185) ──
        标准库索引::网络监听器_本地端口 => 运行时值::整数(读字段整数(取句柄(参数,0), 1)),    // 网络监听器.本地端口()

        // ── UDP 数据报查询 (186-188) ──
        标准库索引::网络数据报_连接 => { // 网络数据报.连接(地址, 端口)
            let 数据报句柄 = 取句柄(参数, 0); let 地址 = 取文本(参数, 1); let 端口 = 取整数(参数, 2) as u16;
            let 资源句柄 = 读字段资源句柄(数据报句柄);
            let arc = { let 表 = 网络表.lock().unwrap();
                if let 网络槽位::UdpSocket(ref s) = *表.读取(资源句柄) { Some(s.clone()) } else { None }
            };
            if let Some(arc) = arc { arc.lock().unwrap().connect(format!("{}:{}", 地址, 端口)).ok(); }
            运行时值::空值()
        }
        标准库索引::网络数据报_断开连接 => { // 网络数据报.断开连接()
            // UDP 标准库无原生"断开"语义；保活原 socket，避免重新绑定导致端口丢失/状态破坏。
            // 文档"恢复到无连接状态"以最佳努力实现（不破坏既有连接）。
            let _数据报句柄 = 取句柄(参数, 0);
            运行时值::空值()
        }
        标准库索引::网络数据报_本地端口 => 运行时值::整数(读字段整数(取句柄(参数,0), 1)),    // 网络数据报.本地端口()

        // ── 流式响应查询 (189-190) ──
        标准库索引::流式响应_状态码 => 运行时值::整数(读字段整数(取句柄(参数,0), 1)),    // 流式响应.状态码()
        标准库索引::流式响应_响应头 => { // 流式响应.响应头()
            let h = 读字段句柄(取句柄(参数,0), 2);
            match 堆读取(h) { Some(堆数据::映射(m)) => 分配映射(m), _ => 分配映射(Vec::new()) }
        }

        // ── 网络响应查询 (191-197) ──
        标准库索引::网络响应_状态码 => 运行时值::整数(读字段整数(取句柄(参数,0), 0)),    // 网络响应.状态码()
        标准库索引::网络响应_内容 => 分配文本(读字段文本(取句柄(参数,0), 1)),          // 网络响应.内容()
        标准库索引::网络响应_响应头 => { // 网络响应.响应头()
            let h = 读字段句柄(取句柄(参数,0), 2);
            match 堆读取(h) { Some(堆数据::映射(m)) => 分配映射(m), _ => 分配映射(Vec::new()) }
        }
        标准库索引::网络响应_是否成功 => { // 网络响应.是否成功()
            let code = 读字段整数(取句柄(参数,0), 0);
            运行时值::逻辑((200..=299).contains(&code))
        }
        标准库索引::网络响应_会话凭证 => { // 网络响应.会话凭证(名称)
            let 名称 = 取文本(参数, 1);
            let c = 读字段句柄(取句柄(参数,0), 3);
            match 堆读取(c) {
                Some(堆数据::映射(m)) => m.iter()
                    .find(|(k,_)| matches!(k, 堆键::文本(n) if n == &名称))
                    .map(|(_,v)| v.clone()).unwrap_or(运行时值::空值()),
                _ => 运行时值::空值(),
            }
        }
        标准库索引::网络响应_所有会话凭证 => { // 网络响应.所有会话凭证()
            let h = 读字段句柄(取句柄(参数,0), 3);
            match 堆读取(h) { Some(堆数据::映射(m)) => 分配映射(m), _ => 分配映射(Vec::new()) }
        }
        标准库索引::网络响应_添加头 => { // 网络响应.添加头(键, 值) — 原地更新响应结构体字段2（响应头映射）
            let 键 = 取文本(参数, 1); let 值 = 取文本(参数, 2);
            if let Err(消息) = 校验HTTP头(&键, &值) { 抛网络异常(消息); }
            let 响应句柄 = 取句柄(参数, 0);
            if let Some(堆数据::结构体 { 类型索引, mut 字段值 }) = 堆读取(响应句柄) {
                let 头句柄 = match 字段值.get(2) { Some(v) if v.标签 == 标签_句柄 => v.句柄, _ => 0 };
                let mut 条目 = match 堆读取(头句柄) { Some(堆数据::映射(m)) => m, _ => Vec::new() };
                条目.push((堆键::文本(键), 分配文本(值)));
                let 新头句柄 = 分配映射(条目).句柄;
                字段值[2] = 运行时值::句柄(新头句柄);
                堆写入(响应句柄, 堆数据::结构体 { 类型索引, 字段值 });
            }
            运行时值::空值()
        }

        // ── 网络请求查询 (198-204) ──
        标准库索引::网络请求_路径 => 分配文本(读字段文本(取句柄(参数,0), 1)),          // 网络请求.路径()
        标准库索引::网络请求_方法 => 分配文本(读字段文本(取句柄(参数,0), 0)),          // 网络请求.方法()
        标准库索引::网络请求_请求头 => { // 网络请求.请求头()
            let h = 读字段句柄(取句柄(参数,0), 2);
            match 堆读取(h) { Some(堆数据::映射(m)) => 分配映射(m), _ => 分配映射(Vec::new()) }
        }
        标准库索引::网络请求_内容 => 分配文本(读字段文本(取句柄(参数,0), 3)),          // 网络请求.内容()
        标准库索引::网络请求_查询参数 => { // 网络请求.查询参数()
            let h = 读字段句柄(取句柄(参数,0), 4);
            match 堆读取(h) { Some(堆数据::映射(m)) => 分配映射(m), _ => 分配映射(Vec::new()) }
        }
        标准库索引::网络请求_完整网址 => 分配文本(读字段文本(取句柄(参数,0), 5)),          // 网络请求.完整网址()
        标准库索引::网络请求_客户端地址 => 分配文本(读字段文本(取句柄(参数,0), 6)),          // 网络请求.客户端地址()
        _ => 运行时值::空值(),
    }
}
