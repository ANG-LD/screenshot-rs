//! 单实例保护：同一用户会话里只允许一个 screenshot-rs 在跑。
//!
//! 用户反馈：程序已经在跑了，从桌面再双击一次图标会起第二个实例（托盘里两个图标、
//! 全局热键第二个还注册不上）。这里在启动早期抢一把锁，抢不到就安静退出。
//!
//! 机制选型（要求"进程被 kill -9 也不会留下死锁"）：
//! - Unix：对锁文件做 `flock(LOCK_EX)`。flock 绑定在 **打开的文件描述** 上，进程无论
//!   怎么死（崩溃 / SIGKILL / 断电）都由内核自动释放，不存在"残留锁文件导致再也启动
//!   不了"的问题；文件本身留在原地无所谓，它只是个加锁载体。
//! - Windows：命名互斥体 `CreateMutexW` + `ERROR_ALREADY_EXISTS`，同样是内核对象，
//!   进程退出即释放。
//!
//! 不用"独占创建文件"（`create_new`）那种做法：那个靠文件存在性判断，进程被 kill 后
//! 会留下永久残留，用户得手动删文件才能再启动。
//!
//! **与自更新/自迁移的配合**（重要）：
//! `update::relocate_to_user_dir` 与 `update::restart_app` 都是「spawn 新进程 → 旧进程
//! exit」，如果新进程照着"非阻塞抢锁"来，就会因为旧进程还握着锁而直接退出，结果是
//! 升级/迁移之后应用彻底没了。所以那两处 spawn 会给子进程带上
//! [`TAKEOVER_ENV`]，子进程据此改成"等旧进程让出锁"（最长 [`TAKEOVER_TIMEOUT`]）。

use std::path::PathBuf;

/// 由自更新/自迁移拉起的"接班人"子进程会带上这个环境变量，表示：
/// 锁被上一个进程占着是正常的，等它退出即可。
pub const TAKEOVER_ENV: &str = "SCREENSHOT_RS_TAKEOVER";

/// 接班人等待旧进程释放锁的最长时间（旧进程是 spawn 完就 exit，通常几毫秒）。
const TAKEOVER_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(5);

/// 抢到的单实例锁。**必须一直持有到进程结束**：一旦 Drop（或进程退出）锁就释放。
pub struct SingleInstance {
    /// Unix：**全部**锁文件句柄（持有 fd 即持有锁，缺一不可）。Windows 下为空。
    #[allow(dead_code)]
    files: Vec<std::fs::File>,
    /// Windows：命名互斥体句柄（存成 isize 以免 HANDLE 裸指针带来 Send/Sync 问题）。
    #[allow(dead_code)]
    handle: isize,
}

impl SingleInstance {
    /// 尝试成为唯一实例。已有实例在跑时返回 `None`（并把对方的 pid 写进日志便于排查）。
    pub fn acquire() -> Option<Self> {
        let takeover = std::env::var_os(TAKEOVER_ENV).is_some();
        Self::acquire_with(takeover)
    }

    fn acquire_with(takeover: bool) -> Option<Self> {
        imp::acquire(takeover)
    }

    /// 需要**同时**持有的锁文件路径（Unix 用；Windows 走命名互斥体没有文件）。
    ///
    /// 为什么要两把而不是一把：只按 `XDG_RUNTIME_DIR` 放锁会留一个真洞——同一个用户
    /// 从桌面启动（该变量存在，锁落在 `/run/user/<uid>/`）和从 ssh / 部分 IDE 终端启动
    /// （该变量不存在，锁落到 `/tmp`）会**各锁各的文件**，两边都放行，多开就又出现了。
    /// 所以额外锁一个与环境变量无关的固定位置 `/tmp/screenshot-rs-<uid>.lock`：
    /// 无论对方从哪种环境启动，只要它也走这个固定位置，就一定撞上。
    ///
    /// 固定位置用字面量 `/tmp` 而不是 `std::env::temp_dir()`——后者会读 `TMPDIR`，
    /// 那又是一个可被启动环境改掉的变量，等于把洞换个地方留着。
    pub fn lock_paths() -> Vec<PathBuf> {
        let mut paths = vec![PathBuf::from(format!("/tmp/screenshot-rs-{}.lock", uid()))];
        // 会话目录：登录会话结束时由系统清理，留着它可以兜住"固定位置被 tmpfiles 清掉"
        if let Some(dir) = std::env::var_os("XDG_RUNTIME_DIR")
            .map(PathBuf::from)
            .filter(|p| p.is_dir())
        {
            let p = dir.join(format!("screenshot-rs-{}.lock", uid()));
            if !paths.contains(&p) {
                paths.push(p);
            }
        }
        paths
    }

