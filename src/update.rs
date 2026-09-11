//! 应用自更新：启动时静默检查 GitHub Releases，发现新版本后提示用户确认，再下载替换运行中的二进制。
//!
//! 采用 [`self_update`]（mature、跨平台）完成"下载 + 替换运行中二进制"这一步——它处理了
//! Windows 不能覆盖运行中 exe（临时文件 + 替换流程）、macOS / Linux 原子改名等平台差异。
//!
//! 检查（check）只读 GitHub API，用已有的 `ureq` + `semver` + `serde_json`，无需 `self_update`
//! 参与；确认后安装（apply）才调用 `self_update`。
//!
//! ## GitHub Release 资产命名约定
//! self_update 会按当前平台挑选 release 资产。为让下载/替换可靠，发布时请把每个平台的二进制
//! 上传为形如 `screenshot-rs-<target>.bin` 的资产（如 `screenshot-rs-x86_64-unknown-linux-gnu.bin`、
//! `screenshot-rs-x86_64-pc-windows-msvc.exe`），并把 tag 命名为 `v{x.y.z}`。
//! `bin_name` 用于定位替换目标与匹配资产。

use std::ffi::OsString;
use std::path::PathBuf;
use std::sync::OnceLock;

use serde::Deserialize;

/// 当前运行程序的原始路径。因为 self_update(`self-replace`)替换后，
/// 原路径上就是新版本；用它来重启可跨平台（Windows 下 current_exe 可能被改名）。
static UPDATE_EXE: OnceLock<PathBuf> = OnceLock::new();

/// GitHub 仓库拥有者（发布者账号）。
pub const REPO_OWNER: &str = "ANG-LD";
/// 仓库名。
pub const REPO_NAME: &str = "screenshot-rs";
/// 替换目标二进制名（也是 release 资产名的一部分）。
pub const BIN_NAME: &str = "screenshot-rs";
/// 当前版本（随编译注入）。
pub const CURRENT_VERSION: &str = env!("CARGO_PKG_VERSION");

/// GitHub `/releases/latest` 的 JSON 里我们需要的字段。
#[derive(Deserialize)]
struct LatestRelease {
    tag_name: Option<String>,
}

/// 语义版本比较：`latest_tag` 比 `current` 新则返回 true。
/// 两边都带 `v` 前缀也没关系（自动剥离）。解析失败一律视为"不更新"（保守）。
fn is_newer(latest_tag: &str, current: &str) -> bool {
    let parse = |s: &str| semver::Version::parse(s.trim_start_matches('v')).ok();
    match (parse(latest_tag), parse(current)) {
        (Some(a), Some(b)) => a > b,
        _ => false,
    }
}

/// 静默检查 GitHub 是否有新版本。
///
/// - 有更新：返回 `Ok(Some(新版本号))`。
/// - 无更新 / 已是最新：`Ok(None)`。
/// - 网络 / 解析失败：`Err(原因)`（调用方应忽略，不阻塞启动）。
pub fn check_for_update() -> Result<Option<String>, String> {
    let url = format!("https://api.github.com/repos/{REPO_OWNER}/{REPO_NAME}/releases/latest");
    let user_agent = format!("{REPO_NAME}/{}", CURRENT_VERSION);

    let body = match ureq::get(url.as_str())
        .set("User-Agent", user_agent.as_str())
        .set("Accept", "application/vnd.github+json")
        .timeout(std::time::Duration::from_secs(10))
        .call()
    {
        Ok(r) => r,
        // 还没发布任何 release（私有 / 未发布）：视作无更新而非异常
        Err(ureq::Error::Status(404, _)) => return Ok(None),
        Err(e) => return Err(format!("查询最新版本失败: {e}")),
    };
    let body = body
        .into_string()
        .map_err(|e| format!("读取响应失败: {e}"))?;

    let release: LatestRelease =
        serde_json::from_str(&body).map_err(|e| format!("解析发布信息失败: {e}"))?;

    // 请求虽然 200，但可能没有 tag（理论不会）；保守返回无更新。
    let Some(tag) = release.tag_name else {
        return Ok(None);
    };

    if is_newer(&tag, CURRENT_VERSION) {
        Ok(Some(tag.trim_start_matches('v').to_string()))
    } else {
        Ok(None)
    }
}

