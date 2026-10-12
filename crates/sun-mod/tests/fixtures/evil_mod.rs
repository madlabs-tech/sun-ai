//! Malicious test mod for `tests/sandbox.rs`: every action tries to reach
//! something and reports whether it worked. Under the sandbox, everything
//! undeclared must fail.
//!
//! Actions (all return `{ "ok": bool, "detail": string }`):
//! - `read_file {path}`, `write_file {path}`
//! - `env {name}`: is the env var set?
//! - `connect {addr}`: a direct TCP connection
//! - `tunnel {target}`: `CONNECT target` through `$HTTPS_PROXY`, then a ping
//! - `exec {program, args}`: runs a program, `detail` is its stdout

use std::io::{BufRead, BufReader, Read, Write};
use std::net::TcpStream;
use std::time::Duration;

use serde_json::{Value, json};
use sun_mod_sdk::{CallParams, Mod, RpcError, code};

struct Evil;

fn arg(params: &Value, key: &str) -> String {
    params[key].as_str().unwrap_or_default().to_owned()
}

fn outcome(result: Result<String, String>) -> Value {
    match result {
        Ok(detail) => json!({ "ok": true, "detail": detail }),
        Err(detail) => json!({ "ok": false, "detail": detail }),
    }
}

fn tunnel(target: &str) -> Result<String, String> {
    let proxy = std::env::var("HTTPS_PROXY").map_err(|e| format!("no proxy: {e}"))?;
    let addr = proxy.trim_start_matches("http://");
    let mut stream = TcpStream::connect(addr).map_err(|e| e.to_string())?;
    stream
        .set_read_timeout(Some(Duration::from_secs(5)))
        .map_err(|e| e.to_string())?;
    write!(
        stream,
        "CONNECT {target} HTTP/1.1\r\nHost: {target}\r\n\r\n"
    )
    .map_err(|e| e.to_string())?;
    let mut reader = BufReader::new(stream.try_clone().map_err(|e| e.to_string())?);
    let mut status = String::new();
    reader.read_line(&mut status).map_err(|e| e.to_string())?;
    if !status.contains(" 200 ") {
        return Err(status.trim().to_owned());
    }
    let mut line = String::new();
    while line != "\r\n" {
        line.clear();
        reader.read_line(&mut line).map_err(|e| e.to_string())?;
    }
    stream.write_all(b"ping\n").map_err(|e| e.to_string())?;
    let mut answer = String::new();
    reader.read_line(&mut answer).map_err(|e| e.to_string())?;
    Ok(answer.trim().to_owned())
}

impl Mod for Evil {
    fn call(&mut self, call: CallParams) -> Result<Value, RpcError> {
        let p = &call.params;
        let result = match call.action.as_str() {
            "read_file" => std::fs::read_to_string(arg(p, "path")).map_err(|e| e.to_string()),
            "write_file" => std::fs::write(arg(p, "path"), "pwned")
                .map(|()| String::new())
                .map_err(|e| e.to_string()),
            "env" => std::env::var(arg(p, "name")).map_err(|e| e.to_string()),
            "connect" => {
                let addr = arg(p, "addr").parse().map_err(|e| format!("{e}"));
                addr.and_then(|addr| {
                    TcpStream::connect_timeout(&addr, Duration::from_secs(3))
                        .map(|_| String::new())
                        .map_err(|e| e.to_string())
                })
            }
            "tunnel" => tunnel(&arg(p, "target")),
            "exec" => {
                let args: Vec<String> =
                    serde_json::from_value(p["args"].clone()).unwrap_or_default();
                std::process::Command::new(arg(p, "program"))
                    .args(args)
                    .output()
                    .map_err(|e| e.to_string())
                    .and_then(|out| {
                        let mut stdout = String::new();
                        out.stdout.as_slice().read_to_string(&mut stdout).ok();
                        match out.status.success() {
                            true => Ok(stdout),
                            false => Err(format!("{}: {stdout}", out.status)),
                        }
                    })
            }
            other => return Err(RpcError::new(code::METHOD_NOT_FOUND, other)),
        };
        Ok(outcome(result))
    }
}

fn main() -> std::io::Result<()> {
    sun_mod_sdk::run(Evil)
}
