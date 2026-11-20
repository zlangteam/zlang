// 文件: 后端/编译器/src/链接器.rs
// 职责: 链接器，支持多平台（Linux/MacOS用cc，Windows用rust-lld）

use std::path::PathBuf;
use std::process::Command;

/// 用户程序主栈保留大小（字节，256MB）。
///
/// 背景（失控递归防护 P0）：编译模式用户递归经原生 CALL 指令增长机器栈，
/// 而 PE 默认栈保留仅 1MB / Linux 默认 8MB，远小于我们的调用深度阈值（debug 50000 帧）——
/// 阈值尚未触发就先发生原生栈溢出崩溃（Windows 0xC00000FD），异常无法被 捕获。
/// 故统一放大栈保留，使阈值先触发并抛出可捕获的「栈溢出」，与解释模式行为对齐。
/// 保留为虚拟地址空间（按需提交），不是立刻占用物理内存。
const 用户程序栈保留字节: u64 = 268_435_456;

pub struct 链接器 {
    pub 目标文件列表: Vec<PathBuf>,
    pub 输出路径: Option<PathBuf>,
    pub 目标平台: 目标平台,
    pub 运行时库路径: Option<PathBuf>,
    /// W4 FFI 导出块：共享模式（生成 DLL 而非 EXE）
    pub 共享模式: bool,
    /// W4 FFI 导出块：需显式导出的符号名（MSVC/LLD 的 /DLL 默认不导出全局符号）。
    /// 由 codegen 生成的导出 thunk 符号名（ZL_EXPORT_0、ZL_EXPORT_1 …）在此登记。
    pub 导出符号: Vec<String>,
    /// 🆕 FFI: 外部库搜索路径列表（用于链接外部 C 库）
    pub 库路径列表: Vec<PathBuf>,
}

#[derive(Debug, Clone)]
pub enum 目标平台 {
    Linux,
    MacOS,
    Windows,
}

impl 目标平台 {
    pub fn 当前() -> Self {
        if cfg!(target_os = "linux") {
            目标平台::Linux
        } else if cfg!(target_os = "macos") {
            目标平台::MacOS
        } else if cfg!(target_os = "windows") {
            目标平台::Windows
        } else {
            目标平台::Linux
        }
    }

    pub fn 可执行文件后缀(&self) -> &str {
        match self {
            目标平台::Linux | 目标平台::MacOS => "",
            目标平台::Windows => ".exe",
        }
    }

    /// 静态库前缀（Windows 无前缀，Unix 有 lib）
    pub fn 库前缀(&self) -> &str {
        match self {
            目标平台::Windows => "",
            _ => "lib",
        }
    }

    /// 静态库后缀（.lib / .a）
    pub fn 库后缀(&self) -> &str {
        match self {
            目标平台::Windows => ".lib",
            _ => ".a",
        }
    }

    /// 动态库前缀（Windows 无前缀，Unix 有 lib）
    pub fn 动态库前缀(&self) -> &str {
        match self {
            目标平台::Windows => "",
            _ => "lib",
        }
    }

    /// 动态库后缀（.dll / .dylib / .so）
    pub fn 动态库后缀(&self) -> &str {
        match self {
            目标平台::Windows => ".dll",
            目标平台::MacOS => ".dylib",
            目标平台::Linux => ".so",
        }
    }

    /// 返回平台标识字符串，用于构建签名
    pub fn 平台标识(&self) -> &str {
        match self {
            目标平台::Linux => "linux",
            目标平台::MacOS => "macos",
            目标平台::Windows => "windows",
        }
    }

    /// 由命令行 --目标平台 参数解析目标平台（linux/macos/windows 及其常见别名）
    pub fn 从标识(标识: &str) -> Option<Self> {
        match 标识.to_lowercase().as_str() {
            "linux" => Some(目标平台::Linux),
            "macos" | "mac" | "osx" => Some(目标平台::MacOS),
            "windows" | "win" => Some(目标平台::Windows),
            _ => None,
        }
    }
}

