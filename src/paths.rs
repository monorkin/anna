//! Where Anna keeps her state, and where Claude Code keeps its.
//!
//! Everything durable lives under one data directory: the log, each thread's
//! session, each hand's throw-away profile. Sockets live in the runtime
//! directory, which the system clears on logout.

use std::env;
use std::path::PathBuf;

pub fn claude_config_home() -> PathBuf {
    if let Some(dir) = env::var_os("CLAUDE_CONFIG_DIR") {
        PathBuf::from(dir)
    } else {
        home().join(".claude")
    }
}

/// Where a program lives, if it is installed.
pub fn program(name: &str) -> Option<PathBuf> {
    let path = env::var_os("PATH")?;
    env::split_paths(&path)
        .map(|directory| directory.join(name))
        .find(|candidate| candidate.is_file())
}

/// What she has learned: katami's store, kept as her own rather than shared
/// with the person's other sessions.
pub fn memory_dir() -> PathBuf {
    config_dir().join("katami")
}

/// The Claude accounts she rotates between: ax's store, kept as her own.
pub fn accounts_dir() -> PathBuf {
    config_dir().join("ax")
}

/// Where the command-line tools she works through keep their config when
/// she runs them — her Basecamp profile, say. They are pointed here through
/// XDG_CONFIG_HOME, so nothing she is set up with ever lands in, or changes,
/// the person's own config for the same tool.
pub fn tools_config_home() -> PathBuf {
    config_dir().join("tools")
}

/// Points the libraries she is built on at her own folders. They read the
/// environment, and so does everything she starts — a hook or a review that
/// katami runs as `anna review …` has to find the same store she does.
pub fn claim_own_state() {
    // Before any thread exists: main calls this first
    unsafe {
        env::set_var("KATAMI_DATA_DIR", memory_dir());
        env::set_var("AX_DATA_DIR", accounts_dir());
        // So what ax tells someone to run next is a command that exists here
        env::set_var("AX_INVOKED_AS", "anna claude");
    }
    keep_to_herself(&config_dir());
    keep_to_herself(&data_dir());
}

/// Which Claude login she works as, when her config names one. Set in the
/// environment like the rest, so claude, katami and ax — and everything
/// they start in turn — all find the same one.
pub fn work_as(claude_config_dir: &std::path::Path) {
    // Before any thread exists: main calls this right after claim_own_state
    unsafe {
        env::set_var("CLAUDE_CONFIG_DIR", claude_config_dir);
    }
}

/// Her folders are closed to everyone else at the top, every time she
/// starts. What is inside — logs, sessions, the database, her tools' logins —
/// is then out of reach whatever mode a single file was made with, and an
/// Anna set up before this was done is closed too.
fn keep_to_herself(directory: &std::path::Path) {
    use std::os::unix::fs::PermissionsExt;

    if make_private_dir(directory).is_ok() {
        let _ = std::fs::set_permissions(directory, std::fs::Permissions::from_mode(0o700));
    }
}

pub fn database() -> PathBuf {
    data_dir().join("anna.db")
}

/// What a backgrounded `anna run` prints, for when she doesn't come up.
pub fn output_file() -> PathBuf {
    data_dir().join("anna.out")
}

pub fn log_file() -> PathBuf {
    data_dir().join("log.jsonl")
}

pub fn thread_dir(conversation: &str) -> PathBuf {
    data_dir().join("threads").join(conversation)
}

/// Where this process keeps the throw-away profiles of its hands and
/// reviewers. Keyed by process so a second `anna` never sweeps the first
/// one's away.
pub fn sessions_dir() -> PathBuf {
    all_sessions_dir().join(std::process::id().to_string())
}

pub fn all_sessions_dir() -> PathBuf {
    data_dir().join("sessions")
}

pub fn source_dir(name: &str) -> PathBuf {
    data_dir().join("sources").join(name)
}

/// The one socket whose name doesn't carry the process id: whoever holds it
/// is the running Anna.
pub fn control_socket() -> PathBuf {
    runtime_dir().join("control.sock")
}

/// Creates a folder that only its owner can enter. Sockets are only as
/// private as the folder they sit in, and the fallback runtime folder lives
/// under the home directory, which others can often traverse. A folder that
/// already exists is left as it is — it may not be Anna's to change.
pub fn make_private_dir(directory: &std::path::Path) -> std::io::Result<()> {
    use std::os::unix::fs::DirBuilderExt;

    std::fs::DirBuilder::new().recursive(true).mode(0o700).create(directory)
}

/// Removes the sockets of processes that are gone — this one's too, when it
/// is on its way out and says so. Every socket but the control socket ends
/// in the id of the process that made it.
pub fn sweep_sockets(including_own: bool) {
    let Ok(entries) = std::fs::read_dir(runtime_dir()) else {
        return;
    };

    for entry in entries.flatten() {
        let name = entry.file_name().to_string_lossy().into_owned();
        let owner = name.strip_suffix(".sock").and_then(|it| it.rsplit('-').next()).and_then(|it| it.parse::<u32>().ok());

        if let Some(owner) = owner {
            let own = owner == std::process::id();
            let gone = !std::path::Path::new("/proc").join(owner.to_string()).exists();
            if gone || (own && including_own) {
                let _ = std::fs::remove_file(entry.path());
            }
        }
    }
}

pub fn socket(name: &str) -> PathBuf {
    runtime_dir().join(format!("{name}-{}.sock", std::process::id()))
}

pub fn config_dir() -> PathBuf {
    if let Some(dir) = env::var_os("XDG_CONFIG_HOME").filter(|it| !it.is_empty()) {
        PathBuf::from(dir).join("anna")
    } else {
        home().join(".config/anna")
    }
}

pub fn data_dir() -> PathBuf {
    if let Some(dir) = env::var_os("XDG_DATA_HOME").filter(|it| !it.is_empty()) {
        PathBuf::from(dir).join("anna")
    } else {
        home().join(".local/share/anna")
    }
}

pub fn runtime_dir() -> PathBuf {
    if let Some(dir) = env::var_os("XDG_RUNTIME_DIR").filter(|it| !it.is_empty()) {
        PathBuf::from(dir).join("anna")
    } else {
        data_dir().join("run")
    }
}

pub fn home() -> PathBuf {
    dirs::home_dir().expect("could not determine the home directory")
}
