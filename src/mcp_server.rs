//! One MCP server as a child process: started, spoken to over JSON-RPC on
//! its stdin and stdout, and started again when it dies or stops answering.
//!
//! A server's tool listing is fingerprinted when it is added. A server whose
//! listing has changed since — a description that grew an instruction, say —
//! is left out until it is added again.

use anyhow::{Context, Result, bail};
use serde_json::{Value, json};
use std::io::{BufRead, BufReader, Write};
use std::process::{Child, ChildStdin, ChildStdout, Command, Stdio};
use std::sync::mpsc::{self, Receiver, RecvTimeoutError};
use std::time::{Duration, Instant};

use crate::config::{self, McpServer};
use crate::logs;

/// How long a server gets to answer. Everyone who uses a server waits in
/// line for it, so one that never answers would otherwise stop them all.
const SECONDS_TO_ANSWER: u64 = 120;

pub struct Server {
    settings: McpServer,
    child: Child,
    stdin: ChildStdin,
    lines: Receiver<String>,
    next_id: u64,
    patience: Duration,
    gone: bool,
}

impl Server {
    pub fn start(settings: &McpServer) -> Result<Server> {
        Server::start_with_patience(settings, Duration::from_secs(SECONDS_TO_ANSWER))
    }

    fn start_with_patience(settings: &McpServer, patience: Duration) -> Result<Server> {
        let mut child = Command::new(&settings.command)
            .args(&settings.args)
            .envs(config::environment(&settings.env))
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .spawn()
            .with_context(|| format!("could not start {}", settings.command))?;

        let mut server = Server {
            settings: settings.clone(),
            stdin: child.stdin.take().context("the server has no stdin")?,
            lines: lines_of(child.stdout.take().context("the server has no stdout")?),
            child,
            next_id: 0,
            patience,
            gone: false,
        };
        server.request(
            "initialize",
            json!({
                "protocolVersion": "2025-06-18",
                "capabilities": {},
                "clientInfo": { "name": "anna", "version": env!("CARGO_PKG_VERSION") },
            }),
        )?;
        server.send(&json!({ "jsonrpc": "2.0", "method": "notifications/initialized" }))?;
        Ok(server)
    }

    pub fn tools(&mut self) -> Result<Vec<Value>> {
        let listing = self.request("tools/list", json!({}))?;
        Ok(listing["tools"].as_array().cloned().unwrap_or_default())
    }

    pub fn call(&mut self, tool: &str, arguments: &Value) -> Result<String> {
        if self.gone {
            self.start_again()?;
        }
        let result = self.request("tools/call", json!({ "name": tool, "arguments": arguments }))?;
        let text = result["content"]
            .as_array()
            .map(|items| {
                items
                    .iter()
                    .filter_map(|item| item["text"].as_str())
                    .collect::<Vec<_>>()
                    .join("\n")
            })
            .unwrap_or_default();

        if result["isError"].as_bool().unwrap_or(false) {
            bail!("{text}");
        }
        Ok(text)
    }

    /// A server that died or stopped answering is started again for the next
    /// call, as long as it still offers what it offered when it was added.
    fn start_again(&mut self) -> Result<()> {
        let mut fresh = Server::start_with_patience(&self.settings, self.patience)
            .with_context(|| format!("{} went away and did not start again", self.settings.command))?;
        if let Some(expected) = &self.settings.fingerprint
            && fingerprint(&fresh.tools()?) != *expected
        {
            bail!("{} went away, and came back with different tools; add it again to accept them", self.settings.command);
        }
        logs::event("mcp.started_again", json!({ "command": self.settings.command }));
        *self = fresh;
        Ok(())
    }

    fn request(&mut self, method: &str, params: Value) -> Result<Value> {
        self.next_id += 1;
        let id = self.next_id;
        self.send(&json!({ "jsonrpc": "2.0", "id": id, "method": method, "params": params }))?;

        let deadline = Instant::now() + self.patience;
        loop {
            let line = match self.lines.recv_timeout(deadline.saturating_duration_since(Instant::now())) {
                Ok(line) => line,
                Err(RecvTimeoutError::Timeout) => {
                    self.give_up();
                    bail!("the server did not answer {method} within {} seconds", self.patience.as_secs());
                }
                Err(RecvTimeoutError::Disconnected) => {
                    self.give_up();
                    bail!("the server went away during {method}");
                }
            };
            let Ok(message) = serde_json::from_str::<Value>(&line) else {
                continue;
            };
            if message["id"] == json!(id) {
                return match message.get("error") {
                    Some(error) => bail!("{method} failed: {error}"),
                    None => Ok(message["result"].clone()),
                };
            }
        }
    }

    fn send(&mut self, message: &Value) -> Result<()> {
        let sent = writeln!(self.stdin, "{message}").and_then(|_| self.stdin.flush());
        if sent.is_err() {
            self.give_up();
        }
        sent.context("the server is not listening any more")
    }

    /// A server that missed its deadline is more likely stuck than slow, and
    /// everyone is waiting in line behind it: it is stopped, and the next
    /// call starts a new one.
    fn give_up(&mut self) {
        self.gone = true;
        let _ = self.child.kill();
    }
}

