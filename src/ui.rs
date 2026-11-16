use crate::client;
use crate::daemon;
use crate::workspace;
use serde_json::{json, Value};
use std::io::{Read, Write};
use std::net::{TcpListener, TcpStream};
use std::path::{Path, PathBuf};
use std::time::Duration;

const DASHBOARD: &str = include_str!("../ui/dashboard.html");
const ICON: &str = include_str!("../ui/icon.svg");

pub fn serve(master: PathBuf) -> i32 {
    if let Err(err) = client::ensure_daemon(&master) {
        eprintln!("sp ui: {err}");
        return 2;
    }
    let config = workspace::load_config(&master);
    let port = config.ui_port;
    let listener = match TcpListener::bind(("127.0.0.1", port)) {
        Ok(listener) => listener,
        Err(err) => {
            eprintln!("sp ui: cannot bind 127.0.0.1:{port}: {err}");
            return 2;
        }
    };
    println!("ScratchPost Mission Control: http://127.0.0.1:{port}/");
    for stream in listener.incoming() {
        match stream {
            Ok(stream) => {
                let master = master.clone();
                let _ = std::thread::Builder::new()
                    .name("sp-ui".to_string())
                    .stack_size(256 * 1024)
                    .spawn(move || handle_connection(stream, &master));
            }
            Err(err) => {
                eprintln!("sp ui: accept failed: {err}");
            }
        }
    }
    0
}

struct Request {
    method: String,
    path: String,
    body: Vec<u8>,
}

fn handle_connection(mut stream: TcpStream, master: &Path) {
    let _ = stream.set_read_timeout(Some(Duration::from_secs(5)));
    let _ = stream.set_write_timeout(Some(Duration::from_secs(5)));
    let Some(request) = read_request(&mut stream) else {
        return;
    };
    let path = request.path.split('?').next().unwrap_or("/");
    let query = request.path.split_once('?').map(|(_, q)| q).unwrap_or("");
    match (request.method.as_str(), path) {
        ("GET", "/") => {
            let _ = respond(
                &mut stream,
                200,
                "text/html; charset=utf-8",
                DASHBOARD.as_bytes(),
            );
        }
        ("GET", "/icon.svg") => {
            let _ = respond(&mut stream, 200, "image/svg+xml", ICON.as_bytes());
        }
        ("GET", "/favicon.ico") => {
            let _ = respond(&mut stream, 200, "image/svg+xml", ICON.as_bytes());
        }
        ("GET", "/api/status") => {
            let value = current_snapshot(master);
            let body = serde_json::to_vec(&value).unwrap_or_else(|_| b"{}".to_vec());
            let _ = respond(&mut stream, 200, "application/json", &body);
        }
        ("GET", "/api/notices") => {
            let value = client::call(
                master,
                "notice.list",
                None,
                json!({"after_seq": 0, "limit": 200}),
            )
            .unwrap_or_else(|err| json!({"error": err.message}));
            let body = serde_json::to_vec(&value).unwrap_or_else(|_| b"{}".to_vec());
            let _ = respond(&mut stream, 200, "application/json", &body);
        }
        ("GET", "/events") => {
            stream_sse(&mut stream, master);
        }
        ("POST", "/api/mode") => {
            let body: Value = serde_json::from_slice(&request.body).unwrap_or_else(|_| json!({}));
            let mut params = serde_json::Map::new();
            if let Some(mode) = body
                .get("mode")
                .and_then(Value::as_str)
                .or_else(|| query_param(query, "mode"))
            {
                params.insert("mode".to_string(), json!(mode));
            }
            if let Some(scope) = body
                .get("barrier_scope")
                .and_then(Value::as_str)
                .or_else(|| query_param(query, "barrier_scope"))
            {
                params.insert("barrier_scope".to_string(), json!(scope));
            }
            let result = client::call(master, "config.set", None, Value::Object(params));
            let value = match result {
                Ok(value) => json!({"ok": true, "config": value}),
                Err(err) => json!({"ok": false, "code": err.code, "message": err.message}),
            };
            let bytes = serde_json::to_vec(&value).unwrap_or_else(|_| b"{}".to_vec());
            let _ = respond(&mut stream, 200, "application/json", &bytes);
        }
        ("POST", "/api/barrier/release") => {
            let result = client::call(master, "barrier.release", None, json!({}));
            let value = match result {
                Ok(value) => json!({"ok": true, "barrier": value}),
                Err(err) => json!({"ok": false, "code": err.code, "message": err.message}),
            };
            let bytes = serde_json::to_vec(&value).unwrap_or_else(|_| b"{}".to_vec());
            let _ = respond(&mut stream, 200, "application/json", &bytes);
        }
        ("POST", "/api/agent/evict") => {
            let body: Value = serde_json::from_slice(&request.body).unwrap_or_else(|_| json!({}));
            let agent = body
                .get("agent")
                .and_then(Value::as_str)
                .or_else(|| query_param(query, "agent"))
                .unwrap_or("");
            let result = client::call(
                master,
                "session.evict",
                Some(agent),
                json!({"agent": agent}),
            );
            let value = match result {
                Ok(value) => json!({"ok": true, "agent": agent, "result": value}),
                Err(err) => json!({"ok": false, "code": err.code, "message": err.message}),
            };
            let bytes = serde_json::to_vec(&value).unwrap_or_else(|_| b"{}".to_vec());
            let _ = respond(&mut stream, 200, "application/json", &bytes);
        }
        _ => {
            let _ = respond(
                &mut stream,
                404,
                "application/json",
                b"{\"error\":\"not found\"}",
            );
        }
    }
}

