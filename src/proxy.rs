//! The only way out of a hand's sandbox: an HTTP CONNECT proxy on a unix
//! socket that tunnels to an allowlist of hosts and refuses everything else.
//! The sandbox has no network of its own, so what isn't allowed here doesn't
//! exist for the hand. Refusals are logged — they're the record of what a
//! hand tried to reach.

use anyhow::{Context, Result};
use serde_json::json;
use std::fs;
use std::io::{self, BufRead, BufReader, Write};
use std::net::{Shutdown, TcpStream};
use std::os::unix::net::{UnixListener, UnixStream};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::thread;

use crate::logs;

pub const CLAUDE_API: &str = "api.anthropic.com:443";

pub struct Proxy {
    socket: PathBuf,
}

impl Proxy {
    pub fn start(socket: &Path, allowed: &[&str]) -> Result<Proxy> {
        if let Some(directory) = socket.parent() {
            fs::create_dir_all(directory)?;
        }
        let _ = fs::remove_file(socket);
        let listener = UnixListener::bind(socket)
            .with_context(|| format!("could not listen on {}", socket.display()))?;
        let allowed: Arc<Vec<String>> = Arc::new(allowed.iter().map(|it| it.to_string()).collect());

        thread::spawn(move || {
            for client in listener.incoming().flatten() {
                let allowed = Arc::clone(&allowed);
                thread::spawn(move || {
                    let _ = serve(client, &allowed);
                });
            }
        });

        Ok(Proxy {
            socket: socket.to_path_buf(),
        })
    }

    pub fn socket(&self) -> &Path {
        &self.socket
    }
}

impl Drop for Proxy {
    fn drop(&mut self) {
        let _ = fs::remove_file(&self.socket);
    }
}

fn serve(client: UnixStream, allowed: &[String]) -> Result<()> {
    let mut reader = BufReader::new(client.try_clone()?);
    let target = connect_target(&mut reader)?;

    if allowed.contains(&target) {
        tunnel(client, reader, &target)
    } else {
        logs::event("proxy.refused", json!({ "target": target }));
        let mut client = client;
        client.write_all(b"HTTP/1.1 403 Forbidden\r\n\r\n")?;
        Ok(())
    }
}

fn connect_target(reader: &mut BufReader<UnixStream>) -> Result<String> {
    let mut request = String::new();
    reader.read_line(&mut request)?;

    let mut header = String::new();
    loop {
        header.clear();
        if reader.read_line(&mut header)? == 0 || header == "\r\n" {
            break;
        }
    }

    let mut parts = request.split_whitespace();
    match (parts.next(), parts.next()) {
        (Some("CONNECT"), Some(target)) => Ok(target.to_string()),
        _ => Ok(format!("not a CONNECT: {}", request.trim())),
    }
}

fn tunnel(mut client: UnixStream, mut from_client: BufReader<UnixStream>, target: &str) -> Result<()> {
    let upstream = TcpStream::connect(target).with_context(|| format!("could not reach {target}"))?;
    client.write_all(b"HTTP/1.1 200 Connection established\r\n\r\n")?;

    let mut to_upstream = upstream.try_clone()?;
    let outbound = thread::spawn(move || {
        let _ = io::copy(&mut from_client, &mut to_upstream);
        let _ = to_upstream.shutdown(Shutdown::Write);
    });

    let mut from_upstream = upstream;
    let _ = io::copy(&mut from_upstream, &mut client);
    let _ = client.shutdown(Shutdown::Write);
    let _ = outbound.join();
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Read;
    use std::net::TcpListener;

    fn socket_path(name: &str) -> PathBuf {
        std::env::temp_dir().join(format!("anna-proxy-{name}-{}.sock", std::process::id()))
    }

    #[test]
    fn tunnels_to_allowed_hosts_and_refuses_the_rest() {
        let upstream = TcpListener::bind("127.0.0.1:0").unwrap();
        let target = upstream.local_addr().unwrap().to_string();
        thread::spawn(move || {
            let (mut connection, _) = upstream.accept().unwrap();
            let mut greeting = [0; 5];
            connection.read_exact(&mut greeting).unwrap();
            connection.write_all(b"hello back").unwrap();
        });

        let proxy = Proxy::start(&socket_path("tunnel"), &[&target]).unwrap();

        let mut allowed = UnixStream::connect(proxy.socket()).unwrap();
        write!(allowed, "CONNECT {target} HTTP/1.1\r\nHost: {target}\r\n\r\n").unwrap();
        let mut reader = BufReader::new(allowed.try_clone().unwrap());
        let mut status = String::new();
        reader.read_line(&mut status).unwrap();
        assert_eq!(status, "HTTP/1.1 200 Connection established\r\n");
        reader.read_line(&mut status).unwrap();
        allowed.write_all(b"hello").unwrap();
        let mut reply = [0; 10];
        reader.read_exact(&mut reply).unwrap();
        assert_eq!(&reply, b"hello back");

        let mut refused = UnixStream::connect(proxy.socket()).unwrap();
        write!(refused, "CONNECT example.com:443 HTTP/1.1\r\n\r\n").unwrap();
        let mut response = String::new();
        refused.read_to_string(&mut response).unwrap();
        assert_eq!(response, "HTTP/1.1 403 Forbidden\r\n\r\n");

        let mut plain = UnixStream::connect(proxy.socket()).unwrap();
        write!(plain, "GET http://example.com/ HTTP/1.1\r\n\r\n").unwrap();
        let mut response = String::new();
        plain.read_to_string(&mut response).unwrap();
        assert_eq!(response, "HTTP/1.1 403 Forbidden\r\n\r\n");
    }
}