/// 下载新版本并替换运行中的二进制。成功返回新版本号。
///
/// 注意：替换后当前进程仍是旧版本，需重启以加载新版本。重启请用 [`restart_app`]，
/// 它会在替换成功后自动拉起新版本并退出当前进程。
pub fn apply_update() -> Result<String, String> {
    // self_update 内部的 reqwest 会遵循 http_proxy/https_proxy 等代理环境变量；
    // 而版本检查用的 ureq 不经代理、可直连 GitHub（见 `check_for_update` 的注释）。
    // 若用户环境残留了不可用的代理（例如代理软件未开启），reqwest 会被该代理劫持，
    // 下载时抛 "error sending request" / Connection refused。这里屏蔽代理让下载直连，
    // 与已证实可用的直连路径保持一致。
    neutralize_proxy();

    // 在 self_update 替换前记录原始可执行文件路径：替换成功后，新版本就在这个路径。
    let exe = std::env::current_exe().map_err(|e| format!("获取当前程序路径失败: {e}"))?;
    let _ = UPDATE_EXE.set(exe.clone());

    // self_update 的替换阶段要在「程序所在目录」写 `.<名>.__temp__XXXXXX` 临时文件再原子改名
    // （同文件系统才能 rename）。若该目录不可写（如装在系统目录 /usr/bin、普通用户无写权限），
    // 下载成功后在替换时仍会报 `os error 13`。这里提前探测：能写才继续下载，避免白白下载
    // 几十 MB 再失败——把应用装到用户可写目录（如 ~/.local/bin/screenshot-rs）后自更新即可正常。
    let exe_dir = exe.parent().unwrap_or(std::path::Path::new("."));
    if !dir_is_writable(exe_dir) {
        return Err(format!(
            "应用更新失败：程序目录「{}」不可写（权限不足）。\n\
             应用装在系统目录时，普通用户无法自我替换运行中的程序。\n\
             请把应用安装到用户可写目录（如 ~/.local/bin/screenshot-rs）后再更新，\n\
             或改用 sudo 安装新版系统包来更新。",
            exe_dir.display()
        ));
    }

    let status = self_update::backends::github::Update::configure()
        .repo_owner(REPO_OWNER)
        .repo_name(REPO_NAME)
        .bin_name(BIN_NAME)
        .current_version(CURRENT_VERSION)
        // 用户已在 GUI 里点「立即更新」，不要再用 stdin 弹 [Y/n] 确认；
        // GUI 应用也不应往控制台打印状态。
        .no_confirm(true)
        .show_output(false)
        .build()
        .map_err(|e| format!("初始化更新器失败: {e}"))?
        .update()
        .map_err(|e| map_update_error(e, exe_dir))?;

    Ok(status.version().to_string())
}

/// 探测 `dir` 是否可写：`self_replace` 要在 exe 所在目录创建临时文件，这里用「创建 + 删除一个
/// 探针文件」来模拟（创建成功即说明可写）。不可写目录返回 false，避免误报可更新。
fn dir_is_writable(dir: &std::path::Path) -> bool {
    let probe = dir.join(".screenshot-rs-update-probe");
    match std::fs::File::create(&probe) {
        Ok(_) => {
            let _ = std::fs::remove_file(&probe);
            true
        }
        Err(_) => false,
    }
}