fn current_snapshot(master: &Path) -> Value {
    if let Some((snapshot, generation)) = daemon::status_snapshot(master) {
        if let Ok(mut value) = serde_json::to_value(snapshot) {
            if let Some(object) = value.as_object_mut() {
                object.insert("generation".to_string(), json!(generation));
            }
            return value;
        }
    }
    client::call(master, "status", None, json!({})).unwrap_or_else(|err| {
        json!({
            "error": err.code,
            "message": err.message,
            "sessions": [],
            "claims": [],
            "components": [],
            "notices": []
        })
    })
}

fn stream_sse(stream: &mut TcpStream, master: &Path) {
    let headers = "HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nCache-Control: no-cache, no-transform\r\nConnection: keep-alive\r\nX-Accel-Buffering: no\r\n\r\n";
    if stream.write_all(headers.as_bytes()).is_err() {
        return;
    }
    let _ = stream.write_all(b"retry: 2000\n\n");
    loop {
        let value = current_snapshot(master);
        let payload = serde_json::to_string(&value).unwrap_or_else(|_| "{}".to_string());
        if write!(stream, "data: {payload}\n\n").is_err() {
            break;
        }
        if stream.flush().is_err() {
            break;
        }
        std::thread::sleep(Duration::from_secs(1));
    }
}

fn read_request(stream: &mut TcpStream) -> Option<Request> {
    let mut data = Vec::new();
    let mut buf = [0u8; 4096];
    loop {
        match stream.read(&mut buf) {
            Ok(0) => break,
            Ok(n) => {
                data.extend_from_slice(&buf[..n]);
                if data.windows(4).any(|w| w == b"\r\n\r\n") || data.len() > 64 * 1024 {
                    break;
                }
            }
            Err(_) => break,
        }
    }
    let split = data
        .windows(4)
        .position(|w| w == b"\r\n\r\n")
        .unwrap_or(data.len());
    let header = String::from_utf8_lossy(&data[..split]);
    let mut lines = header.lines();
    let first = lines.next().unwrap_or("");
    let mut parts = first.split_whitespace();
    let method = parts.next().unwrap_or("GET").to_string();
    let path = parts.next().unwrap_or("/").to_string();
    let mut content_length = 0usize;
    for line in lines {
        if let Some((name, value)) = line.split_once(':') {
            if name.eq_ignore_ascii_case("content-length") {
                content_length = value.trim().parse().unwrap_or(0);
            }
        }
    }
    let body_start = split + 4;
    while data.len() < body_start + content_length && content_length > 0 {
        match stream.read(&mut buf) {
            Ok(0) => break,
            Ok(n) => data.extend_from_slice(&buf[..n]),
            Err(_) => break,
        }
    }
    let body = if data.len() >= body_start {
        data[body_start..].to_vec()
    } else {
        Vec::new()
    };
    Some(Request { method, path, body })
}

fn respond(
    stream: &mut TcpStream,
    status: u16,
    content_type: &str,
    body: &[u8],
) -> std::io::Result<()> {
    let reason = match status {
        200 => "OK",
        404 => "Not Found",
        _ => "Error",
    };
    let header = format!(
        "HTTP/1.1 {status} {reason}\r\nContent-Type: {content_type}\r\nContent-Length: {}\r\nCache-Control: no-store\r\nConnection: close\r\n\r\n",
        body.len()
    );
    stream.write_all(header.as_bytes())?;
    stream.write_all(body)?;
    stream.flush()?;
    Ok(())
}

fn query_param<'a>(query: &'a str, key: &str) -> Option<&'a str> {
    for pair in query.split('&') {
        if let Some((name, value)) = pair.split_once('=') {
            if name == key {
                return Some(value);
            }
        }
    }
    None
}
