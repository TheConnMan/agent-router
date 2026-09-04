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

/// A human-readable three to five word background-job title.
///
/// The control-only `BACKGROUND_RUN=1` line never enters the title. When the task names a ticket,
/// the ticket is retained and followed by two to four words from the task description.
pub fn short_job_name(task: &str) -> String {
    let is_implement = task.trim_start().starts_with("/implement");
    let mut ticket = None;
    let mut words = Vec::new();

    for line in task.lines().filter(|line| {
        let line = line.trim();
        !line.is_empty() && line != "BACKGROUND_RUN=1"
    }) {
        for raw in line.split(|character: char| !(character.is_alphanumeric() || character == '-'))
        {
            if raw.is_empty() {
                continue;
            }
            if ticket.is_none() && is_ticket(raw) {
                ticket = Some(raw.to_string());
                continue;
            }
            for word in raw.split('-').filter(|word| !word.is_empty()) {
                if is_implement && words.is_empty() && word.eq_ignore_ascii_case("implement") {
                    continue;
                }
                words.push(title_case(word));
            }
        }
    }

    let limit = if ticket.is_some() { 4 } else { 5 };
    words.truncate(limit);
    if ticket.is_some() {
        for fallback in ["Implement", "Task"] {
            if words.len() >= 2 {
                break;
            }
            words.push(fallback.to_string());
        }
    } else {
        if words.is_empty() {
            words.extend(["Background", "Work", "Item"].map(str::to_string));
        }
        for fallback in ["Background", "Job"] {
            if words.len() >= 3 {
                break;
            }
            words.push(fallback.to_string());
        }
    }

    ticket
        .into_iter()
        .chain(words)
        .collect::<Vec<_>>()
        .join(" ")
}

/// Validate and normalize a title returned by the classifier model.
///
/// A ticket in the task must lead the title. The remaining title must contain two to six words,
/// and punctuation is rejected so a model explanation cannot become a session name.
pub fn validate_job_name(task: &str, candidate: &str) -> Option<String> {
    let words: Vec<&str> = candidate.split_whitespace().collect();
    if words.is_empty() || words.iter().any(|word| !valid_title_word(word)) {
        return None;
    }

    let ticket = task
        .split(|character: char| !(character.is_alphanumeric() || character == '-'))
        .find(|word| is_ticket(word));
    let description = if let Some(ticket) = ticket {
        if words.first().copied() != Some(ticket) {
            return None;
        }
        &words[1..]
    } else {
        words.as_slice()
    };
    if !(2..=6).contains(&description.len()) {
        return None;
    }

    let mut normalized = Vec::with_capacity(words.len());
    if let Some(ticket) = ticket {
        normalized.push(ticket.to_string());
        normalized.extend(description.iter().map(|word| title_case(word)));
    } else {
        normalized.extend(words.iter().map(|word| title_case(word)));
    }
    Some(normalized.join(" "))
}

fn valid_title_word(word: &str) -> bool {
    word.chars()
        .all(|character| character.is_alphanumeric() || character == '-')
}

fn is_ticket(value: &str) -> bool {
    let Some((prefix, number)) = value.split_once('-') else {
        return false;
    };
    (2..=6).contains(&prefix.len())
        && prefix.bytes().all(|byte| byte.is_ascii_uppercase())
        && !number.is_empty()
        && number.bytes().all(|byte| byte.is_ascii_digit())
}

fn title_case(word: &str) -> String {
    let mut characters = word.chars();
    let Some(first) = characters.next() else {
        return String::new();
    };
    first.to_uppercase().chain(characters).collect()
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

pub(crate) fn router_log_path(home: &Path, prefix: &str) -> PathBuf {
    home.join(".local/state/agent-router/logs")
        .join(format!("{prefix}-{}.log", now_ms()))
}

/// Spawn a process in its own session and append its output to `log_path`.
///
/// `override_env` names the `AGENT_ROUTER_*_BIN` variable that pins the program being spawned,
/// and is what turns an `ENOENT` here into a named `Error::Launch`. `None` is for spawns that
/// are not provider CLIs: those keep `Error::Io`. See
/// docs/decisions/0005-launch-error-and-binary-resolver.md.
pub fn spawn_detached(
    mut command: Command,
    log_path: &Path,
    override_env: Option<&str>,
) -> Result<u32> {
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

    // The program is read before the spawn so the failure can name it: `Command` is consumed by
    // the borrow the spawn takes, and the mapper needs the path either way.
    let program = PathBuf::from(command.get_program());
    command
        .spawn()
        .map(|child| child.id())
        .map_err(|error| match override_env {
            Some(override_env) => crate::binary::launch_error(&program, override_env, error),
            None => crate::Error::Io(error),
        })
}

#[cfg(test)]
mod tests {
    use super::{short_job_name, validate_job_name};

    #[test]
    fn an_implement_prompt_uses_its_ticket_and_a_short_task_title() {
        assert_eq!(
            short_job_name("/implement RS-123 rename background sessions\nBACKGROUND_RUN=1"),
            "RS-123 Rename Background Sessions"
        );
    }

    #[test]
    fn a_plain_background_prompt_has_a_three_to_five_word_title() {
        assert_eq!(
            short_job_name("audit scheduled background agents"),
            "Audit Scheduled Background Agents"
        );
    }

    #[test]
    fn a_ticket_without_a_description_keeps_a_useful_fallback_title() {
        assert_eq!(short_job_name("/implement GH-432"), "GH-432 Implement Task");
    }

    #[test]
    fn a_model_title_keeps_the_task_ticket_and_normalizes_title_case() {
        assert_eq!(
            validate_job_name(
                "/implement RS-123 search the input box",
                "RS-123 input box search"
            ),
            Some("RS-123 Input Box Search".to_string())
        );
    }

    #[test]
    fn a_model_title_must_start_with_the_task_ticket_and_have_two_to_six_description_words() {
        assert_eq!(
            validate_job_name("/implement GH-123 fix bugs", "Fix Bugs GH-123"),
            None
        );
        assert_eq!(
            validate_job_name("/implement GH-123 fix bugs", "GH-123 Fix"),
            None
        );
        assert_eq!(
            validate_job_name("audit the scheduler", "Audit The Scheduler"),
            Some("Audit The Scheduler".to_string())
        );
    }
}
