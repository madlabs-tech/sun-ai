//! Helpers shared by the engine integration tests.
#![allow(clippy::unwrap_used)]

use std::io::{BufRead, BufReader, Read, Write};

use sun_engine::DecisionClient;

/// A fake `/v1/systemone` (Laya or Jev) that answers every request with `body`.
pub fn systemone(body: &'static str) -> DecisionClient {
    let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let url = format!("http://{}", listener.local_addr().unwrap());
    std::thread::spawn(move || {
        for stream in listener.incoming() {
            let mut stream = stream.unwrap();
            let mut reader = BufReader::new(stream.try_clone().unwrap());
            let mut len = 0;
            loop {
                let mut line = String::new();
                reader.read_line(&mut line).unwrap();
                if let Some(n) = line.to_ascii_lowercase().strip_prefix("content-length:") {
                    len = n.trim().parse().unwrap();
                }
                if line == "\r\n" {
                    break;
                }
            }
            reader.read_exact(&mut vec![0; len]).unwrap();
            let head = "HTTP/1.1 200 OK\r\ncontent-type: application/json\r\nconnection: close";
            let _ = write!(
                stream,
                "{head}\r\ncontent-length: {}\r\n\r\n{body}",
                body.len()
            );
        }
    });
    DecisionClient::new(&url, None).unwrap()
}

/// A plan without caps (Community), for writes that take one.
pub fn unlimited() -> sun_core::entitlement::Entitlement {
    use sun_core::entitlement::{Entitlement, EntitlementStatus};
    Entitlement::community(EntitlementStatus::Community)
}
