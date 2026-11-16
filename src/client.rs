use crate::protocol::{now_ms, Endpoint, RpcRequest, RpcResponse};
use crate::workspace;
use serde_json::{json, Value};
use std::fs::OpenOptions;
use std::io::{BufRead, BufReader, Write};
use std::os::unix::net::UnixStream;
use std::path::Path;
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

#[derive(Debug, Clone)]
pub struct ClientError {
    pub code: String,
    pub message: String,
}

impl std::fmt::Display for ClientError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}: {}", self.code, self.message)
    }
}

impl std::error::Error for ClientError {}

pub fn read_endpoint(master: &Path) -> Result<Endpoint, ClientError> {
    workspace::read_endpoint(master).ok_or_else(|| ClientError {
        code: "SP_DAEMON_UNAVAILABLE".to_string(),
        message: "daemon endpoint is not available; run `sp daemon`".to_string(),
    })
}

pub fn call(
    master: &Path,
    method: &str,
    agent: Option<&str>,
    params: Value,
) -> Result<Value, ClientError> {
    call_with_timeout(master, method, agent, params, 10_000)
}

pub fn call_with_timeout(
    master: &Path,
    method: &str,
    agent: Option<&str>,
    params: Value,
    timeout_ms: u64,
) -> Result<Value, ClientError> {
    let endpoint = read_endpoint(master)?;
    let stream = UnixStream::connect(&endpoint.socket).map_err(|err| ClientError {
        code: "SP_DAEMON_UNAVAILABLE".to_string(),
        message: format!("cannot connect to daemon at {}: {err}", endpoint.socket),
    })?;
    stream
        .set_read_timeout(Some(Duration::from_millis(timeout_ms)))
        .ok();
    stream
        .set_write_timeout(Some(Duration::from_millis(timeout_ms)))
        .ok();
    let mut writer = stream.try_clone().map_err(|err| ClientError {
        code: "SP_TRANSPORT_ERROR".to_string(),
        message: err.to_string(),
    })?;
    let request = RpcRequest {
        id: now_ms() ^ (std::process::id() as u64) << 16,
        token: endpoint.token,
        method: method.to_string(),
        agent: agent.map(ToString::to_string),
        params,
    };
    let payload = serde_json::to_vec(&request).map_err(|err| ClientError {
        code: "SP_TRANSPORT_ERROR".to_string(),
        message: err.to_string(),
    })?;
    writer.write_all(&payload).map_err(|err| ClientError {
        code: "SP_TRANSPORT_ERROR".to_string(),
        message: err.to_string(),
    })?;
    writer.write_all(b"\n").map_err(|err| ClientError {
        code: "SP_TRANSPORT_ERROR".to_string(),
        message: err.to_string(),
    })?;
    writer.flush().map_err(|err| ClientError {
        code: "SP_TRANSPORT_ERROR".to_string(),
        message: err.to_string(),
    })?;
    let mut reader = BufReader::new(stream);
    let mut line = String::new();
    reader.read_line(&mut line).map_err(|err| ClientError {
        code: "SP_TRANSPORT_TIMEOUT".to_string(),
        message: err.to_string(),
    })?;
    if line.trim().is_empty() {
        return Err(ClientError {
            code: "SP_TRANSPORT_ERROR".to_string(),
            message: "daemon closed the connection without a response".to_string(),
        });
    }
    let response: RpcResponse = serde_json::from_str(&line).map_err(|err| ClientError {
        code: "SP_PROTOCOL_MISMATCH".to_string(),
        message: err.to_string(),
    })?;
    if response.ok {
        Ok(response.data)
    } else {
        Err(ClientError {
            code: response.code.unwrap_or_else(|| "SP_INTERNAL".to_string()),
            message: response
                .message
                .unwrap_or_else(|| "daemon request failed".to_string()),
        })
    }
}

pub fn emit_notices(master: &Path, agent: Option<&str>) {
    let cursor_name = match agent {
        Some(agent) => format!(
            "notices-{}.cursor",
            workspace::sanitize_agent(agent).unwrap_or_else(|_| "agent".to_string())
        ),
        None => "notices.cursor".to_string(),
    };
    let cursor_path = workspace::run_dir(master).join(cursor_name);
    let after = std::fs::read_to_string(&cursor_path)
        .ok()
        .and_then(|value| value.trim().parse::<u64>().ok())
        .unwrap_or(0);
    let value = match call(
        master,
        "notice.list",
        agent,
        json!({"after_seq": after, "limit": 50}),
    ) {
        Ok(value) => value,
        Err(_) => return,
    };
    let notices = value
        .get("notices")
        .and_then(Value::as_array)
        .cloned()
        .unwrap_or_default();
    let mut latest = after;
    let mut printed = 0usize;
    for notice in notices {
        let seq = notice.get("seq").and_then(Value::as_u64).unwrap_or(0);
        latest = latest.max(seq);
        if printed >= 20 {
            continue;
        }
        let severity = notice
            .get("severity")
            .and_then(Value::as_str)
            .unwrap_or("info");
        let kind = notice
            .get("kind")
            .and_then(Value::as_str)
            .unwrap_or("notice");
        let source = notice
            .get("agent")
            .and_then(Value::as_str)
            .unwrap_or("system");
        let message = notice.get("message").and_then(Value::as_str).unwrap_or("");
        eprintln!("SP_NOTICE v1 seq={seq} severity={severity} kind={kind} agent={source} message={message}");
        printed += 1;
    }
    if latest > after {
        let _ = workspace::atomic_write(&cursor_path, latest.to_string().as_bytes());
    }
}

pub fn ping(master: &Path) -> bool {
    call_with_timeout(master, "ping", None, json!({}), 500).is_ok()
}

pub fn ensure_daemon(master: &Path) -> Result<(), ClientError> {
    if ping(master) {
        return Ok(());
    }
    workspace::ensure_layout(master).map_err(|err| ClientError {
        code: "SP_INTERNAL".to_string(),
        message: err.to_string(),
    })?;
    let exe = std::env::current_exe().map_err(|err| ClientError {
        code: "SP_INTERNAL".to_string(),
        message: err.to_string(),
    })?;
    let log = OpenOptions::new()
        .create(true)
        .append(true)
        .open(workspace::log_path(master))
        .map_err(|err| ClientError {
            code: "SP_INTERNAL".to_string(),
            message: err.to_string(),
        })?;
    let log_err = log.try_clone().map_err(|err| ClientError {
        code: "SP_INTERNAL".to_string(),
        message: err.to_string(),
    })?;
    Command::new(exe)
        .arg("daemon")
        .arg("--foreground")
        .current_dir(master)
        .env("SP_MASTER_ROOT", master)
        .stdin(Stdio::null())
        .stdout(Stdio::from(log))
        .stderr(Stdio::from(log_err))
        .spawn()
        .map_err(|err| ClientError {
            code: "SP_DAEMON_UNAVAILABLE".to_string(),
            message: format!("cannot start daemon: {err}"),
        })?;
    let deadline = Instant::now() + Duration::from_secs(8);
    while Instant::now() < deadline {
        if ping(master) {
            return Ok(());
        }
        std::thread::sleep(Duration::from_millis(25));
    }
    Err(ClientError {
        code: "SP_DAEMON_UNAVAILABLE".to_string(),
        message: "daemon did not become ready within 8 seconds".to_string(),
    })
}