impl 链接器 {
    pub fn 新建() -> Self {
        链接器 {
        目标文件列表: vec![],
        输出路径: None,
        目标平台: 目标平台::当前(),
        运行时库路径: None,
        共享模式: false,
        导出符号: vec![],
        库路径列表: vec![],
    }
    }

    pub fn 添加目标文件(&mut self, 路径: PathBuf) {
        self.目标文件列表.push(路径);
    }

    pub fn 设置输出路径(&mut self, 路径: PathBuf) {
        self.输出路径 = Some(路径);
    }

    /// W4 FFI 导出块：开启后链接器产出 DLL（而非 EXE）
    pub fn 设置共享模式(&mut self, 值: bool) {
        self.共享模式 = 值;
    }

    /// W4 FFI 导出块：登记需显式导出的符号名（与 codegen 生成的 thunk 符号一一对应）
    pub fn 设置导出符号(&mut self, 符号: Vec<String>) {
        self.导出符号 = 符号;
    }

    /// 🆕 FFI: 添加外部库搜索路径（用于链接外部 C 库）
    pub fn 添加库路径(&mut self, 路径: PathBuf) {
        self.库路径列表.push(路径);
    }

    /// 自动查找运行时库：依次搜索环境变量、可执行文件同目录、构建目录。
    /// 查找顺序与「重建产物落点」严格一致——统一优先 release（产出/release/base.lib），
    /// 消除「重建写 release、查找优先 debug 旧库遮蔽新库」的自相矛盾。
    pub fn 自动查找运行时库(&mut self) {
        // 1. 环境变量 Z_RUNTIME
        if let Ok(路径) = std::env::var("Z_RUNTIME") {
            let p = PathBuf::from(&路径);
            if p.exists() {
                self.运行时库路径 = Some(p);
                return;
            }
        }
        // 2. 当前可执行文件同目录（宿主 智语言开发套件.exe 位于 产出/debug/）
        if let Ok(当前) = std::env::current_exe() {
            if let Some(目录) = 当前.parent() {
                // Windows: base.lib, Unix: libbase.a
                for 名称 in &["base.lib", "libbase.a"] {
                    let 库路径 = 目录.join(名称);
                    if 库路径.exists() {
                        self.运行时库路径 = Some(库路径);
                        return;
                    }
                }
            }
        }
        // 3. 构建目录 (cargo build 输出) — release 优先（与重建落点一致），debug 兼容旧环境
        //    ⚡修复⚡ 必须以「工作区根」为基准解析，不能直接用相对路径：
        //    相对 PathBuf 会按**进程 CWD** 解析，而项目构建（构建运行）时 CWD 是项目目录
        //    （如 测试/2项目类测试/并发测试），"产出/release/base.lib" 会被解析到项目内
        //    根本不存在的路径，导致刚重建成功的库「查不到」，报
        //    「已构建运行时静态库，但仍无法在已知路径定位 base.lib」并链接失败。
        //    工作区根由宿主 exe 反推（产出/debug/智语言开发套件.exe → 上三层），
        //    与 构建运行时静态库 的推导方式严格一致。
        if let Ok(当前) = std::env::current_exe() {
            if let Some(工作区根) = 当前
                .parent()
                .and_then(|p| p.parent())
                .and_then(|p| p.parent())
            {
                for 后缀 in &["产出/release/base.lib", "产出/release/libbase.a",
                               "产出/debug/base.lib", "产出/debug/libbase.a",
                               "target/release/base.lib", "target/release/libbase.a",
                               "target/debug/base.lib", "target/debug/libbase.a"] {
                    let p = 工作区根.join(后缀);
                    if p.exists() {
                        self.运行时库路径 = Some(p);
                        return;
                    }
                }
            }
        }
        // 4. Unix 系统路径
        let 系统路径 = PathBuf::from("/usr/local/lib/libbase.a");
        if 系统路径.exists() {
            self.运行时库路径 = Some(系统路径);
        }
    }