/// 把 self_update 失败翻译成用户能看懂的错误。权限不足（os error 13 / EACCES /
/// Permission denied）给出可操作指引；其它原样透传。
fn map_update_error(e: impl std::fmt::Display, exe_dir: &std::path::Path) -> String {
    let msg = e.to_string();
    if msg.contains("os error 13")
        || msg.contains("Permission denied")
        || msg.contains("EACCES")
    {
        format!(
            "应用更新失败：程序目录「{}」不可写（权限不足），无法替换运行中的程序。\n\
             请把应用安装到用户可写目录（如 ~/.local/bin）后重试，\n\
             或改用 sudo 安装新版系统包来更新。\n\
             （原错误：{msg}）",
            exe_dir.display()
        )
    } else {
        format!("应用更新失败: {msg}")
    }
}

/// 把「系统安装(exe 目录不可写)」的应用**自迁移**到用户可写目录，让 deb/系统包装机也能自更新。
///
/// 背景：deb 把二进制装在 `/usr/bin`(root 属主)，普通用户无法就地替换。首次运行时把
/// 「自身 + ONNX Runtime provider 库」复制到 `~/.local/`(用户可写)，然后用同一参数重新
/// exec 该副本并退出当前进程；此后每次都以用户目录副本为运行主体：
/// - `self_update` 能替换它(可写)→ 系统装机也能自更新；
/// - RUNPATH `$ORIGIN/../lib/screenshot-rs` 恰好命中 `~/.local/lib/screenshot-rs/` 下的
///   provider 库 → GPU 加速不丢；
/// - OCR small 模型仍在系统 `/usr/lib/screenshot-rs/`(deb 未卸载)→ 照常读到；
/// - 用户目录副本会比版本：比当前旧就原子替换（否则装新包也被旧副本粘住、永不提示更新），
///   不比当前旧则保留（应用内自更新的产物，不能降级）。
///
/// `--version` 能力探针字面量（**本项目自用**，不是给用户看的输出）。
///
/// `main()` 处理 `--version` 时用 `black_box` 引用它，于是**只要二进制支持
/// `--version`，文件里就一定有这串字节**，不支持的老版本没有。
/// `relocate_to_user_dir` 先用它扫描副本文件，就能在不启动进程的前提下判断副本
/// 认不认识 `--version`——否则拿老副本去问版本，老版本会把它当普通启动真的开出 GUI。
pub const VERSION_QUERY_MARKER: &str = "screenshot-rs--version-probe--v1";

/// 文件里是否含 [`VERSION_QUERY_MARKER`]（分块扫描，不把几十 MB 整个读进内存）。
#[cfg(target_os = "linux")]
fn has_version_support(path: &std::path::Path) -> bool {
    use std::io::Read;
    const CHUNK: usize = 1 << 20;
    let needle = VERSION_QUERY_MARKER.as_bytes();
    let Ok(mut file) = std::fs::File::open(path) else {
        return false;
    };
    // 多留 needle.len()-1 字节余量：跨块边界的匹配靠把上一块尾巴搬回开头兜住。
    let mut buf = vec![0u8; CHUNK + needle.len()];
    let mut carry = 0usize;
    loop {
        let read = match file.read(&mut buf[carry..]) {
            Ok(0) | Err(_) => return false,
            Ok(n) => n,
        };
        let filled = carry + read;
        if buf[..filled].windows(needle.len()).any(|w| w == needle) {
            return true;
        }
        carry = (needle.len() - 1).min(filled);
        buf.copy_within(filled - carry..filled, 0);
    }
}

/// 问用户目录副本「你是什么版本」。先做无副作用的能力探测，再执行 `--version`。
/// 返回 `None` = 副本不认识 `--version`（老版本）、执行失败、或输出不是合法 semver。
#[cfg(target_os = "linux")]
fn user_copy_version(target: &std::path::Path) -> Option<String> {
    if !has_version_support(target) {
        return None;
    }
    let out = std::process::Command::new(target)
        .arg("--version")
        .output()
        .ok()?;
    if !out.status.success() {
        return None;
    }
    let raw = String::from_utf8_lossy(&out.stdout);
    let ver = raw.trim();
    if semver::Version::parse(ver).is_err() {
        return None;
    }
    Some(ver.to_string())
}

