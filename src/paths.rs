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

pub fn log_file() -> PathBuf {
    data_dir().join("log.jsonl")
}

pub fn thread_dir(conversation: &str) -> PathBuf {
    data_dir().join("threads").join(conversation)
}

pub fn hand_dir(id: &str) -> PathBuf {
    data_dir().join("hands").join(id)
}

pub fn socket(name: &str) -> PathBuf {
    runtime_dir().join(format!("{name}.sock"))
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

fn runtime_dir() -> PathBuf {
    if let Some(dir) = env::var_os("XDG_RUNTIME_DIR").filter(|it| !it.is_empty()) {
        PathBuf::from(dir).join("anna")
    } else {
        data_dir().join("run")
    }
}

fn home() -> PathBuf {
    dirs::home_dir().expect("could not determine the home directory")
}