    /// 确保运行时静态库（base.lib）存在且为最新，否则经 cargo 构建。
    ///
    /// 关键背景：`cargo build --bin 智语言` 仅构建 基础功能 的 `rlib`（供 Rust 依赖链接），
    /// **不会**触发 `staticlib` 产物；而用户程序经外部链接器（rust-lld）链接所需的正是
    /// `base.lib`。因此该静态库需要在每次链接用户程序前显式保证存在且不过期，
    /// 否则会出现“所有 `智语言_*` 符号 undefined”的链接失败。
    ///
    /// 新鲜度判定（彻底方案）：不再依赖脆弱的 mtime 时间戳比较，改为「内容指纹比对」——
    /// 读取 base.lib 同目录的 `.base_build_fingerprint` 标记，与调用方传入的 `期望_base指纹`
    /// （由 智语言开发套件 构建期烧入的 基础功能 源树指纹）比较；不一致或缺失即重建。
    /// 重建采用 `cargo clean + cargo build --release`，绕过 cargo 自身的 up-to-date 误判
    /// （改 base 源码后裸 cargo build 秒回 0.19s 仍链旧库的痛点）。
    pub fn 确保运行时库已构建(&mut self, 期望_base指纹: &str) -> Result<(), String> {
        if self.运行时库路径.is_none() {
            self.自动查找运行时库();
        }
        // 已查到 → 用内容指纹比对判定是否过期（取代 mtime 启发式）
        if let Some(现有) = &self.运行时库路径.clone() {
            if let Some(目录) = 现有.parent() {
                let 标记路径 = 目录.join(".base_build_fingerprint");
                let 标记一致 = std::fs::read_to_string(&标记路径)
                    .map(|值| 值.trim() == 期望_base指纹)
                    .unwrap_or(false);
                if 标记一致 {
                    return Ok(()); // 快路径：标记一致 → 直接复用（零额外 IO）
                }
                // ⚠ 标记只是**缓存**，权威是产物自身（2026-11-03）。
                //   标记由本函数在「本文件重建成功后」写入；任何绕过本函数的构建
                //   （例如直接 `cargo build -p basic_functions`）都会产出新库而留下旧标记。
                //   旧逻辑据此判「过期」→ 在无源码机器上重建必然失败 → 每次编译打一条死警告。
                //   现在先扫库内自带的 `ZHI-BASE-FP:<hex>`：一致 ⇒ 只是缓存陈旧 ⇒ 写回后复用。
                if base::构建标识::读取内嵌标识(现有.as_path()).ok().as_deref() == Some(期望_base指纹) {
                    // 只读安装目录（系统级安装）写不进去属正常：提示即可，不阻断复用。
                    if let Err(错误) = std::fs::write(&标记路径, 期望_base指纹) {
                        eprintln!(
                            "[智语言] 提示：无法写回运行时库指纹缓存（{}），已直接复用库内自带构建标识",
                            错误
                        );
                    }
                    return Ok(());
                }
                // 标记与产物都不同源 → 重建
            }
        } else {
            // 完全查不到 → 重建
        }
        let 旧路径 = self.运行时库路径.clone();
        self.构建运行时静态库(期望_base指纹)?;
        // 重建后重新查找（路径搜索基于当前可执行文件所在目录）
        self.运行时库路径 = None;
        self.自动查找运行时库();
        // 查找顺序里「宿主同目录」排在重建落点（产出/release）之前：宿主在 产出/debug 时，
        // 同目录那份过期的库会再次被选中，刚重建的新库反而用不上。
        // 因此重建后以库内自带构建标识为准：查到的不同源，就改用重建落点里同源的那份。
        let 同源 = |路径: &std::path::Path| {
            base::构建标识::读取内嵌标识(路径).ok().as_deref() == Some(期望_base指纹)
        };
        if !self.运行时库路径.as_deref().map(|p| 同源(p)).unwrap_or(false) {
            if let Some(落点目录) = std::env::current_exe().ok()
                .and_then(|p| p.parent().and_then(|p| p.parent()).map(|p| p.join("release")))
            {
                for 名称 in &["base.lib", "libbase.a"] {
                    let 候选 = 落点目录.join(名称);
                    if 候选.exists() && 同源(&候选) {
                        self.运行时库路径 = Some(候选);
                        break;
                    }
                }
            }
        }
        if self.运行时库路径.is_none() { self.运行时库路径 = 旧路径; }
        if self.运行时库路径.is_none() {
            return Err(
                "已构建运行时静态库，但仍无法在已知路径定位 base.lib".to_string(),
            );
        }
        Ok(())
    }