/// 是否要用当前这份二进制覆盖 `~/.local/bin` 里的旧副本。
///
/// - 副本比当前**旧** → 覆盖。用户刚装了新包（例如 deb 0.1.1），不该被上次迁移留下的
///   老副本粘住——否则进程永远跑老版本，`check_for_update` 也永远拿老版本号去比。
/// - 副本 **>= 当前** → 保留。副本可能是应用内自更新后的更新版本，不能降级。
/// - 副本版本**未知**（连 `--version` 都不认识，或报出来的不是合法 semver）→ 覆盖。
///   不支持 `--version` 的必然比当前这份老；报不出合法版本号的副本也不该拦着装新包。
fn should_replace_user_copy(current: &str, copy_version: Option<&str>) -> bool {
    match copy_version {
        Some(v) if semver::Version::parse(v.trim_start_matches('v')).is_ok() => is_newer(current, v),
        _ => true,
    }
}

/// 原子替换用户目录副本：同目录写临时文件 → chmod 755 → rename 覆盖。
/// 用 rename 而非直接 `copy` 覆盖：正在运行的旧副本持有旧 inode 不受影响；
/// 中途失败也不会把正在使用的副本写成半截（直接覆盖会写坏它）。
#[cfg(target_os = "linux")]
fn replace_user_copy(src: &std::path::Path, target: &std::path::Path) -> std::io::Result<()> {
    let tmp = target.with_file_name(format!("{BIN_NAME}.new"));
    std::fs::copy(src, &tmp)?;
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&tmp, std::fs::Permissions::from_mode(0o755))?;
    }
    std::fs::rename(&tmp, target)
}

