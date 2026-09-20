mod broker;
mod claude;
mod clock;
mod config;
mod conversation;
mod editor;
mod hand;
mod judge;
mod logs;
mod mcp;
mod mcp_cli;
mod paths;
mod proxy;
mod thread;
mod thread_tools;

use anyhow::Result;
use std::sync::Arc;
use usage::{Cli, Subcommands};

use crate::conversation::Terminal;

/// A self-governing agent you work with like a colleague
#[derive(Cli)]
#[usage(bin = "anna", version, unknown_flags = "error")]
struct Cli {
    #[usage(subcommand)]
    command: Command,
}

#[derive(Subcommands)]
enum Command {
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
    if let Err(error) = run(Cli::parse()) {
        eprintln!("error: {error:#}");
        std::process::exit(1);
    }
}

fn run(cli: Cli) -> Result<()> {
    match cli.command {
        Command::Chat { message, conversation } => {
            thread::wake(Arc::new(Terminal::new(&conversation)), &message)
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
