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

use serde::Deserialize;

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
/// 注意：替换后当前进程仍是旧版本，需提示用户重启以加载新版本。
pub fn apply_update() -> Result<String, String> {
    let status = self_update::backends::github::Update::configure()
        .repo_owner(REPO_OWNER)
        .repo_name(REPO_NAME)
        .bin_name(BIN_NAME)
        .current_version(CURRENT_VERSION)
        .build()
        .map_err(|e| format!("初始化更新器失败: {e}"))?
        .update()
        .map_err(|e| format!("应用更新失败: {e}"))?;

    Ok(status.version().to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

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