/// 仅 Linux 生效：Windows nsis 为 currentUser(用户目录、可写)无需迁移；
/// macOS 是 .app bundle(Contents/MacOS/Resources 结构不能拆散)不能迁移。
/// 由 `main()` 启动时最先调用；迁移后当前进程已被重新 exec(子进程 + 退出)。
#[cfg(target_os = "linux")]
pub fn relocate_to_user_dir() {
    // 用户可写目标目录 ~/.local/bin + ~/.local/lib/screenshot-rs
    let Some(home_dir) = std::env::var_os("HOME") else { return };
    let home = std::path::Path::new(home_dir.as_os_str());
    let bin_dir = home.join(".local").join("bin");
    let lib_dir = home.join(".local").join("lib").join(BIN_NAME);
    let target_exe = bin_dir.join(BIN_NAME);

    // 已在用户可写目录运行(便携/已迁移)→ 无需再迁移。
    let exe = match std::env::current_exe() {
        Ok(e) => e,
        Err(_) => return,
    };
    if dir_is_writable(exe.parent().unwrap_or(std::path::Path::new("."))) {
        return;
    }
    tracing::info!(
        "[update] 系统安装(exe 目录不可写: {}), 迁移到用户目录运行: {}",
        exe.display(),
        target_exe.display()
    );

    // 1) 把自身同步到用户目录：
    //    - 目标不存在 → 复制；
    //    - 目标存在但**比当前这份旧** → 原子替换。以前这里无条件保留旧副本（注释说
    //      「它才是自更新主体」），结果装上新包也照样跑着上次留下的老副本：进程一直
    //      是老版本，`check_for_update` 也一直拿老版本号去比，永远不提示更新；
    //    - 目标存在且不比当前旧 → 保留（可能是应用内自更新后的更新版本，不能降级）。
    if !target_exe.exists() {
        if std::fs::create_dir_all(&bin_dir).is_err() {
            tracing::warn!("[update] 无法创建 {}，跳过迁移", bin_dir.display());
            return;
        }
        if std::fs::copy(&exe, &target_exe).is_err() {
            tracing::warn!("[update] 复制自身到 {} 失败", target_exe.display());
            return;
        }
        use std::os::unix::fs::PermissionsExt;
        let _ = std::fs::set_permissions(&target_exe, std::fs::Permissions::from_mode(0o755));
    } else {
        let copy_version = user_copy_version(&target_exe);
        if should_replace_user_copy(CURRENT_VERSION, copy_version.as_deref()) {
            match replace_user_copy(&exe, &target_exe) {
                Ok(()) => tracing::info!(
                    "[update] 用户目录副本版本 {} 低于当前 {}，已替换为当前版本",
                    copy_version.as_deref().unwrap_or("未知(不支持 --version)"),
                    CURRENT_VERSION
                ),
                Err(e) => tracing::warn!(
                    "[update] 替换用户目录副本 {} 失败({e})，继续用旧副本运行",
                    target_exe.display()
                ),
            }
        } else {
            tracing::info!(
                "[update] 用户目录副本 {} 不低于当前 {}，保留（可能是自更新后的更新版本）",
                copy_version.as_deref().unwrap_or("未知"),
                CURRENT_VERSION
            );
        }
    }

    // 2) 复制 ONNX Runtime provider 库(cuda + shared)到 ~/.local/lib/screenshot-rs/。
    //    源：系统资源目录(deb: /usr/lib/screenshot-rs)。RUNPATH 从 ~/.local/bin 解析到
    //    ~/.local/lib/screenshot-rs，正好命中；模型仍在系统目录(base 未卸载)由
    //    bundled_resource_dirs 的 /usr/lib/<exe>/ 分支读到，无需迁移。
    let src_lib = std::path::Path::new("/usr/lib").join(BIN_NAME);
    if src_lib.is_dir() && std::fs::create_dir_all(&lib_dir).is_ok() {
        if let Ok(read) = std::fs::read_dir(&src_lib) {
            for entry in read.flatten() {
                let name = entry.file_name().to_string_lossy().to_string();
                if name.starts_with("libonnxruntime_providers_") && name.ends_with(".so") {
                    let dest = lib_dir.join(&name);
                    if !dest.exists() {
                        let _ = std::fs::copy(entry.path(), &dest);
                    }
                }
            }
        }
    }

    // 3) 用同一参数重新 exec 用户目录副本，退出当前进程。
    let args: Vec<OsString> = std::env::args_os().skip(1).collect();
    match std::process::Command::new(&target_exe).args(&args).spawn() {
        Ok(_) => {
            tracing::info!("[update] 已迁移到 {}，重启子进程后退出当前进程", target_exe.display());
            std::process::exit(0);
        }
        Err(e) => tracing::warn!("[update] 重新 exec 迁移副本失败({e})，继续以当前进程运行"),
    }
}

/// 自动重启到已安装的新版本：拉起 `UPDATE_EXE`（即 self_update 替换后新版本所在路径），
/// 成功后退出当前进程。若拉起失败（例如文件被占用），则保持当前进程运行，让用户手动重启。
///
/// 应在 [`apply_update`] 成功返回后调用；调用方也可先短暂显示"更新完成"再触发本次重启。
pub fn restart_app() {
    let exe = match UPDATE_EXE.get() {
        Some(exe) => exe.clone(),
        // 未记录（理论上不会）：回退到当前路径。
        None => match std::env::current_exe() {
            Ok(exe) => exe,
            Err(_) => return,
        },
    };

    let args: Vec<OsString> = std::env::args_os().skip(1).collect();
    match std::process::Command::new(&exe).args(&args).spawn() {
        Ok(_) => {
            // 新进程已启动，结束当前（旧版本）进程。由非主线程调用也安全。
            std::process::exit(0);
        }
        Err(e) => {
            // 无法自动重启：保留当前进程，提示用户手动重启。
            eprintln!("应用自更新后自动重启失败: {e}");
        }
    }
}

