//! Shared helpers for tests.

use std::io::{BufRead, BufReader, Write};
use std::net::TcpListener;

/// A tiny HTTP server for tests: serves `routes` (path -> body), supports GET
/// and HEAD, answers `/redir` with a redirect to `/file`, and returns 404 for
/// anything else. Returns the port; the server thread lives for the test run.
pub fn serve(routes: Vec<(String, Vec<u8>)>) -> u16 {
    let listener = TcpListener::bind(("127.0.0.1", 0)).unwrap();
    let port = listener.local_addr().unwrap().port();
    std::thread::spawn(move || {
        for stream in listener.incoming().flatten() {
            let mut reader = BufReader::new(stream);
            let mut request_line = String::new();
            if reader.read_line(&mut request_line).is_err() {
                continue;
            }
            loop {
                let mut line = String::new();
                if reader.read_line(&mut line).unwrap_or(0) == 0 || line.trim().is_empty() {
                    break;
                }
            }
            let mut parts = request_line.split_whitespace();
            let (method, path) = (parts.next().unwrap_or(""), parts.next().unwrap_or(""));
            let out = reader.get_mut();
            if path == "/redir" {
                let _ = write!(
                    out,
                    "HTTP/1.1 302 Found\r\nLocation: /file\r\nContent-Length: 0\r\nConnection: close\r\n\r\n"
                );
            } else if let Some((_, body)) = routes.iter().find(|(p, _)| p == path) {
                let _ = write!(
                    out,
                    "HTTP/1.1 200 OK\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
                    body.len()
                );
                if method != "HEAD" {
                    let _ = out.write_all(body);
                }
            } else {
                let _ = write!(
                    out,
                    "HTTP/1.1 404 Not Found\r\nContent-Length: 0\r\nConnection: close\r\n\r\n"
                );
            }
        }
    });
    port
}
