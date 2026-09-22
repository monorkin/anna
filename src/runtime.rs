//! What one `anna` process builds once and every thread shares: the judge,
//! the editor, the MCP servers, and the proxy hands reach Claude through.
//!
//! Starting up also sweeps away what dead processes left behind. A hand's
//! profile holds a live access token, and a process that was killed never got
//! to delete it.

use anyhow::Result;
use serde_json::json;
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use crate::config::{self, Config};
use crate::editor::Editor;
use crate::judge::Judge;
use crate::logs;
use crate::mcp::Catalog;
use crate::paths;
use crate::proxy::{self, Proxy};
use crate::sandbox::Outside;
use crate::store::Store;
use crate::toolchains::Toolchains;

pub struct Runtime {
    pub config: Config,
    pub database: PathBuf,
    pub personality: Option<String>,
    /// How she writes: the editor holds her to it after the fact, and every
    /// thread is told it up front, so most of what she writes needs no
    /// rewriting.
    pub style: Option<String>,
    pub judge: Arc<Judge>,
    pub editor: Arc<Editor>,
    pub catalog: Arc<Catalog>,
    pub outside: Arc<Outside>,
    _proxy: Proxy,
}

impl Runtime {
    pub fn start() -> Result<Runtime> {
        sweep(&paths::all_sessions_dir());
        paths::sweep_sockets(false);

        let config = Config::load()?;
        let judge = Arc::new(Judge::with_whatever_is_set_up(&config));
        let style = config::style()?;
        let editor = Arc::new(Editor::new(style.clone(), judge.clone()));
        let personality = config::personality()?;
        let catalog = Arc::new(Catalog::open(&config, editor.clone(), judge.clone()));
        let proxy = Proxy::start(&paths::socket("proxy"), &[proxy::CLAUDE_API])?;
        let outside = Arc::new(Outside {
            proxy_socket: proxy.socket().to_path_buf(),
            toolchains: Toolchains::discover(),
            time_limit: Duration::from_secs(config.minutes_per_run * 60),
            scopes: Outside::can_have_scopes(),
        });
        if !outside.scopes {
            logs::event("sandbox.without_limits", json!({ "reason": "the user's systemd gave no scope" }));
        }

        let database = paths::database();
        Store::open_at(&database)?;

        Ok(Runtime {
            config,
            database,
            personality,
            style,
            judge,
            editor,
            catalog,
            outside,
            _proxy: proxy,
        })
    }
}

fn sweep(all_sessions: &Path) {
    let Ok(entries) = fs::read_dir(all_sessions) else {
        return;
    };

    for entry in entries.flatten() {
        let process = entry.file_name().to_string_lossy().into_owned();
        if !Path::new("/proc").join(&process).exists() {
            let _ = fs::remove_dir_all(entry.path());
            logs::event("sessions.swept", json!({ "process": process }));
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn only_dead_processes_are_swept() {
        let all_sessions = std::env::temp_dir().join(format!("anna-sweep-{}", std::process::id()));
        let mine = all_sessions.join(std::process::id().to_string());
        let dead = all_sessions.join("4194305");
        fs::create_dir_all(mine.join("h1/profile")).unwrap();
        fs::create_dir_all(dead.join("h2/profile")).unwrap();

        sweep(&all_sessions);

        assert!(mine.join("h1/profile").exists());
        assert!(!dead.exists());
        fs::remove_dir_all(all_sessions).unwrap();
    }
}