/// 绕过失效的 HTTP(S) 代理环境变量，让 self_update 内部的 reqwest 走直连。
///
/// 背景：版本检查用的 `ureq` 不读任何代理环境变量、总是直连 GitHub（已验证可用）；
/// 而 self_update 内部用 `reqwest`，它**会**遵循 `http_proxy`/`https_proxy` 走代理。
/// 若本地残留了未运行的代理端口（比如之前 Clash/proxy 留下的 `http_proxy=127.0.0.1:PORT`），
/// reqwest 就会去连那个死端口而报 `Connection refused` / `error sending request`，
/// 而 `ureq` 走直连反而畅通。这里设置 `NO_PROXY=*` 让 reqwest 也直连，
/// 与已验证的检查路径保持一致；访问 GitHub 若走 TUN/全局 VPN，直连流量会被透明接管，依旧可用。
///
/// 注意：只追加 `NO_PROXY`，**不删除**原有的代理变量，避免破坏其它场景的代理配置。
fn neutralize_proxy() {
    std::env::set_var("no_proxy", "*");
    std::env::set_var("NO_PROXY", "*");
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 用户目录副本的替换决策：旧副本要换掉（否则装新包也被粘住、永不提示更新），
    /// 同版本/更新版本要保留（自更新产物不能降级），问不出版本的老副本当作旧的处理。
    #[test]
    fn user_copy_replacement_decision() {
        // 副本旧（0.1.0 副本 vs 当前 0.1.1）→ 换
        assert!(should_replace_user_copy("0.1.1", Some("0.1.0")));
        // 副本不认识 --version（老版本）→ 换
        assert!(should_replace_user_copy("0.1.1", None));
        // 同版本 → 保留
        assert!(!should_replace_user_copy("0.1.1", Some("0.1.1")));
        // 副本更新（应用内自更新到 0.2.0）→ 保留，绝不降级
        assert!(!should_replace_user_copy("0.1.1", Some("0.2.0")));
        // 非 semver 的副本版本串 → 当作未知 → 换（不 panic）
        assert!(should_replace_user_copy("0.1.1", Some("garbage")));
    }

    /// 能力探针扫描：含标记的文件判为支持 `--version`；标记**骑在分块边界上**也要命中
    /// （这是分块扫描最容易写错的地方，用 1MB 边界上下的位置钉住）。
    #[cfg(target_os = "linux")]
    #[test]
    fn version_query_marker_scan_across_chunk_boundary() {
        let dir = std::env::temp_dir().join(format!("screenshot-rs-marker-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();

        // 标记起点落在第一个 1MB 分块的最后 5 字节里：跨块匹配
        let mut bytes = vec![b'.'; (1 << 20) - 5];
        bytes.extend_from_slice(VERSION_QUERY_MARKER.as_bytes());
        bytes.extend_from_slice(b"...tail");
        let hit = dir.join("with-marker");
        std::fs::write(&hit, &bytes).unwrap();
        assert!(has_version_support(&hit), "跨块边界的标记应命中");

        let miss = dir.join("without-marker");
        std::fs::write(&miss, b"no marker in here").unwrap();
        assert!(!has_version_support(&miss));
        // 不存在的文件 / 空文件都不能 panic
        assert!(!has_version_support(&dir.join("nonexistent")));
        let empty = dir.join("empty");
        std::fs::write(&empty, b"").unwrap();
        assert!(!has_version_support(&empty));

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn newer_when_latest_greater() {
        assert!(is_newer("v0.2.0", "0.1.0"));
        assert!(is_newer("2.0.0", "1.9.9"));
        assert!(!is_newer("0.1.0", "0.1.0"));
        assert!(!is_newer("v0.1.0", "0.1.0"));
        assert!(!is_newer("0.0.9", "0.1.0"));
    }

    #[test]
    fn newer_ignores_unparseable() {
        assert!(!is_newer("not-a-version", "0.1.0"));
        assert!(!is_newer("0.2.0", "not-a-version"));
    }
}
