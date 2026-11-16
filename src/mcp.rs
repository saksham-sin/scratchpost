use crate::client;
use serde_json::{json, Value};
use std::io::{self, BufRead, Write};
use std::path::{Path, PathBuf};
use std::process::Command;

const MCP_PROTOCOL_VERSION: &str = "2024-11-05";

pub fn run(master: PathBuf) -> i32 {
    let _ = client::ensure_daemon(&master);
    let stdin = io::stdin();
    let mut stdout = io::stdout();
    for line in stdin.lock().lines() {
        let line = match line {
            Ok(line) => line,
            Err(_) => break,
        };
        if line.trim().is_empty() {
            continue;
        }
        let request: Value = match serde_json::from_str(&line) {
            Ok(value) => value,
            Err(err) => {
                let response = json!({
                    "jsonrpc": "2.0",
                    "id": Value::Null,
                    "error": {"code": -32700, "message": format!("parse error: {err}")}
                });
                let _ = writeln!(stdout, "{}", response);
                let _ = stdout.flush();
                continue;
            }
        };
        let id = request.get("id").cloned().unwrap_or(Value::Null);
        let method = request
            .get("method")
            .and_then(Value::as_str)
            .unwrap_or_default();
        if method.starts_with("notifications/") {
            continue;
        }
        let response = match method {
            "initialize" => json!({
                "jsonrpc": "2.0",
                "id": id,
                "result": {
                    "protocolVersion": MCP_PROTOCOL_VERSION,
                    "capabilities": {"tools": {}},
                    "serverInfo": {
                        "name": "scratchpost",
                        "version": env!("CARGO_PKG_VERSION")
                    },
                    "instructions": "ScratchPost coordinates parallel agent sessions in this workspace. Call sp_status first to see active agents, claims, and open barriers. Before adding a Liquibase changeSet id or a Flyway V<n>__ migration, call sp_sequence_next and use the returned key. Reserve files you are about to edit with sp_claim. git commit is gated: a blocked commit returns isError=true with the full SP_BLOCKED contract in structuredContent, including resolution.steps[].command. Run those commands, then retry the original commit. Never use git commit --no-verify and never edit .scratchpost/."
                }
            }),
            "ping" => json!({"jsonrpc": "2.0", "id": id, "result": {}}),
            "tools/list" => json!({
                "jsonrpc": "2.0",
                "id": id,
                "result": {"tools": tool_definitions()}
            }),
            "tools/call" => handle_tool_call(&master, id, &request),
            _ => json!({
                "jsonrpc": "2.0",
                "id": id,
                "error": {"code": -32601, "message": format!("method not found: {method}")}
            }),
        };
        let _ = writeln!(stdout, "{}", response);
        let _ = stdout.flush();
    }
    0
}