    /// 经 cargo 构建 基础功能 包的 staticlib，产物落到构建目录（与宿主二进制同目录）。
    /// 采用 `clean + build --release` 强制从源重建，绕过 cargo 缓存 up-to-date 误判；
    /// 重建后将 `期望_base指纹` 写入 `产出/release/.base_build_fingerprint` 标记。
    fn 构建运行时静态库(&self, 期望_base指纹: &str) -> Result<(), String> {
        let 当前 = std::env::current_exe()
            .map_err(|e| format!("无法获取当前可执行文件路径: {}", e))?;
        // 当前 = <工作区>/产出/debug/智语言.exe → 上三层即工作区根
        let 工作区根 = 当前
            .parent()
            .and_then(|p| p.parent())
            .and_then(|p| p.parent())
            .ok_or_else(|| "无法由当前可执行文件路径推导工作区根目录".to_string())?;

        // 强制清除旧 staticlib 产物，确保从最新源重建（根治「秒回 0.19s 误判已最新」）
        let _ = Command::new("cargo")
            .arg("clean")
            .arg("-p")
            .arg("basic_functions")
            .arg("--manifest-path")
            .arg(工作区根.join("Cargo.toml"))
            .arg("--target-dir")
            .arg(工作区根.join("产出"))
            .output();

        let 输出 = Command::new("cargo")
            .arg("build")
            // ⚡性能⚡ D2: 运行时静态库用 release 构建（优化 + 更小体积），
            // 用户编译产物链接 release base 显著提升运行时性能（GC/堆操作/引用计数）。
            .arg("--release")
            .arg("-p")
            .arg("basic_functions")
            .arg("--manifest-path")
            .arg(工作区根.join("Cargo.toml"))
            .arg("--target-dir")
            .arg(工作区根.join("产出"))
            .output()
            .map_err(|e| {
                format!(
                    "执行 cargo 构建 基础功能 失败: {}（请确保已安装 Rust 工具链且在工作区内运行）",
                    e
                )
            })?;
        if !输出.status.success() {
            return Err(format!(
                "构建 基础功能 staticlib 失败:\n{}",
                String::from_utf8_lossy(&输出.stderr)
            ));
        }
        // 写新鲜度标记（与重建落点一致：产出/release/）
        let 标记路径 = 工作区根.join("产出").join("release").join(".base_build_fingerprint");
        let _ = std::fs::write(&标记路径, 期望_base指纹);
        Ok(())
    }

    pub fn 链接(&self) -> Result<(), String> {
        let 输出路径 = self.输出路径.as_ref().ok_or("未设置输出路径")?;

        // 确保输出目录存在
        if let Some(父) = 输出路径.parent() {
            std::fs::create_dir_all(父).map_err(|e| format!("创建输出目录失败: {}", e))?;
        }

        match self.目标平台 {
            目标平台::Linux | 目标平台::MacOS => self.链接_cc(输出路径),
            目标平台::Windows => self.链接_windows(输出路径),
        }
    }

    // ========== §6.5.3 导出静态库：归档而非链接 ==========