    /// 仅供测试：在指定路径集合上抢锁。
    #[cfg(test)]
    fn acquire_paths(paths: &[PathBuf], wait: Option<std::time::Duration>) -> Option<Self> {
        imp::acquire_paths(paths, wait)
    }
}

#[cfg(unix)]
fn uid() -> u32 {
    // SAFETY: getuid 无参数、无副作用，永远成功
    unsafe { libc::getuid() }
}

#[cfg(not(unix))]
fn uid() -> u32 {
    0
}

// ───────────────────────── Unix（flock） ─────────────────────────

#[cfg(unix)]
mod imp {
    use super::{SingleInstance, TAKEOVER_TIMEOUT};
    use std::fs::{File, OpenOptions};
    use std::io::{Read, Seek, SeekFrom, Write};
    use std::os::fd::AsRawFd;
    use std::path::PathBuf;
    use std::time::{Duration, Instant};

    pub fn acquire(takeover: bool) -> Option<SingleInstance> {
        // 接班人：等旧进程让出锁；普通启动：一次抢不到就认为已有实例
        let wait = takeover.then_some(TAKEOVER_TIMEOUT);
        acquire_paths(&SingleInstance::lock_paths(), wait)
    }

    /// 逐把抢锁，**全部拿到才算成功**（顺序固定，两个进程同时启动也不会互相死等）。
    pub fn acquire_paths(paths: &[PathBuf], wait: Option<Duration>) -> Option<SingleInstance> {
        let deadline = wait.map(|w| Instant::now() + w);
        loop {
            match try_lock_all(paths) {
                LockOutcome::All(files) => {
                    // 把 pid 写进第一把锁，方便下一次启动时日志里能说清是谁占着
                    if let Some(file) = files.first() {
                        let mut f = file;
                        let _ = f.set_len(0);
                        let _ = f.seek(SeekFrom::Start(0));
                        let _ = write!(f, "{}", std::process::id());
                        let _ = f.flush();
                    }
                    let shown: Vec<String> =
                        paths.iter().map(|p| p.display().to_string()).collect();
                    tracing::info!("单实例：已获得锁 {}", shown.join(" + "));
                    return Some(SingleInstance {
                        files,
                        handle: 0,
                    });
                }
                LockOutcome::Held { path, pid } => match deadline {
                    Some(d) if Instant::now() < d => {
                        // 旧进程正在退出：睡一下再抢（flock 没有带超时的阻塞模式）
                        std::thread::sleep(Duration::from_millis(20));
                    }
                    _ => {
                        match pid {
                            Some(pid) => tracing::info!(
                                "已有实例在运行（pid={pid}，锁 {}），本次启动退出",
                                path.display()
                            ),
                            None => tracing::info!(
                                "已有实例在运行（锁 {}），本次启动退出",
                                path.display()
                            ),
                        }
                        return None;
                    }
                },
                // 一把锁都没法建立（例如目录不可写）：宁可能多开，也不能让用户起不来
                LockOutcome::Unusable => {
                    tracing::warn!("单实例：无法建立任何锁文件，跳过单实例检查");
                    return Some(SingleInstance {
                        files: Vec::new(),
                        handle: 0,
                    });
                }
            }
        }
    }

    enum LockOutcome {
        All(Vec<File>),
        /// 某一处被占：可能是别的实例持有（pid 用来打日志）
        Held { path: PathBuf, pid: Option<u32> },
        /// 所有路径都无法打开/加锁（不是"已有实例"，是环境问题）
        Unusable,
    }

    fn try_lock_all(paths: &[PathBuf]) -> LockOutcome {
        let mut files = Vec::new();
        let mut unusable = 0usize;
        for path in paths {
            match OpenOptions::new()
                .create(true)
                .read(true)
                .write(true)
                .truncate(false)
                .open(path)
            {
                Ok(file) => match flock(&file, false) {
                    FlockResult::Acquired => files.push(file),
                    FlockResult::Held => {
                        // 已经拿到的先还回去，避免半持有一堆锁
                        drop(files);
                        return LockOutcome::Held {
                            path: path.clone(),
                            pid: read_pid(&file),
                        };
                    }
                    FlockResult::Failed(e) => {
                        // 文件系统不支持 flock 之类：当作没有约束，只警告
                        tracing::warn!("单实例：{} 加锁失败({e})，忽略该锁", path.display());
                        unusable += 1;
                    }
                },
                Err(e) => {
                    tracing::warn!("单实例：无法打开锁文件 {}（{e}），忽略该锁", path.display());
                    unusable += 1;
                }
            }
        }
        if files.is_empty() && unusable > 0 {
            LockOutcome::Unusable
        } else {
            LockOutcome::All(files)
        }
    }

