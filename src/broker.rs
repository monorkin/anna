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
use std::thread;

use crate::logs;

const PROTOCOL_VERSION: &str = "2025-06-18";

pub trait Tool: Send + Sync {
    fn name(&self) -> &str;
    fn description(&self) -> &str;
    fn input_schema(&self) -> Value;
    fn call(&self, arguments: &Value) -> Result<String>;
}

pub struct Endpoint {
    socket: PathBuf,
}

impl Endpoint {
    pub fn open(socket: &Path, tools: Vec<Box<dyn Tool>>) -> Result<Endpoint> {
        if let Some(directory) = socket.parent() {
            fs::create_dir_all(directory)?;
        }
        let _ = fs::remove_file(socket);
        let listener = UnixListener::bind(socket)
            .with_context(|| format!("could not listen on {}", socket.display()))?;
        let tools = Arc::new(tools);

        thread::spawn(move || {
            for session in listener.incoming().flatten() {
                let tools = Arc::clone(&tools);
                thread::spawn(move || {
                    let _ = serve(session, &tools);
                });
            }
        });

        Ok(Endpoint {
            socket: socket.to_path_buf(),
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

impl Drop for Endpoint {
    fn drop(&mut self) {
        let _ = fs::remove_file(&self.socket);
    }
}

fn serve(session: UnixStream, tools: &[Box<dyn Tool>]) -> Result<()> {
    let mut writer = session.try_clone()?;
    for line in BufReader::new(session).lines() {
        let line = line?;
        if line.trim().is_empty() {
            continue;
        }
        let message: Value = serde_json::from_str(&line)?;
        if let Some(response) = respond(&message, tools) {
            writeln!(writer, "{response}")?;
        }
    }
    Ok(())
}

fn respond(message: &Value, tools: &[Box<dyn Tool>]) -> Option<Value> {
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
        "tools/call" => Ok(call(&message["params"], tools)),
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

fn call(params: &Value, tools: &[Box<dyn Tool>]) -> Value {
    let name = params["name"].as_str().unwrap_or_default();
    logs::event("broker.called", json!({ "tool": name }));

    let outcome = match tools.iter().find(|it| it.name() == name) {
        Some(tool) => tool.call(&params["arguments"]),
        None => Err(anyhow::anyhow!("there is no tool called {name} here")),
    };

    match outcome {
        Ok(text) => json!({ "content": [{ "type": "text", "text": text }], "isError": false }),
        Err(error) => json!({ "content": [{ "type": "text", "text": format!("{error:#}") }], "isError": true }),
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
    }
}