    /// 把目标文件列表打包为静态库（`.a` / `.lib`），供 C 程序静态链接。
    ///
    /// 与 `链接` 的本质区别：静态库**不做符号解析**，只是目标文件的归档集合，
    /// 因此不链接运行时库 base、不需要导出表、允许存在未定义符号
    /// （由最终 C 程序链接时一并解析——用户须同时链接 base 静态库）。
    ///
    /// 工具优先级（跨平台 + 中文路径铁律）：
    /// 1. Rust sysroot 自带 `llvm-ar`（LLVM 许可可再分发，UTF-16 API 免疫中文路径）
    /// 2. PATH 中的 `llvm-ar`
    /// 3. Windows MSVC：`rust-lld -flavor link /LIB`（LLVM 内置归档器，无需额外组件）
    /// 4. POSIX 回退：系统 `ar`
    pub fn 归档静态库(&self) -> Result<(), String> {
        let 输出路径 = self.输出路径.as_ref().ok_or("未设置输出路径")?;
        if let Some(父) = 输出路径.parent() {
            std::fs::create_dir_all(父).map_err(|e| format!("创建输出目录失败: {}", e))?;
        }
        // 归档器要求输出文件不存在（否则会追加到旧归档，残留过期目标文件）
        if 输出路径.exists() {
            let _ = std::fs::remove_file(输出路径);
        }
        if self.目标文件列表.is_empty() {
            return Err("静态库归档失败：没有任何目标文件".to_string());
        }

        // 1/2. llvm-ar（sysroot 或 PATH）
        if let Some(ar路径) = self.查找_llvm_ar() {
            let mut cmd = Command::new(&ar路径);
            cmd.arg("crs").arg(输出路径);
            for 文件 in &self.目标文件列表 {
                cmd.arg(文件);
            }
            let 输出 = cmd
                .output()
                .map_err(|e| format!("执行 llvm-ar 失败: {}", e))?;
            if 输出.status.success() {
                return Ok(());
            }
            // llvm-ar 存在但失败 → 继续尝试后备方案，最终失败时汇总报错
        }

        // 3. Windows：rust-lld 的 lib 模式（LLVM 内置归档器）
        if matches!(self.目标平台, 目标平台::Windows) {
            if let Ok(lld路径) = self.查找_rust_lld() {
                let mut cmd = Command::new(&lld路径);
                cmd.arg("-flavor").arg("link").arg("/LIB").arg("/NOLOGO");
                cmd.arg(format!("/OUT:{}", 输出路径.display()));
                for 文件 in &self.目标文件列表 {
                    cmd.arg(文件);
                }
                let 输出 = cmd
                    .output()
                    .map_err(|e| format!("执行 rust-lld /LIB 失败: {}", e))?;
                if 输出.status.success() {
                    return Ok(());
                }
                return Err(format!(
                    "静态库归档失败（rust-lld /LIB）:\n--- stdout ---\n{}\n--- stderr ---\n{}",
                    String::from_utf8_lossy(&输出.stdout),
                    String::from_utf8_lossy(&输出.stderr)
                ));
            }
        }

        // 4. POSIX 回退：系统 ar
        if !matches!(self.目标平台, 目标平台::Windows) {
            let mut cmd = Command::new("ar");
            cmd.arg("crs").arg(输出路径);
            for 文件 in &self.目标文件列表 {
                cmd.arg(文件);
            }
            let 输出 = cmd
                .output()
                .map_err(|e| format!("执行 ar 失败: {}（请安装 llvm-ar 或 binutils）", e))?;
            if 输出.status.success() {
                return Ok(());
            }
            return Err(format!(
                "静态库归档失败（ar）:\n{}",
                String::from_utf8_lossy(&输出.stderr)
            ));
        }

        Err("静态库归档失败：找不到可用的归档器（llvm-ar / rust-lld / ar）".to_string())
    }

    /// 查找 llvm-ar：优先 Rust sysroot 自带（llvm-tools 组件），其次 PATH。
    fn 查找_llvm_ar(&self) -> Option<PathBuf> {
        let 可执行名 = if matches!(self.目标平台, 目标平台::Windows) {
            "llvm-ar.exe"
        } else {
            "llvm-ar"
        };
        if let Ok(sysroot) = self.获取_sysroot() {
            let 三元组 = Self::当前目标三元组();
            let 候选 = vec![
                sysroot
                    .join("lib/rustlib")
                    .join(三元组)
                    .join("bin")
                    .join(可执行名),
                sysroot.join("bin").join(可执行名),
            ];
            for p in 候选 {
                if p.exists() {
                    return Some(p);
                }
            }
        }
        // PATH 探测：能成功执行 --version 即认为可用
        if Command::new(可执行名).arg("--version").output().is_ok() {
            return Some(PathBuf::from(可执行名));
        }
        None
    }