fn tool_definitions() -> Value {
    json!([
        {
            "name": "sp_status",
            "description": "Return the ScratchPost workspace ledger: sessions, claims, intents, notices, barrier state, and conflict components.",
            "inputSchema": {"type": "object", "properties": {}}
        },
        {
            "name": "sp_notices",
            "description": "Return coordination notices after a sequence number.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "after_seq": {"type": "integer", "minimum": 0},
                    "limit": {"type": "integer", "minimum": 1, "maximum": 500}
                }
            }
        },
        {
            "name": "sp_sequence_next",
            "description": "Allocate/query the lowest free sequence key in a namespace. Does not reserve it; use sp_resequence or retry the commit.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "namespace": {"type": "string"},
                    "after": {"type": "integer", "minimum": 0}
                },
                "required": ["namespace"]
            }
        },
        {
            "name": "sp_claim",
            "description": "Reserve a repository path before staging, so peers see contention while work is in progress. Mandatory claims are enforced by the commit gate; advisory claims only feed the conflict graph.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "agent": {"type": "string"},
                    "repo": {"type": "string"},
                    "path": {"type": "string"},
                    "strength": {"type": "string", "enum": ["advisory", "mandatory"]},
                    "ttl_ms": {"type": "integer", "minimum": 1000, "maximum": 3600000},
                    "note": {"type": "string"},
                    "release": {"type": "boolean", "description": "Release matching claims instead of creating one."}
                },
                "required": ["agent", "repo", "path"]
            }
        },
        {
            "name": "sp_barrier_arrive",
            "description": "Signal that your work is complete so peers waiting at a coordination barrier can proceed. Returns the barrier state; retry the commit when released is true.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "agent": {"type": "string"},
                    "scope": {"type": "string", "enum": ["workspace", "component"]},
                    "component": {"type": "string"}
                },
                "required": ["agent"]
            }
        },
        {
            "name": "sp_resequence",
            "description": "Deterministically change a Liquibase changeSet id or Flyway filename in an agent session and stage the result.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "agent": {"type": "string"},
                    "file": {"type": "string"},
                    "from": {"type": "string"},
                    "to": {"type": "string"}
                },
                "required": ["agent", "file", "from", "to"]
            }
        },
        {
            "name": "sp_commit",
            "description": "Create a gated commit in an agent session. Equivalent to running git commit inside the session workspace, but callable without a shell. On a coordination conflict this returns isError=true with the full SP_BLOCKED contract in structuredContent, including resolution.steps[].command.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "agent": {"type": "string"},
                    "repo": {"type": "string", "description": "Repository name relative to the session workspace."},
                    "message": {"type": "string"},
                    "paths": {
                        "type": "array",
                        "items": {"type": "string"},
                        "description": "Paths to stage before committing. Omit to commit the current index."
                    }
                },
                "required": ["agent", "repo", "message"]
            }
        },
        {
            "name": "sp_sync",
            "description": "Synchronize an agent session with the canonical base branch.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "agent": {"type": "string"},
                    "repo": {"type": "string"}
                },
                "required": ["agent"]
            }
        },
        {
            "name": "sp_config_set",
            "description": "Set coordination mode to fcfs, barrier-stacked, or barrier-integration.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "mode": {"type": "string", "enum": ["fcfs", "barrier-stacked", "barrier-integration"]}
                },
                "required": ["mode"]
            }
        },
        {
            "name": "sp_evict",
            "description": "Evict an abandoned or stalled agent session from the ledger.",
            "inputSchema": {
                "type": "object",
                "properties": {"agent": {"type": "string"}},
                "required": ["agent"]
            }
        }
    ])
}