    enum FlockResult {
        Acquired,
        Held,
        Failed(std::io::Error),
    }

    /// `flock(LOCK_EX | LOCK_NB)`。
    ///
    /// 只有 `EWOULDBLOCK`（别的进程持锁）才算"已被占用"；其它错误（文件系统不支持
    /// flock 等）另行区分，绝不能当成"已有实例"——那会让用户在某类挂载上永远启动不了。
    fn flock(file: &File, blocking: bool) -> FlockResult {
        let mut op = libc::LOCK_EX;
        if !blocking {
            op |= libc::LOCK_NB;
        }
        // SAFETY: fd 来自存活中的 File；flock 只操作这个 fd 的锁状态
        if unsafe { libc::flock(file.as_raw_fd(), op) } == 0 {
            return FlockResult::Acquired;
        }
        let err = std::io::Error::last_os_error();
        match err.raw_os_error() {
            Some(code) if code == libc::EWOULDBLOCK || code == libc::EAGAIN => FlockResult::Held,
            _ => FlockResult::Failed(err),
        }
    }

    fn read_pid(file: &File) -> Option<u32> {
        let mut buf = String::new();
        let mut f = file;
        f.seek(SeekFrom::Start(0)).ok()?;
        f.read_to_string(&mut buf).ok()?;
        buf.trim().parse().ok()
    }
}

// ───────────────────────── Windows（命名互斥体） ─────────────────────────

#[cfg(windows)]
mod imp {
    use super::SingleInstance;
    use std::path::PathBuf;
    use std::time::Duration;
    use windows_sys::Win32::Foundation::{CloseHandle, GetLastError, ERROR_ALREADY_EXISTS};
    use windows_sys::Win32::System::Threading::CreateMutexW;

    /// 命名互斥体名。用 `Local\`（本会话）而不是 `Global\`：与 Unix 侧"每个用户会话
    /// 一个实例"的尺度一致，也避免 `Global\` 需要额外权限。
    const MUTEX_NAME: &str = "Local\\screenshot-rs-single-instance";

    pub fn acquire(takeover: bool) -> Option<SingleInstance> {
        let name: Vec<u16> = MUTEX_NAME
            .encode_utf16()
            .chain(std::iter::once(0))
            .collect();
        // SAFETY: 传空 SECURITY_ATTRIBUTES = 默认安全属性；名字是以 NUL 结尾的宽字符串
        let handle = unsafe { CreateMutexW(std::ptr::null(), 0, name.as_ptr()) };
        if handle.is_null() {
            tracing::warn!("单实例：创建互斥体失败（GetLastError={}），跳过检查", unsafe {
                GetLastError()
            });
            // 拿不到互斥体的原因不是"已有实例"，不阻塞启动
            return Some(SingleInstance { files: Vec::new(), handle: 0 });
        }
        // SAFETY: GetLastError 无参数
        if unsafe { GetLastError() } == ERROR_ALREADY_EXISTS && !takeover {
            tracing::info!("已有实例在运行，本次启动退出");
            // SAFETY: handle 来自 CreateMutexW 且未被关闭
            unsafe { CloseHandle(handle) };
            return None;
        }
        Some(SingleInstance { files: Vec::new(), handle: handle as isize })
    }

    /// Windows 侧没有文件锁路径（走命名互斥体）；测试只在 Unix 上跑。
    #[allow(dead_code)]
    pub fn acquire_paths(_paths: &[PathBuf], _wait: Option<Duration>) -> Option<SingleInstance> {
        acquire(false)
    }
}