    // ========== Linux/MacOS：使用系统 cc ==========

    fn 链接_cc(&self, 输出路径: &PathBuf) -> Result<(), String> {
        let mut cmd = Command::new("cc");
        for 文件 in &self.目标文件列表 {
            cmd.arg(文件);
        }
        if let Some(运行时路径) = &self.运行时库路径 {
            if let Some(库目录) = 运行时路径.parent() {
                cmd.arg("-L").arg(库目录);
            }
            cmd.arg("-lbase");
        }
        // 🆕 FFI: 传递外部库搜索路径
        for 库路径 in &self.库路径列表 {
            cmd.arg("-L").arg(库路径);
        }
        if self.共享模式 {
            cmd.arg("-shared");
        }
        // 放大主栈保留（见 用户程序栈保留字节 说明）：使失控递归先命中深度阈值抛可捕获
        // 「栈溢出」，而非先发生原生栈溢出崩溃。
        #[cfg(target_os = "macos")]
        cmd.arg(format!("-Wl,-stack_size,0x{:x}", 用户程序栈保留字节));
        #[cfg(target_os = "linux")]
        cmd.arg(format!("-Wl,-z,stack-size={}", 用户程序栈保留字节));

        cmd.arg("-lm").arg("-lpthread").arg("-o").arg(输出路径);

        let 输出 = cmd.output().map_err(|e| format!("执行链接器失败: {}", e))?;
        if !输出.status.success() {
            let stderr = String::from_utf8_lossy(&输出.stderr);
            return Err(format!("链接失败: {}", stderr));
        }
        Ok(())
    }

    // ========== Windows：使用 rust-lld (LLVM 链接器) ==========

    fn 链接_windows(&self, 输出路径: &PathBuf) -> Result<(), String> {
        // 检测当前 Rust 目标三元组，判断是 MSVC 还是 GNU
        let 是msvc = cfg!(target_env = "msvc");

        if 是msvc {
            self.链接_msvc_lld(输出路径)
        } else {
            // GNU/MinGW 环境：直接用 rust-lld(gnu)（弃 gcc——中文路径必崩，2026-08-05 定稿）
            self.链接_mingw(输出路径)
        }
    }

