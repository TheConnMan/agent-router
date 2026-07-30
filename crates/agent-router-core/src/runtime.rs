use crate::Result;
use std::ffi::OsString;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::{SystemTime, UNIX_EPOCH};

/// The current user's home directory, or an empty path when no supported variable is set.
pub fn home_dir() -> PathBuf {
    let home = nonempty_var("HOME");
    let user_profile = nonempty_var("USERPROFILE");
    let native_windows_home = match (nonempty_var("HOMEDRIVE"), nonempty_var("HOMEPATH")) {
        (Some(drive), Some(path)) => {
            let mut combined = drive;
            combined.push(path);
            Some(PathBuf::from(combined))
        }
        _ => None,
    };

    #[cfg(target_os = "windows")]
    {
        user_profile
            .map(PathBuf::from)
            .or(native_windows_home)
            .or_else(|| home.map(PathBuf::from))
            .unwrap_or_default()
    }

    #[cfg(not(target_os = "windows"))]
    {
        home.map(PathBuf::from)
            .or_else(|| user_profile.map(PathBuf::from))
            .or(native_windows_home)
            .unwrap_or_default()
    }
}

fn nonempty_var(name: &str) -> Option<OsString> {
    let value = std::env::var_os(name)?;
    if value.is_empty() {
        return None;
    }
    Some(value)
}

/// `$CODEX_HOME` when set, otherwise `$HOME/.codex`.
pub fn default_codex_home() -> PathBuf {
    std::env::var_os("CODEX_HOME")
        .map(PathBuf::from)
        .unwrap_or_else(|| home_dir().join(".codex"))
}

/// Current time as epoch milliseconds.
pub fn now_ms() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|duration| duration.as_millis() as i64)
        .unwrap_or(0)
}

/// Current time as epoch seconds.
pub fn now_epoch() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|duration| duration.as_secs() as i64)
        .unwrap_or(0)
}

/// The first forty Unicode scalar values of a task.
pub fn truncated_title(task: &str) -> String {
    task.chars().take(40).collect()
}

/// The canonical directory when available, with an absolute fallback.
pub fn canonicalize_dir(path: &Path) -> PathBuf {
    std::fs::canonicalize(path).unwrap_or_else(|_| {
        if path.is_absolute() {
            path.to_path_buf()
        } else {
            std::env::current_dir()
                .map(|cwd| cwd.join(path))
                .unwrap_or_else(|_| path.to_path_buf())
        }
    })
}

pub(crate) fn router_log_path(prefix: &str) -> PathBuf {
    home_dir()
        .join(".local/state/agent-router/logs")
        .join(format!("{prefix}-{}.log", now_ms()))
}

/// Spawn a process in its own session and append its output to `log_path`.
pub fn spawn_detached(mut command: Command, log_path: &Path) -> Result<u32> {
    if let Some(parent) = log_path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let log = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(log_path)?;
    let log_error = log.try_clone()?;
    command
        .stdin(Stdio::null())
        .stdout(Stdio::from(log))
        .stderr(Stdio::from(log_error));

    #[cfg(unix)]
    {
        use std::os::unix::process::CommandExt;

        // SAFETY: setsid is async signal safe and is the only operation before exec.
        unsafe {
            command.pre_exec(|| {
                libc::setsid();
                Ok(())
            });
        }
    }

    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;

        const DETACHED_PROCESS: u32 = 0x0000_0008;
        const CREATE_NEW_PROCESS_GROUP: u32 = 0x0000_0200;
        command.creation_flags(DETACHED_PROCESS | CREATE_NEW_PROCESS_GROUP);
    }

    Ok(command.spawn()?.id())
}