#[cfg(windows)]
impl Drop for SingleInstance {
    fn drop(&mut self) {
        if self.handle != 0 {
            // SAFETY: handle 来自 CreateMutexW，且只在这里关闭一次
            unsafe {
                windows_sys::Win32::Foundation::CloseHandle(self.handle as *mut core::ffi::c_void);
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::Path;

    /// 测试用独立锁文件：默认路径是"每会话 + 固定位置"，多个测试并行跑会互相干扰。
    fn tmp_lock(tag: &str) -> PathBuf {
        std::env::temp_dir().join(format!(
            "screenshot-rs-test-{}-{}-{tag}.lock",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.as_nanos())
                .unwrap_or(0)
        ))
    }

    /// 同一把锁：第二次必须抢不到（这正是"禁止多开"要的行为）。
    #[cfg(unix)]
    #[test]
    fn second_acquire_is_rejected() {
        let path = tmp_lock("reject");
        let first = SingleInstance::acquire_paths(&[path.clone()], None).expect("第一次应当抢到");
        assert!(
            SingleInstance::acquire_paths(&[path.clone()], None).is_none(),
            "已有实例在跑时第二次获取必须失败"
        );
        drop(first);
        let again = SingleInstance::acquire_paths(&[path.clone()], None);
        assert!(again.is_some(), "上一个实例退出后应当能重新抢到");
        drop(again);
        let _ = std::fs::remove_file(&path);
    }

    /// **多把锁的核心不变量**：任意一把被别处占着，就必须整体拒绝。
    /// 这是"桌面启动（XDG_RUNTIME_DIR 存在）"与"ssh/IDE 终端启动（该变量不存在）"
    /// 环境不一致时仍然拦得住的关键——两条路径共享固定位置那一把。
    #[cfg(unix)]
    #[test]
    fn rejection_only_needs_one_shared_path() {
        let a = tmp_lock("multi-a");
        let b = tmp_lock("multi-b");
        let other = tmp_lock("multi-other");

        // 别的实例只持有 b（模拟"它从另一种环境启动，只锁到其中一处"）
        let holder = SingleInstance::acquire_paths(&[b.clone()], None).expect("持有 b");
        assert!(
            SingleInstance::acquire_paths(&[a.clone(), b.clone()], None).is_none(),
            "只要共享的那一把被占，就必须拒绝"
        );
        // 换成只持有不相干的锁：不该影响我们
        let other_holder = SingleInstance::acquire_paths(&[other.clone()], None).expect("持有 other");
        drop(holder);
        let ok = SingleInstance::acquire_paths(&[a.clone(), b.clone()], None);
        assert!(ok.is_some(), "不相干的锁不应影响");
        drop(ok);
        drop(other_holder);
        for p in [a, b, other] {
            let _ = std::fs::remove_file(p);
        }
    }

    /// 接班人模式：旧进程还握着锁时应当**等待**它释放，而不是直接放弃
    /// （自更新/自迁移就是"spawn 新进程 → 旧进程退出"，直接放弃会让应用升级后消失）。
    #[cfg(unix)]
    #[test]
    fn takeover_waits_for_previous_instance() {
        let path = tmp_lock("takeover");
        let old = SingleInstance::acquire_paths(&[path.clone()], None).expect("旧实例应当抢到");

        let t = std::thread::spawn(move || {
            std::thread::sleep(std::time::Duration::from_millis(200));
            drop(old);
        });

        let start = std::time::Instant::now();
        let new = SingleInstance::acquire_paths(
            &[path.clone()],
            Some(std::time::Duration::from_secs(5)),
        );
        assert!(new.is_some(), "接班人应当等到旧进程让出锁");
        assert!(
            start.elapsed() >= std::time::Duration::from_millis(150),
            "应当是等待后拿到，而不是立刻拿到"
        );
        drop(new);
        let _ = t.join();
        let _ = std::fs::remove_file(&path);
    }

    /// 锁文件里写着持有者的 pid（下一次启动的日志能说清是谁占着）。
    #[cfg(unix)]
    #[test]
    fn lock_file_records_holder_pid() {
        let path = tmp_lock("pid");
        let guard = SingleInstance::acquire_paths(&[path.clone()], None).expect("应当抢到");
        let content = std::fs::read_to_string(&path).expect("锁文件应当可读");
        assert_eq!(
            content.trim().parse::<u32>().ok(),
            Some(std::process::id()),
            "锁文件里应当是当前进程的 pid"
        );
        drop(guard);
        let _ = std::fs::remove_file(&path);
    }

    /// 默认锁路径：必须有那把**与环境变量无关**的固定位置 `/tmp/...`（多开漏洞就补在这），
    /// 另外带上每会话的 `XDG_RUNTIME_DIR` 那把。
    #[test]
    fn lock_paths_include_env_independent_path() {
        let paths = SingleInstance::lock_paths();
        let canonical = PathBuf::from(format!("/tmp/screenshot-rs-{}.lock", uid()));
        assert!(
            paths.contains(&canonical),
            "必须包含固定位置 {}，实际 {:?}",
            canonical.display(),
            paths
        );
        if let Some(dir) = std::env::var_os("XDG_RUNTIME_DIR") {
            if Path::new(&dir).is_dir() {
                let session = PathBuf::from(&dir).join(format!("screenshot-rs-{}.lock", uid()));
                assert!(paths.contains(&session), "应当同时锁每会话目录，实际 {:?}", paths);
            }
        }
    }
}
