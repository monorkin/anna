mod broker;
mod claude;
mod clock;
mod config;
mod conversation;
mod dispatcher;
mod editor;
mod hand;
mod judge;
mod logs;
mod mcp;
mod mcp_cli;
mod paths;
mod proxy;
mod reviewer;
mod runtime;
mod sandbox;
mod setup;
mod source;
mod thread;
mod thread_tools;
mod toolchains;

use anyhow::Result;
use std::sync::Arc;
use usage::{Cli, Subcommands};

use crate::conversation::Terminal;
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
    /// Start Anna: listen on every source until stopped
    Run,
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
    },
    /// Manage the MCP servers Anna works through
    Mcp {
        #[usage(subcommand)]
        command: McpCommand,
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
    // Rust ignores SIGPIPE, which turns `anna log | head` into a panic
    unsafe {
        libc::signal(libc::SIGPIPE, libc::SIG_DFL);
    }

    if let Err(error) = run(Cli::parse()) {
        eprintln!("error: {error:#}");
        std::process::exit(1);
    }
}

fn run(cli: Cli) -> Result<()> {
    match cli.command {
        Command::Setup => setup::run(),
        Command::Run => dispatcher::run(),
        Command::Chat { message, conversation } => {
            thread::wake(&Runtime::start()?, Arc::new(Terminal::new(&conversation)), &message)
        }
        Command::Log { follow } => logs::print(follow),
        Command::Mcp { command } => match command {
            McpCommand::Add { name, server_command } => mcp_cli::add(&name, &server_command),
            McpCommand::List => mcp_cli::list(),
            McpCommand::Remove { name } => mcp_cli::remove(&name),
            McpCommand::Prose { name, tool, argument } => mcp_cli::mark_prose(&name, &tool, &argument),
        },
    }
}
