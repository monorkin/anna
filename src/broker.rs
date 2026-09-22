//! The broker: the MCP server every session talks to.
//!
//! Each session gets its own endpoint — a unix socket serving exactly the
//! tools that session was granted. Which socket a call arrives on is who is
//! calling, so there are no tokens to leak or forge. A session reaches its
//! endpoint through socat as a stdio MCP server, which works the same from
//! inside a sandbox.
//!
//! The protocol is MCP's stdio framing, one JSON-RPC message per line, and
//! only the part sessions use: initialize, tools/list, tools/call, ping.

use anyhow::{Context, Result};
use serde_json::{Value, json};
use std::fs;
use std::io::{BufRead, BufReader, Write};
use std::os::unix::net::{UnixListener, UnixStream};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::thread;
use std::time::Instant;

use crate::logs;
use crate::paths;

const PROTOCOL_VERSION: &str = "2025-06-18";

pub trait Tool: Send + Sync {
    fn name(&self) -> &str;
    fn description(&self) -> &str;
    fn input_schema(&self) -> Value;
    fn call(&self, arguments: &Value) -> Result<String>;
}

pub struct Endpoint {
    socket: PathBuf,
    closed: Arc<AtomicBool>,
}

impl Endpoint {
    pub fn open(socket: &Path, tools: Vec<Box<dyn Tool>>) -> Result<Endpoint> {
        if let Some(directory) = socket.parent() {
            paths::make_private_dir(directory)?;
        }
        let _ = fs::remove_file(socket);
        let listener = UnixListener::bind(socket)
            .with_context(|| format!("could not listen on {}", socket.display()))?;
        let tools = Arc::new(tools);
        let closed = Arc::new(AtomicBool::new(false));
        // The socket is named for the session it serves — thread-<key>,
        // hand-<id> — which is who a call in the log belongs to
        let who: Arc<str> = Arc::from(socket.file_stem().unwrap_or_default().to_string_lossy().as_ref());

        let closed_for_listening = closed.clone();
        thread::spawn(move || {
            for session in listener.incoming().flatten() {
                if closed_for_listening.load(Ordering::Relaxed) {
                    break;
                }
                let tools = Arc::clone(&tools);
                let closed = closed_for_listening.clone();
                let who = who.clone();
                thread::spawn(move || {
                    let _ = serve(session, &tools, &closed, &who);
                });
            }
        });

        Ok(Endpoint {
            socket: socket.to_path_buf(),
            closed,
        })
    }

    pub fn socket(&self) -> &Path {
        &self.socket
    }
}

/// The `--mcp-config` that points a session at an endpoint. Inside a sandbox
/// the socket is bound somewhere else, so the path is the caller's to give.
pub fn mcp_config(socket: &Path) -> String {
    json!({ "mcpServers": { "anna": {
        "command": "socat",
        "args": ["-", format!("UNIX-CONNECT:{}", socket.display())],
    }}})
    .to_string()
}

/// An endpoint is a session's grant, and it ends with the session: the
/// listener stops — woken by one last connection, since nothing else stops
/// an accept — and whoever is still connected is served nothing more.
/// Without this every turn would leave a thread, a socket and its tools
/// behind for as long as Anna runs.
impl Drop for Endpoint {
    fn drop(&mut self) {
        self.closed.store(true, Ordering::Relaxed);
        let _ = UnixStream::connect(&self.socket);
        let _ = fs::remove_file(&self.socket);
    }
}

fn serve(session: UnixStream, tools: &[Box<dyn Tool>], closed: &AtomicBool, who: &str) -> Result<()> {
    let mut writer = session.try_clone()?;
    for line in BufReader::new(session).lines() {
        let line = line?;
        if closed.load(Ordering::Relaxed) {
            break;
        }
        if line.trim().is_empty() {
            continue;
        }
        let message: Value = serde_json::from_str(&line)?;
        if let Some(response) = respond(&message, tools, who) {
            writeln!(writer, "{response}")?;
        }
    }
    Ok(())
}

fn respond(message: &Value, tools: &[Box<dyn Tool>], who: &str) -> Option<Value> {
    let id = message.get("id")?.clone();
    let method = message["method"].as_str().unwrap_or_default();

    let outcome = match method {
        "initialize" => Ok(json!({
            "protocolVersion": PROTOCOL_VERSION,
            "capabilities": { "tools": {} },
            "serverInfo": { "name": "anna", "version": env!("CARGO_PKG_VERSION") },
        })),
        "ping" => Ok(json!({})),
        "tools/list" => Ok(json!({ "tools": tools.iter().map(|it| listing(it.as_ref())).collect::<Vec<_>>() })),
        "tools/call" => Ok(call(&message["params"], tools, who)),
        _ => Err(json!({ "code": -32601, "message": format!("unknown method {method}") })),
    };

    Some(match outcome {
        Ok(result) => json!({ "jsonrpc": "2.0", "id": id, "result": result }),
        Err(error) => json!({ "jsonrpc": "2.0", "id": id, "error": error }),
    })
}