fn lines_of(output: ChildStdout) -> Receiver<String> {
    let (sender, receiver) = mpsc::channel();
    std::thread::spawn(move || {
        for line in BufReader::new(output).lines().map_while(|line| line.ok()) {
            if sender.send(line).is_err() {
                break;
            }
        }
    });
    receiver
}

impl Drop for Server {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

pub fn fingerprint_of(settings: &McpServer) -> Result<String> {
    let mut server = Server::start(settings)?;
    Ok(fingerprint(&server.tools()?))
}

/// The server and its tools, when they are still the ones it was added
/// with.
pub fn listed(settings: &McpServer) -> Result<(Server, Vec<Value>)> {
    let mut server = Server::start(settings)?;
    let listing = server.tools()?;
    if settings.fingerprint.as_deref() == Some(fingerprint(&listing).as_str()) {
        Ok((server, listing))
    } else {
        bail!("its tools have changed since it was added; add it again to accept them")
    }
}

/// FNV-1a over the listing's canonical JSON. This guards against a listing
/// that quietly changes, not against an adversary who can pick collisions.
fn fingerprint(listing: &[Value]) -> String {
    let mut hash: u64 = 0xcbf2_9ce4_8422_2325;
    for byte in Value::from(listing).to_string().bytes() {
        hash ^= u64::from(byte);
        hash = hash.wrapping_mul(0x0000_0100_0000_01b3);
    }
    format!("{hash:016x}")
}

/// A server in a few lines of shell, for the tests here and in `mcp`.
#[cfg(test)]
pub fn fake_server() -> McpServer {
    const FAKE_SERVER: &str = r#"
while read -r line; do
  id=$(printf '%s' "$line" | sed -n 's/.*"id":\([0-9]*\).*/\1/p')
  case "$line" in
    *'"initialize"'*) echo "{\"jsonrpc\":\"2.0\",\"id\":$id,\"result\":{\"capabilities\":{}}}" ;;
    *'"tools/list"'*) echo "{\"jsonrpc\":\"2.0\",\"method\":\"notifications/noise\"}"
                      echo "{\"jsonrpc\":\"2.0\",\"id\":$id,\"result\":{\"tools\":[{\"name\":\"shout\",\"description\":\"Shouts\",\"inputSchema\":{\"type\":\"object\"}}]}}" ;;
    *'"shout"'*)      echo "{\"jsonrpc\":\"2.0\",\"id\":$id,\"result\":{\"content\":[{\"type\":\"text\",\"text\":\"HELLO\"}],\"isError\":false}}" ;;
    *'"stall"'*)      sleep 5 ;;
    *'"die"'*)        exit 0 ;;
    *'"tools/call"'*) echo "{\"jsonrpc\":\"2.0\",\"id\":$id,\"result\":{\"content\":[{\"type\":\"text\",\"text\":\"no such tool\"}],\"isError\":true}}" ;;
  esac
done
"#;

    McpServer {
        command: "bash".to_string(),
        args: vec!["-c".to_string(), FAKE_SERVER.to_string()],
        env: Default::default(),
        prose: Default::default(),
        fingerprint: None,
        trusted_only: false,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_server_is_started_listed_and_called() {
        let mut server = Server::start(&fake_server()).unwrap();

        let tools = server.tools().unwrap();
        assert_eq!(tools.len(), 1);
        assert_eq!(tools[0]["name"], "shout");

        assert_eq!(server.call("shout", &json!({})).unwrap(), "HELLO");
        assert_eq!(server.call("whisper", &json!({})).unwrap_err().to_string(), "no such tool");
    }

    #[test]
    fn a_server_whose_tools_changed_since_it_was_added_is_left_out() {
        let mut settings = fake_server();
        assert!(listed(&settings).is_err());

        settings.fingerprint = Some(fingerprint_of(&settings).unwrap());
        let (_, listing) = listed(&settings).unwrap();
        assert_eq!(listing[0]["name"], "shout");

        settings.fingerprint = Some("0000000000000000".to_string());
        assert!(listed(&settings).is_err());
    }

    #[test]
    fn a_server_that_hangs_or_dies_is_given_up_on_and_started_again() {
        let mut settings = fake_server();
        settings.fingerprint = Some(fingerprint_of(&settings).unwrap());
        let mut server = Server::start_with_patience(&settings, Duration::from_secs(1)).unwrap();

        let began = Instant::now();
        let stalled = server.call("stall", &json!({})).unwrap_err().to_string();
        assert_eq!(stalled, "the server did not answer tools/call within 1 seconds");
        assert!(began.elapsed() < Duration::from_secs(3));
        assert_eq!(server.call("shout", &json!({})).unwrap(), "HELLO", "the next call gets a new server");

        assert!(server.call("die", &json!({})).unwrap_err().to_string().contains("went away"));
        assert_eq!(server.call("shout", &json!({})).unwrap(), "HELLO");

        server.settings.fingerprint = Some("0000000000000000".to_string());
        server.call("die", &json!({})).unwrap_err();
        let changed = server.call("shout", &json!({})).unwrap_err().to_string();
        assert!(changed.contains("came back with different tools"), "{changed}");
    }
}
