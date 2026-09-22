mod backup;
mod breaker;
mod broker;
mod claude;
mod clock;
mod config;
mod control;
mod conversation;
mod deadline;
mod dispatcher;
mod editor;
mod fsutil;
mod hand;
mod judge;
mod lifecycle;
mod logs;
mod mcp;
mod mcp_cli;
mod paths;
mod prompt;
mod proxy;
mod reviewer;
mod runtime;
mod sandbox;
mod schedule_tools;
mod secrets;
mod service;
mod setup;
mod source;
mod source_cli;
mod store;
mod thread;
mod thread_tools;
mod toolchains;
mod work_tools;

use anyhow::Result;
use std::sync::Arc;
use usage::{Cli, Subcommands};

use crate::conversation::{Standing, Terminal};
use crate::runtime::Runtime;

/// A self-governing agent you work with like a colleague
#[derive(Cli)]
#[usage(bin = "anna", version, unknown_flags = "error")]
struct Cli {
    #[usage(subcommand)]
    command: Command,
}

#[derive(Subcommands)]
enum Command {
    /// Walk through the one-time setup; safe to run again
    Setup,
    /// Bring Anna up in the background, through systemd when the service is installed
    Start,
    /// Stop Anna and everything she started
    Stop,
    /// Show whether Anna is running and what she's listening on
    Status,
    /// Have Anna check a source now instead of at the next tick; all of them when none is named
    Poke { source: Option<String> },
    /// Run Anna in this terminal: listen on every source until stopped
    Run,
    /// Write everything that makes this Anna this Anna to a zip; safe while she runs
    Backup {
        /// Where to write the zip; defaults to anna-backup-<time>.zip here
        #[usage(long)]
        to: Option<std::path::PathBuf>,
        /// Leave her tokens and logins out of the zip: Jev's key, the Claude accounts, her tools' own config
        #[usage(long)]
        without_secrets: bool,
    },
    /// Bring an Anna back from a backup zip; she has to be stopped
    Restore {
        path: std::path::PathBuf,
        /// Replace the Anna that is already set up here
        #[usage(long)]
        force: bool,
    },
    /// Talk to Anna from this terminal
    Chat {
        message: String,
        /// Which terminal conversation this belongs to
        #[usage(long, default = "main")]
        conversation: String,
    },
    /// Show what Anna has been doing
    Log {
        /// Keep printing as things happen
        #[usage(short = 'f', long)]
        follow: bool,
        /// Only the lines of one thread, hand or conversation; any part of its id
        #[usage(long)]
        only: Option<String>,
    },
    /// Manage the MCP servers Anna works through
    Mcp {
        #[usage(subcommand)]
        command: McpCommand,
    },
    /// Look at the places Anna listens
    Source {
        #[usage(subcommand)]
        command: SourceCommand,
    },
    /// Inspect and manage what Anna remembers
    Memory {
        #[usage(subcommand)]
        command: katami::cli::MemoryCommand,
    },
    /// Manage the Claude subscriptions Anna works with
    Claude {
        #[usage(subcommand)]
        command: ClaudeCommand,
    },
}

#[derive(Subcommands)]
enum SourceCommand {
    /// Read a source once and show what Anna would make of it, without acting on anything
    Check { name: String },
}

#[derive(Subcommands)]
enum ClaudeCommand {
    /// Manage the accounts in the rotation
    Account {
        #[usage(subcommand)]
        command: ax::cli::AccountCommand,
    },
}

#[derive(Subcommands)]
enum McpCommand {
    /// Add a server, or accept the changed tools of one already added
    Add {
        name: String,
        /// The command that starts the server
        #[usage(double_dash = "required")]
        server_command: Vec<String>,
    },
    /// List servers and their tools
    List,
    /// Remove a server
    Remove { name: String },
    /// Mark a tool's argument as prose for people, so the editor restyles it
    Prose {
        name: String,
        tool: String,
        argument: String,
    },
}

fn main() {
    // A config that can't be read is the command's to complain about
    let claude_config_dir = config::Config::load().ok().and_then(|it| it.claude_config_dir);
    paths::claim_own_state(claude_config_dir);

    // Rust ignores SIGPIPE, which turns `anna log | head` into a panic, so
    // the commands that print and leave get the usual behaviour back. The
    // ones that talk to MCP servers don't: there a closed pipe has to come
    // back as an error, or a server that died takes the whole of Anna with
    // it at the next write.
    let talks_to_servers = matches!(std::env::args().nth(1).as_deref(), Some("run" | "chat"));
    if !talks_to_servers {
        unsafe {
            libc::signal(libc::SIGPIPE, libc::SIG_DFL);
        }
    }

    let result = match std::env::args().nth(1).as_deref() {
        Some(command) if KATAMI_RUNS_ITSELF_AS.contains(&command) => katami::cli::run(katami::cli::Cli::parse()),
        _ => run(Cli::parse()),
    };
    if let Err(error) = result {
        eprintln!("error: {error:#}");
        std::process::exit(1);
    }
}

/// katami runs its own executable for these: the hooks it registers with
/// claude, and the reviews and curation it spawns in the background. Inside
/// Anna that executable is Anna, so she hands them straight to katami.
const KATAMI_RUNS_ITSELF_AS: [&str; 3] = ["hook", "review", "curate"];

fn run(cli: Cli) -> Result<()> {
    match cli.command {
        Command::Setup => setup::run(),
        Command::Start => lifecycle::start(),
        Command::Stop => lifecycle::stop(),
        Command::Status => lifecycle::status(),
        Command::Poke { source } => lifecycle::poke(source.as_deref()),
        Command::Run => dispatcher::run(),
        Command::Backup { to, without_secrets } => backup::backup(to, without_secrets),
        Command::Restore { path, force } => backup::restore(&path, force),
        Command::Chat { message, conversation } => {
            // Whoever is at this terminal can already do anything Anna can
            thread::wake(&Runtime::start()?, Arc::new(Terminal::new(&conversation)), Standing::Trusted, &message)
        }
        Command::Log { follow, only } => logs::print(follow, only.as_deref()),
        Command::Mcp { command } => match command {
            McpCommand::Add { name, server_command } => mcp_cli::add(&name, &server_command),
            McpCommand::List => mcp_cli::list(),
            McpCommand::Remove { name } => mcp_cli::remove(&name),
            McpCommand::Prose { name, tool, argument } => mcp_cli::mark_prose(&name, &tool, &argument),
        },
        Command::Source { command } => match command {
            SourceCommand::Check { name } => source_cli::check(&name),
        },
        Command::Memory { command } => katami::cli::run(katami::cli::Cli {
            command: katami::cli::Command::Memory { command },
        }),
        Command::Claude { command } => match command {
            ClaudeCommand::Account { command } => ax::cli::run(ax::cli::Cli {
                command: ax::cli::Command::Account { command },
            }),
        },
    }
}