    /// MSVC 工具链：使用 rust-lld 链接 COFF .obj + .lib
    fn 链接_msvc_lld(&self, 输出路径: &PathBuf) -> Result<(), String> {
        let lld路径 = self.查找_rust_lld()?;

        let mut cmd = Command::new(&lld路径);
        // rust-lld 的 link 模式模拟 MSVC link.exe
        cmd.arg("-flavor").arg("link");

        // 添加所有目标文件
        for 文件 in &self.目标文件列表 {
            cmd.arg(文件);
        }

        // 添加运行时库
        if let Some(运行时库) = &self.运行时库路径 {
            cmd.arg(运行时库);
        }

        // 添加 Rust 标准库
        if let Some(rust库目录) = self.查找_rust_lib目录() {
            // 查找 std dll import library
            if let Ok(条目) = std::fs::read_dir(&rust库目录) {
                for 项 in 条目.flatten() {
                    let 名称 = 项.file_name().to_string_lossy().to_string();
                    if 名称.starts_with("std-") && 名称.ends_with(".dll.lib") {
                        cmd.arg(项.path());
                        break; // 只取第一个匹配的 std import lib
                    }
                }
            }
            cmd.arg(format!("/LIBPATH:{}", rust库目录.display()));
        }

        // 🆕 FFI: 添加外部库搜索路径
        for 库路径 in &self.库路径列表 {
            cmd.arg(format!("/LIBPATH:{}", 库路径.display()));
        }

        // 输出设置
        cmd.arg(format!("/OUT:{}", 输出路径.display()));
        cmd.arg("/NOLOGO");

        // 子系统（共享模式 → 生成 DLL 导出表，供外部加载调用）
        if self.共享模式 {
            cmd.arg("/DLL");
            // MSVC/LLD 的 /DLL 默认不导出全局符号，需逐个显式导出 thunk 符号
            for 符号 in &self.导出符号 {
                cmd.arg(format!("/EXPORT:{}", 符号));
            }
        } else {
            cmd.arg("/SUBSYSTEM:CONSOLE");
        }

        // 必需的系统库
        cmd.arg("/DEFAULTLIB:msvcrt");
        cmd.arg("/DEFAULTLIB:vcruntime");     // C++ 异常处理（Rust panic_unwind 需要）
        cmd.arg("/DEFAULTLIB:kernel32");
        cmd.arg("/DEFAULTLIB:advapi32");
        cmd.arg("/DEFAULTLIB:bcrypt");
        cmd.arg("/DEFAULTLIB:userenv");
        cmd.arg("/DEFAULTLIB:ws2_32");
        cmd.arg("/DEFAULTLIB:ntdll");
        // TLS（schannel / ureq 默认 TLS 后端）依赖的 Windows 系统库，
        // 缺失会导致「undefined symbol: FreeCredentialsHandle / CryptDecodeObjectEx / NCryptFreeObject」等链接错误
        cmd.arg("/DEFAULTLIB:secur32");
        cmd.arg("/DEFAULTLIB:crypt32");
        cmd.arg("/DEFAULTLIB:ncrypt");
        cmd.arg("/DEFAULTLIB:user32");

        // 调试信息
        cmd.arg("/DEBUG");

        // 放大主栈保留（见 用户程序栈保留字节 说明）：PE 默认 1MB 会在深度阈值前原生栈溢出。
        cmd.arg(format!("/STACK:{}", 用户程序栈保留字节));

        let 输出 = cmd.output().map_err(|e| format!("执行 rust-lld 失败: {}", e))?;

        if 输出.status.success() {
            Ok(())
        } else {
            let stdout = String::from_utf8_lossy(&输出.stdout);
            let stderr = String::from_utf8_lossy(&输出.stderr);
            Err(format!(
                "rust-lld 链接失败:\n--- stdout ---\n{}\n--- stderr ---\n{}",
                stdout, stderr
            ))
        }
    }

    /// MinGW/GNU 工具链：使用 rust-lld 的 gnu 模式链接。
    ///
    /// 中文路径根治铁律（2026-08-05 定稿）：**弃用 gcc 链接**——MinGW 的 gcc 是 GNU 第三方程序，
    /// 链老 MSVCRT 把 UTF-8 中文路径按 GBK(936) 解码会乱码「找不到文件」。
    /// rust-lld（Rust 工具链自带，LLVM 链接器）走 UTF-16 API，天生免疫中文路径。
    fn 链接_mingw(&self, 输出路径: &PathBuf) -> Result<(), String> {
        let lld路径 = self.查找_rust_lld()?;

        let mut lld = Command::new(&lld路径);
        lld.arg("-flavor").arg("gnu");
        for 文件 in &self.目标文件列表 {
            lld.arg(文件);
        }
        if let Some(运行时库) = &self.运行时库路径 {
            lld.arg(运行时库);
            if let Some(库目录) = 运行时库.parent() {
                lld.arg("-L").arg(库目录);
            }
        }
        // 🆕 FFI: 添加外部库搜索路径
        for 库路径 in &self.库路径列表 {
            lld.arg("-L").arg(库路径);
        }
        if self.共享模式 {
            lld.arg("-shared");
            // 导出全部全局符号（等价原 gcc 的 -Wl,--export-all-symbols）
            lld.arg("--export-all-symbols");
            // W4 FFI 导出块：显式导出 ZL_EXPORT_N 符号，确保进入导出表
            for 符号 in &self.导出符号 {
                lld.arg(format!("--export-dynamic-symbol={}", 符号));
            }
        }
        // 放大主栈保留（见 用户程序栈保留字节 说明）
        lld.arg(format!("--stack={}", 用户程序栈保留字节));
        lld.arg("-o").arg(输出路径);

        let 输出 = lld.output().map_err(|e| format!("执行 rust-lld(gnu) 失败: {}", e))?;
        if 输出.status.success() {
            return Ok(());
        }
        let stderr = String::from_utf8_lossy(&输出.stderr);
        Err(format!(
            "链接失败: rust-lld(gnu) 链接失败。\n\
             stderr: {}\n\
             请确保已安装 MSVC Build Tools（含 rust-lld，Rust 工具链自带）。",
            stderr
        ))
    }