fn handle_tool_call(master: &Path, id: Value, request: &Value) -> Value {
    let params = request.get("params").cloned().unwrap_or(Value::Null);
    let name = params
        .get("name")
        .and_then(Value::as_str)
        .unwrap_or_default();
    let arguments = params
        .get("arguments")
        .cloned()
        .unwrap_or_else(|| json!({}));
    let calls = match name {
        "sp_status" => client::call(master, "status", None, json!({})),
        "sp_notices" => client::call(
            master,
            "notice.list",
            None,
            json!({
                "after_seq": arguments.get("after_seq").and_then(Value::as_u64).unwrap_or(0),
                "limit": arguments.get("limit").and_then(Value::as_u64).unwrap_or(100)
            }),
        ),
        "sp_sequence_next" => {
            let namespace = arguments
                .get("namespace")
                .and_then(Value::as_str)
                .unwrap_or("");
            client::call(
                master,
                "sequence.next",
                None,
                json!({
                    "namespace": namespace,
                    "after": arguments.get("after").and_then(Value::as_u64).unwrap_or(0)
                }),
            )
        }
        "sp_claim" => {
            let agent = arguments.get("agent").and_then(Value::as_str).unwrap_or("");
            let release = arguments
                .get("release")
                .and_then(Value::as_bool)
                .unwrap_or(false);
            let mut params = serde_json::Map::new();
            params.insert("agent".to_string(), json!(agent));
            for key in ["repo", "path", "strength", "note"] {
                if let Some(value) = arguments.get(key).and_then(Value::as_str) {
                    params.insert(key.to_string(), json!(value));
                }
            }
            if let Some(ttl) = arguments.get("ttl_ms").and_then(Value::as_u64) {
                params.insert("ttl_ms".to_string(), json!(ttl));
            }
            client::call(
                master,
                if release {
                    "claim.release"
                } else {
                    "claim.path"
                },
                Some(agent),
                Value::Object(params),
            )
        }
        "sp_barrier_arrive" => {
            let agent = arguments.get("agent").and_then(Value::as_str).unwrap_or("");
            let scope = arguments
                .get("scope")
                .and_then(Value::as_str)
                .unwrap_or("workspace");
            let component = arguments.get("component").and_then(Value::as_str);
            client::call(
                master,
                "barrier.arrive",
                Some(agent),
                json!({"agent": agent, "scope": scope, "component": component}),
            )
        }
        "sp_resequence" => run_cli_tool(
            master,
            &[
                "resequence",
                "--agent",
                arguments.get("agent").and_then(Value::as_str).unwrap_or(""),
                "--file",
                arguments.get("file").and_then(Value::as_str).unwrap_or(""),
                "--from",
                arguments.get("from").and_then(Value::as_str).unwrap_or(""),
                "--to",
                arguments.get("to").and_then(Value::as_str).unwrap_or(""),
            ],
        ),
        "sp_commit" => {
            let agent = arguments.get("agent").and_then(Value::as_str).unwrap_or("");
            let repo = arguments.get("repo").and_then(Value::as_str).unwrap_or("");
            let message = arguments
                .get("message")
                .and_then(Value::as_str)
                .unwrap_or("");
            let paths: Vec<String> = arguments
                .get("paths")
                .and_then(Value::as_array)
                .map(|values| {
                    values
                        .iter()
                        .filter_map(Value::as_str)
                        .map(ToString::to_string)
                        .collect()
                })
                .unwrap_or_default();
            run_session_commit(master, agent, repo, message, &paths)
        }
        "sp_sync" => run_cli_tool(
            master,
            &[
                "sync",
                "--rebase",
                "--agent",
                arguments.get("agent").and_then(Value::as_str).unwrap_or(""),
            ],
        ),
        "sp_config_set" => client::call(
            master,
            "config.set",
            None,
            json!({"mode": arguments.get("mode").and_then(Value::as_str).unwrap_or("")}),
        ),
        "sp_evict" => {
            let agent = arguments.get("agent").and_then(Value::as_str).unwrap_or("");
            client::call(
                master,
                "session.evict",
                Some(agent),
                json!({"agent": agent}),
            )
        }
        _ => {
            return json!({
                "jsonrpc": "2.0",
                "id": id,
                "error": {"code": -32602, "message": format!("unknown tool: {name}")}
            });
        }
    };

    let notices = client::call(
        master,
        "notice.list",
        None,
        json!({"after_seq": 0, "limit": 50}),
    )
    .ok()
    .and_then(|value| value.get("notices").cloned())
    .unwrap_or_else(|| json!([]));

    match calls {
        Ok(data) => {
            let text = serde_json::to_string_pretty(&data).unwrap_or_else(|_| "{}".to_string());
            let mut structured = data.clone();
            if let Some(object) = structured.as_object_mut() {
                object.insert("notices".to_string(), notices);
            }
            json!({
                "jsonrpc": "2.0",
                "id": id,
                "result": {
                    "content": [{"type": "text", "text": text}],
                    "structuredContent": structured,
                    "isError": false
                }
            })
        }
        Err(err) => {
            // A blocked commit is a coordination conflict, not a transport
            // failure. When the CLI emitted an `SP_CONFLICT_JSON` footer, surface
            // that exact contract so one parser handles stderr and MCP alike.
            let contract = extract_conflict_contract(&err.message);
            let mut structured = match contract.clone() {
                Some(report) => report,
                None => json!({
                    "schema": "sp.mcp-error.v1",
                    "code": err.code,
                    "message": err.message,
                }),
            };
            if let Some(object) = structured.as_object_mut() {
                object.insert("notices".to_string(), notices);
                object.insert("transport".to_string(), json!("mcp"));
            }
            let text = serde_json::to_string_pretty(&structured).unwrap_or_default();
            json!({
                "jsonrpc": "2.0",
                "id": id,
                "error": {
                    "code": -32000,
                    "message": err.message,
                    "data": {
                        "code": err.code,
                        "isError": true,
                        "conflict": contract,
                    }
                },
                "result": {
                    "content": [{"type": "text", "text": text}],
                    "structuredContent": structured,
                    "isError": true
                }
            })
        }
    }
}

/// Extracts the `SP_CONFLICT_JSON={...}` footer a blocked CLI command writes to
/// stderr. Returns `None` for ordinary failures.
fn extract_conflict_contract(stderr: &str) -> Option<Value> {
    const PREFIX: &str = "SP_CONFLICT_JSON=";
    let line = stderr
        .lines()
        .rev()
        .find_map(|line| line.trim().strip_prefix(PREFIX))?;
    let value: Value = serde_json::from_str(line).ok()?;
    if value.get("schema").and_then(Value::as_str) == Some("sp.conflict.v1") {
        Some(value)
    } else {
        None
    }
}