fn listing(tool: &dyn Tool) -> Value {
    json!({
        "name": tool.name(),
        "description": tool.description(),
        "inputSchema": tool.input_schema(),
    })
}

fn call(params: &Value, tools: &[Box<dyn Tool>], who: &str) -> Value {
    let name = params["name"].as_str().unwrap_or_default();
    let began = Instant::now();
    let outcome = match tools.iter().find(|it| it.name() == name) {
        Some(tool) => tool.call(&params["arguments"]),
        None => Err(anyhow::anyhow!("there is no tool called {name} here")),
    };
    let took = began.elapsed().as_millis();

    match outcome {
        Ok(text) => {
            logs::event("broker.called", json!({ "by": who, "tool": name, "ms": took }));
            json!({ "content": [{ "type": "text", "text": text }], "isError": false })
        }
        Err(error) => {
            logs::event("broker.refused", json!({ "by": who, "tool": name, "ms": took, "error": format!("{error:#}") }));
            json!({ "content": [{ "type": "text", "text": format!("{error:#}") }], "isError": true })
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    struct Echo;

    impl Tool for Echo {
        fn name(&self) -> &str {
            "echo"
        }

        fn description(&self) -> &str {
            "Says it back"
        }

        fn input_schema(&self) -> Value {
            json!({ "type": "object", "properties": { "text": { "type": "string" } }, "required": ["text"] })
        }

        fn call(&self, arguments: &Value) -> Result<String> {
            arguments["text"]
                .as_str()
                .map(|it| it.to_uppercase())
                .context("text is required")
        }
    }

    fn exchange(stream: &mut UnixStream, reader: &mut BufReader<UnixStream>, message: Value) -> Value {
        writeln!(stream, "{message}").unwrap();
        let mut line = String::new();
        reader.read_line(&mut line).unwrap();
        serde_json::from_str(&line).unwrap()
    }

    #[test]
    fn a_session_lists_and_calls_the_tools_it_was_given() {
        let socket = std::env::temp_dir().join(format!("anna-broker-{}.sock", std::process::id()));
        let endpoint = Endpoint::open(&socket, vec![Box::new(Echo)]).unwrap();
        let mut stream = UnixStream::connect(&socket).unwrap();
        let mut reader = BufReader::new(stream.try_clone().unwrap());

        let hello = exchange(&mut stream, &mut reader, json!({ "jsonrpc": "2.0", "id": 1, "method": "initialize", "params": {} }));
        assert_eq!(hello["result"]["serverInfo"]["name"], "anna");

        writeln!(stream, "{}", json!({ "jsonrpc": "2.0", "method": "notifications/initialized" })).unwrap();

        let list = exchange(&mut stream, &mut reader, json!({ "jsonrpc": "2.0", "id": 2, "method": "tools/list" }));
        assert_eq!(list["result"]["tools"][0]["name"], "echo");

        let called = exchange(&mut stream, &mut reader, json!({ "jsonrpc": "2.0", "id": 3, "method": "tools/call", "params": { "name": "echo", "arguments": { "text": "hi" } } }));
        assert_eq!(called["result"]["content"][0]["text"], "HI");
        assert_eq!(called["result"]["isError"], false);

        let failed = exchange(&mut stream, &mut reader, json!({ "jsonrpc": "2.0", "id": 4, "method": "tools/call", "params": { "name": "echo", "arguments": {} } }));
        assert_eq!(failed["result"]["isError"], true);

        let missing = exchange(&mut stream, &mut reader, json!({ "jsonrpc": "2.0", "id": 5, "method": "tools/call", "params": { "name": "rm", "arguments": {} } }));
        assert_eq!(missing["result"]["content"][0]["text"], "there is no tool called rm here");

        let unknown = exchange(&mut stream, &mut reader, json!({ "jsonrpc": "2.0", "id": 6, "method": "resources/list" }));
        assert_eq!(unknown["error"]["code"], -32601);

        assert!(mcp_config(endpoint.socket()).contains(&format!("UNIX-CONNECT:{}", socket.display())));

        drop(endpoint);
        assert!(!socket.exists());
        writeln!(stream, "{}", json!({ "jsonrpc": "2.0", "id": 7, "method": "tools/call", "params": { "name": "echo", "arguments": { "text": "hi" } } })).unwrap();
        let mut line = String::new();
        assert_eq!(reader.read_line(&mut line).unwrap(), 0, "a session still connected after its endpoint is gone is served nothing more");
    }
}