    // ========== 工具函数 ==========

    /// 获取当前编译目标三元组
    fn 当前目标三元组() -> &'static str {
        if cfg!(all(target_arch = "x86_64", target_os = "windows", target_env = "msvc")) {
            "x86_64-pc-windows-msvc"
        } else if cfg!(all(target_arch = "x86_64", target_os = "windows", target_env = "gnu")) {
            "x86_64-pc-windows-gnu"
        } else if cfg!(all(target_arch = "x86_64", target_os = "linux", target_env = "gnu")) {
            "x86_64-unknown-linux-gnu"
        } else if cfg!(all(target_arch = "x86_64", target_os = "linux", target_env = "musl")) {
            "x86_64-unknown-linux-musl"
        } else if cfg!(all(target_arch = "x86_64", target_os = "macos")) {
            "x86_64-apple-darwin"
        } else if cfg!(all(target_arch = "aarch64", target_os = "macos")) {
            "aarch64-apple-darwin"
        } else if cfg!(all(target_arch = "aarch64", target_os = "linux")) {
            "aarch64-unknown-linux-gnu"
        } else {
            // 回退：从环境变量获取
            "x86_64-pc-windows-msvc"
        }
    }

    /// 查找 rust-lld 可执行文件（Rust 工具链自带）
    fn 查找_rust_lld(&self) -> Result<PathBuf, String> {
        let 三元组 = Self::当前目标三元组();

        // 1. 通过 rustc --print sysroot 查找
        if let Ok(sysroot) = self.获取_sysroot() {
            let 候选 = vec![
                sysroot.join("lib/rustlib").join(三元组).join("bin/rust-lld"),
                // 也尝试 .exe 扩展名
                sysroot.join("lib/rustlib").join(三元组).join("bin/rust-lld.exe"),
                // 备用位置
                sysroot.join("bin/rust-lld"),
                sysroot.join("bin/rust-lld.exe"),
            ];
            for p in &候选 {
                if p.exists() {
                    return Ok(p.clone());
                }
            }
        }
        // 2. 直接在 PATH 中搜索
        Err("找不到 rust-lld。请确保 Rust 工具链正确安装。".to_string())
    }

    /// 获取 Rust sysroot
    fn 获取_sysroot(&self) -> Result<PathBuf, String> {
        let 输出 = Command::new("rustc")
            .arg("--print").arg("sysroot")
            .output()
            .map_err(|e| format!("无法执行 rustc: {}", e))?;
        if 输出.status.success() {
            let 路径 = String::from_utf8_lossy(&输出.stdout).trim().to_string();
            Ok(PathBuf::from(路径))
        } else {
            Err("rustc --print sysroot 失败".to_string())
        }
    }

    /// 查找 Rust 标准库目录
    fn 查找_rust_lib目录(&self) -> Option<PathBuf> {
        let 三元组 = Self::当前目标三元组();
        if let Ok(sysroot) = self.获取_sysroot() {
            let 候选 = vec![
                sysroot.join("lib/rustlib").join(三元组).join("lib"),
            ];
            for p in &候选 {
                if p.exists() {
                    return Some(p.clone());
                }
            }
        }
        None
    }
}