/// Runs a gated commit inside an agent session without requiring a shell.
///
/// The session environment is reconstructed from the ledger, so the managed
/// `git` shim applies exactly as it would for an interactive agent.
fn run_session_commit(
    master: &Path,
    agent: &str,
    repo: &str,
    message: &str,
    paths: &[String],
) -> Result<Value, client::ClientError> {
    let session = client::call(
        master,
        "session.ensure",
        Some(agent),
        json!({"agent": agent}),
    )?;
    let workspace = session
        .get("workspace")
        .and_then(Value::as_str)
        .ok_or_else(|| client::ClientError {
            code: "SP_INTERNAL".to_string(),
            message: "session record has no workspace".to_string(),
        })?;
    let repo_dir = Path::new(workspace).join(repo);
    if !repo_dir.is_dir() {
        return Err(client::ClientError {
            code: "SP_REPOSITORY_UNAVAILABLE".to_string(),
            message: format!("no repository {repo} in session workspace"),
        });
    }

    let mut env: Vec<(String, String)> = vec![
        (
            "SP_MASTER_ROOT".to_string(),
            master.to_string_lossy().to_string(),
        ),
        ("SP_AGENT".to_string(), agent.to_string()),
        ("SP_SESSION_ROOT".to_string(), workspace.to_string()),
    ];
    if let Ok(record) = serde_json::from_value::<crate::protocol::SessionRecord>(session.clone()) {
        for (key, value) in crate::session::resource_env(&record, master) {
            env.push((key, value));
        }
    }
    let shim_dir = crate::workspace::bin_dir(master);
    let path = format!(
        "{}:{}",
        shim_dir.to_string_lossy(),
        std::env::var("PATH").unwrap_or_default()
    );
    env.push(("PATH".to_string(), path));

    if !paths.is_empty() {
        let mut add = Command::new("git");
        add.current_dir(&repo_dir).args(["add", "--"]);
        add.args(paths);
        for (key, value) in &env {
            add.env(key, value);
        }
        let output = add.output().map_err(|err| client::ClientError {
            code: "SP_INTERNAL".to_string(),
            message: err.to_string(),
        })?;
        if !output.status.success() {
            return Err(client::ClientError {
                code: "SP_TOOL_FAILED".to_string(),
                message: format!(
                    "git add failed: {}",
                    String::from_utf8_lossy(&output.stderr).trim()
                ),
            });
        }
    }

    let mut commit = Command::new("git");
    commit
        .current_dir(&repo_dir)
        .args(["commit", "-m", message]);
    for (key, value) in &env {
        commit.env(key, value);
    }
    let output = commit.output().map_err(|err| client::ClientError {
        code: "SP_INTERNAL".to_string(),
        message: err.to_string(),
    })?;
    let stdout = String::from_utf8_lossy(&output.stdout).trim().to_string();
    let stderr = String::from_utf8_lossy(&output.stderr).trim().to_string();
    if output.status.success() {
        Ok(json!({
            "ok": true,
            "agent": agent,
            "repo": repo,
            "stdout": stdout,
            "stderr": stderr,
        }))
    } else {
        Err(client::ClientError {
            code: "SP_COMMIT_BLOCKED".to_string(),
            message: if stderr.is_empty() { stdout } else { stderr },
        })
    }
}

fn run_cli_tool(master: &Path, args: &[&str]) -> Result<Value, client::ClientError> {
    let exe = std::env::current_exe().map_err(|err| client::ClientError {
        code: "SP_INTERNAL".to_string(),
        message: err.to_string(),
    })?;
    let output = Command::new(exe)
        .args(args)
        .current_dir(master)
        .env("SP_MASTER_ROOT", master)
        .output()
        .map_err(|err| client::ClientError {
            code: "SP_INTERNAL".to_string(),
            message: err.to_string(),
        })?;
    if output.status.success() {
        Ok(json!({
            "ok": true,
            "command": args.join(" "),
            "stdout": String::from_utf8_lossy(&output.stdout).trim()
        }))
    } else {
        Err(client::ClientError {
            code: "SP_TOOL_FAILED".to_string(),
            message: format!(
                "{}: {}",
                args.join(" "),
                String::from_utf8_lossy(&output.stderr).trim()
            ),
        })
    }
}
