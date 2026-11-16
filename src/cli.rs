use crate::client;
use crate::daemon;
use crate::git_engine;
use crate::mcp;
use crate::protocol::{Mode, SessionRecord};
use crate::session;
use crate::shim;
use crate::style::{self, Style};
use crate::ui;
use crate::workspace;
use serde_json::{json, Value};
use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::Duration;

pub fn master_root() -> PathBuf {
    if let Ok(value) = std::env::var("SP_MASTER_ROOT") {
        return workspace::canonical(&PathBuf::from(value));
    }
    let cwd = std::env::current_dir().unwrap_or_else(|_| PathBuf::from("."));
    workspace::find_master_root(&cwd)
}

fn print_session(session: &SessionRecord) {
    let style = Style::stdout();
    println!("{}", style.header("session"));
    println!("  {}", style.dim(&format!("agent {}", session.agent)));
    let rows = vec![
        ("session".to_string(), session.session_id.clone()),
        ("slot".to_string(), session.slot.to_string()),
        ("port_base".to_string(), session.port_base.to_string()),
        ("workspace".to_string(), session.workspace.clone()),
    ];
    for line in style::align(&rows) {
        println!("  {line}");
    }
    for repo in &session.repos {
        println!(
            "  {} {} {} {} {}",
            style.dim("repo"),
            style.bold(&repo.name),
            style.kv("path", &repo.relative_path),
            style.kv("branch", &repo.branch),
            style.kv("base", &repo.base_oid[..repo.base_oid.len().min(12)])
        );
    }
}

fn ensure_session(master: &Path, agent: &str) -> Result<SessionRecord, String> {
    client::ensure_daemon(master).map_err(|err| err.to_string())?;
    let value = client::call(
        master,
        "session.ensure",
        Some(agent),
        json!({"agent": agent}),
    )
    .map_err(|err| err.to_string())?;
    serde_json::from_value(value).map_err(|err| err.to_string())
}

fn heartbeat_loop(master: PathBuf, agent: String, stop: Arc<AtomicBool>) {
    std::thread::Builder::new()
        .name("sp-heartbeat".to_string())
        .stack_size(128 * 1024)
        .spawn(move || {
            while !stop.load(Ordering::Acquire) {
                std::thread::sleep(Duration::from_secs(10));
                if stop.load(Ordering::Acquire) {
                    break;
                }
                let _ = client::call(
                    &master,
                    "session.heartbeat",
                    Some(&agent),
                    json!({"agent": agent}),
                );
            }
        })
        .ok();
}

fn child_command(
    master: &Path,
    session: &SessionRecord,
    program: &str,
    args: &[String],
) -> Command {
    let mut cmd = Command::new(program);
    cmd.args(args);
    cmd.current_dir(&session.workspace);
    for (key, value) in session::resource_env(session, master) {
        cmd.env(key, value);
    }
    cmd.env("PATH", shim::shell_path(master));
    cmd
}

pub fn run(args: &[String]) -> i32 {
    let master = master_root();
    if args.is_empty() {
        print_usage();
        return 0;
    }
    let command = args[0].as_str();
    if !matches!(command, "notices" | "ui" | "--mcp" | "mcp") {
        let agent = std::env::var("SP_AGENT").ok();
        client::emit_notices(&master, agent.as_deref());
    }
    match command {
        "init" => init(&master),
        "daemon" | "scratchpostd" => match daemon::run_daemon() {
            Ok(()) => 0,
            Err(err) => {
                eprintln!("scratchpostd: {err}");
                2
            }
        },
        "shell" => shell_command(&master, args),
        "run" => run_command(&master, args),
        "git" => shim::run_git_shim(&args[1..]),
        "hook" => {
            if args.len() < 2 {
                eprintln!("usage: sp hook <pre-commit|pre-push>");
                2
            } else {
                shim::run_hook(&args[1], &args[2..])
            }
        }
        "status" => status_command(&master, args),
        "claims" => claims_command(&master, args),
        "claim" => claim_command(&master, args),
        "instructions" => instructions_command(&master, args),
        "config" => config_command(&master, args),
        "resequence" => resequence_command(&master, args),
        "sequence" => sequence_command(&master, args),
        "sync" => sync_command(&master, args),
        "barrier" => barrier_command(&master, args),
        "evict" => evict_command(&master, args),
        "notices" => notices_command(&master, args),
        "ui" => ui::serve(master),
        "--mcp" | "mcp" => mcp::run(master),
        "version" | "--version" => {
            let style = Style::stdout();
            println!(
                "{} {}",
                style.header(""),
                style.kv("version", env!("CARGO_PKG_VERSION"))
            );
            println!(
                "  {}",
                style.kv("protocol", &crate::protocol::PROTOCOL_VERSION.to_string())
            );
            0
        }
        "help" | "--help" | "-h" => {
            print_usage();
            0
        }
        unknown => {
            eprintln!("sp: unknown command {unknown}");
            print_usage();
            2
        }
    }
}

fn print_usage() {
    let style = Style::stderr();
    let groups: [(&str, &[&str]); 5] = [
        (
            "workspace",
            &[
                "sp init",
                "sp daemon",
                "sp status [--json]",
                "sp ui",
            ],
        ),
        (
            "sessions",
            &[
                "sp shell --agent <name>",
                "sp run --agent <name> -- <command...>",
                "sp evict --agent <name>",
            ],
        ),
        (
            "coordination",
            &[
                "sp claims [--json]",
                "sp claim --repo <name> --path <path> [--strength advisory|mandatory] [--ttl-ms <n>] [--note <text>]",
                "sp claim --release [--repo <name>] [--path <path>]",
                "sp barrier <status|arrive|release> [--agent <name>] [--scope workspace|component] [--component <key>]",
                "sp notices [--json]",
            ],
        ),
        (
            "sequences",
            &[
                "sp sequence next --namespace <name> --after <n>",
                "sp resequence --agent <name> --file <relative> --from <key> --to <key>",
                "sp sync --rebase [--repo <name>]",
            ],
        ),
        (
            "configuration",
            &[
                "sp config [--mode <fcfs|barrier-stacked|barrier-integration>] [--barrier-scope <workspace|component>] [--barrier-timeout-ms <n>]",
                "sp instructions [--print]",
                "sp --mcp",
            ],
        ),
    ];
    eprintln!(
        "{} {}",
        style.header(""),
        style.dim("parallel coding-agent workspace coordinator")
    );
    for (group, commands) in groups {
        eprintln!("\n{}", style.section(group));
        for command in commands {
            eprintln!("  {command}");
        }
    }
    eprintln!(
        "\n{}",
        style.dim("run `sp instructions --print` for the agent contract")
    );
}

fn init(master: &Path) -> i32 {
    match workspace::ensure_layout(master) {
        Ok(()) => {}
        Err(err) => {
            eprintln!("sp init: cannot create state layout: {err}");
            return 2;
        }
    }
    let config = workspace::load_config(master);
    if let Err(err) = workspace::save_config(master, &config) {
        eprintln!("sp init: cannot write config: {err}");
        return 2;
    }
    let exe = std::env::current_exe().unwrap_or_else(|_| PathBuf::from("sp"));
    if let Err(err) = workspace::install_bin_links(master, &exe) {
        eprintln!("sp init: cannot install shim links: {err}");
        return 2;
    }
    if let Err(err) = workspace::install_hooks(master, &exe) {
        eprintln!("sp init: cannot install managed hooks: {err}");
        return 2;
    }
    let repos = git_engine::discover_repos(master);
    for (name, repo) in &repos {
        if let Err(err) = git_engine::install_repo_config(master, repo) {
            eprintln!("sp init: cannot configure repository {name}: {err}");
            return 2;
        }
    }
    if repos.is_empty() {
        eprintln!(
            "sp init: warning: no Git repositories discovered under {}",
            master.display()
        );
    }
    let style = Style::stdout();
    println!("{}", style.header("init"));
    match crate::agents::write_scaffolds(master, &config) {
        Ok(report) => {
            println!("\n{}", style.section("agent scaffolds"));
            for path in &report.written {
                println!(
                    "  {} {}",
                    style.good("wrote"),
                    relative_display(master, path)
                );
            }
            for path in &report.skipped {
                println!(
                    "  {} {} {}",
                    style.dim("kept "),
                    relative_display(master, path),
                    style.dim("(not generated by ScratchPost)")
                );
            }
        }
        Err(err) => {
            eprintln!("sp init: cannot write agent scaffolds: {err}");
            return 2;
        }
    }
    println!("\n{}", style.section("daemon"));
    match crate::client::ensure_daemon(master) {
        Ok(()) => {
            let endpoint = workspace::endpoint_path(master);
            let token = workspace::token_path(master);
            println!(
                "  {} {}",
                style.good("ready"),
                relative_display(master, &endpoint)
            );
            println!(
                "  {} {}",
                style.dim("token"),
                relative_display(master, &token)
            );
        }
        Err(err) => {
            eprintln!(
                "  {} daemon not started: {}",
                style.warn("warning"),
                err.message
            );
        }
    }
    println!(
        "\n{} {}",
        style.good("initialized"),
        workspace::state_dir(master).display()
    );
    println!("discovered {} repository(ies)", repos.len());
    println!("\n{}", style.section("next"));
    println!(
        "  1. attach {} to your agent's chat",
        style.accent("scratchpost-agent-prompt.md")
    );
    println!("  2. ask the agent to pick this workspace up and follow the contract");
    println!(
        "     it attaches itself with {}",
        style.accent("sp run --agent <name> -- bash")
    );
    println!(
        "  3. watch what the daemon does in {}",
        style.accent(crate::audit::REPORT_FILE)
    );
    0
}

fn relative_display(master: &Path, path: &Path) -> String {
    path.strip_prefix(master)
        .unwrap_or(path)
        .to_string_lossy()
        .to_string()
}

fn parse_flag(args: &[String], name: &str) -> Option<String> {
    let mut idx = 0;
    while idx < args.len() {
        if args[idx] == name {
            return args.get(idx + 1).cloned();
        }
        if let Some(value) = args[idx].strip_prefix(&format!("{name}=")) {
            return Some(value.to_string());
        }
        idx += 1;
    }
    None
}

fn shell_command(master: &Path, args: &[String]) -> i32 {
    let Some(agent) = parse_flag(args, "--agent").or_else(|| std::env::var("AGENT_ID").ok()) else {
        eprintln!("usage: sp shell --agent <name>   (or set AGENT_ID)");
        return 2;
    };
    let session = match ensure_session(master, &agent) {
        Ok(session) => session,
        Err(err) => {
            eprintln!("sp shell: {err}");
            return 2;
        }
    };
    print_session(&session);
    let shell = std::env::var("SHELL").unwrap_or_else(|_| "/bin/sh".to_string());
    let stop = Arc::new(AtomicBool::new(false));
    heartbeat_loop(master.to_path_buf(), agent.clone(), Arc::clone(&stop));
    let mut cmd = child_command(master, &session, &shell, &[]);
    let status = cmd.status();
    stop.store(true, Ordering::Release);
    status
        .map(|status| status.code().unwrap_or(2))
        .unwrap_or_else(|err| {
            eprintln!("sp shell: cannot start {shell}: {err}");
            2
        })
}

fn run_command(master: &Path, args: &[String]) -> i32 {
    let Some(agent) = parse_flag(args, "--agent").or_else(|| std::env::var("AGENT_ID").ok()) else {
        eprintln!("usage: sp run --agent <name> -- <command...>   (or set AGENT_ID)");
        return 2;
    };
    let separator = args.iter().position(|arg| arg == "--");
    let Some(separator) = separator else {
        eprintln!("usage: sp run --agent <name> -- <command...>");
        return 2;
    };
    if separator + 1 >= args.len() {
        eprintln!("sp run: missing command after --");
        return 2;
    }
    let program = args[separator + 1].clone();
    let child_args = args[separator + 2..].to_vec();
    let session = match ensure_session(master, &agent) {
        Ok(session) => session,
        Err(err) => {
            eprintln!("sp run: {err}");
            return 2;
        }
    };
    print_session(&session);
    let stop = Arc::new(AtomicBool::new(false));
    heartbeat_loop(master.to_path_buf(), agent.clone(), Arc::clone(&stop));
    let mut cmd = child_command(master, &session, &program, &child_args);
    let status = cmd.status();
    stop.store(true, Ordering::Release);
    let _ = client::call(
        master,
        "session.heartbeat",
        Some(&agent),
        json!({"agent": agent}),
    );
    status
        .map(|status| status.code().unwrap_or(2))
        .unwrap_or_else(|err| {
            eprintln!("sp run: cannot execute {program}: {err}");
            2
        })
}

fn current_snapshot(master: &Path) -> Option<Value> {
    if let Some((snapshot, generation)) = daemon::status_snapshot(master) {
        if let Ok(mut value) = serde_json::to_value(snapshot) {
            if let Some(object) = value.as_object_mut() {
                object.insert("generation".to_string(), json!(generation));
            }
            return Some(value);
        }
    }
    client::ensure_daemon(master).ok();
    client::call(master, "status", None, json!({})).ok()
}

fn status_command(master: &Path, args: &[String]) -> i32 {
    let Some(value) = current_snapshot(master) else {
        eprintln!("sp status: daemon unavailable");
        return 2;
    };
    if args.iter().any(|arg| arg == "--json") {
        println!("{}", serde_json::to_string_pretty(&value).unwrap());
        return 0;
    }
    let sessions = value
        .get("sessions")
        .and_then(|v| v.as_array())
        .cloned()
        .unwrap_or_default();
    let claims = value
        .get("claims")
        .and_then(|v| v.as_array())
        .cloned()
        .unwrap_or_default();
    let mode = value
        .get("config")
        .and_then(|v| v.get("mode"))
        .and_then(Value::as_str)
        .unwrap_or("unknown");
    let style = Style::stdout();
    println!("{}", style.header("status"));
    println!(
        "  {} {} {}",
        style.dim("mode"),
        style.accent(mode),
        style.dim(&format!(
            "· {} session(s) · {} claim(s)",
            sessions.len(),
            claims.len()
        ))
    );
    if sessions.is_empty() {
        println!("  {}", style.dim("no active sessions"));
    }
    for session in sessions {
        let agent = session.get("agent").and_then(Value::as_str).unwrap_or("-");
        let slot = session.get("slot").and_then(Value::as_u64).unwrap_or(0);
        let port = session
            .get("port_base")
            .and_then(Value::as_u64)
            .unwrap_or(0);
        let branch = session
            .get("repos")
            .and_then(Value::as_array)
            .and_then(|v| v.first())
            .and_then(|v| v.get("branch"))
            .and_then(Value::as_str)
            .unwrap_or("-");
        let workspace = session
            .get("workspace")
            .and_then(Value::as_str)
            .unwrap_or("-");
        println!(
            "  {} {}",
            style.bold(agent),
            style.dim(&format!("slot {slot} · port {port} · {branch}"))
        );
        println!("    {}", style.dim(workspace));
    }
    0
}

fn claims_command(master: &Path, args: &[String]) -> i32 {
    client::ensure_daemon(master).ok();
    let value = match client::call(master, "claim.list", None, json!({})) {
        Ok(value) => value,
        Err(err) => {
            eprintln!("sp claims: {err}");
            return 2;
        }
    };
    if args.iter().any(|arg| arg == "--json") {
        println!("{}", serde_json::to_string_pretty(&value).unwrap());
        return 0;
    }
    let style = Style::stdout();
    let claims = value.as_array().cloned().unwrap_or_default();
    println!("{}", style.header("claims"));
    println!(
        "  {}",
        style.dim(&format!("{} sequence claim(s)", claims.len()))
    );
    if claims.is_empty() {
        println!("  {}", style.dim("no sequence claims recorded"));
        return 0;
    }
    for claim in claims {
        let id = claim.get("claim_id").and_then(Value::as_u64).unwrap_or(0);
        let rule = claim.get("rule").and_then(Value::as_str).unwrap_or("-");
        let namespace = claim
            .get("namespace")
            .and_then(Value::as_str)
            .unwrap_or("-");
        let key = claim.get("key").and_then(Value::as_str).unwrap_or("-");
        let state = claim.get("state").and_then(Value::as_str).unwrap_or("-");
        let agent = claim.get("agent").and_then(Value::as_str).unwrap_or("-");
        println!(
            "  {} {} {}",
            style.dim(&format!("#{id}")),
            style.bold(&format!("{namespace} {key}")),
            style.state(state)
        );
        println!("    {}", style.dim(&format!("{rule} · held by {agent}")));
    }
    0
}

fn claim_command(master: &Path, args: &[String]) -> i32 {
    let agent = parse_flag(args, "--agent")
        .or_else(|| std::env::var("SP_AGENT").ok())
        .or_else(|| std::env::var("AGENT_ID").ok());
    let Some(agent) = agent else {
        eprintln!("sp claim: --agent <name> is required (or set SP_AGENT)");
        return 2;
    };
    if let Err(err) = client::ensure_daemon(master) {
        eprintln!("sp claim: {err}");
        return 2;
    }
    let release = args.iter().any(|arg| arg == "--release");
    let mut params = serde_json::Map::new();
    params.insert("agent".to_string(), json!(agent));
    for (flag, key) in [
        ("--repo", "repo"),
        ("--path", "path"),
        ("--strength", "strength"),
        ("--note", "note"),
    ] {
        if let Some(value) = parse_flag(args, flag) {
            params.insert(key.to_string(), json!(value));
        }
    }
    if let Some(ttl) = parse_flag(args, "--ttl-ms") {
        match ttl.parse::<u64>() {
            Ok(parsed) => {
                params.insert("ttl_ms".to_string(), json!(parsed));
            }
            Err(err) => {
                eprintln!("sp claim: invalid --ttl-ms: {err}");
                return 2;
            }
        }
    }
    let method = if release {
        "claim.release"
    } else {
        "claim.path"
    };
    match client::call(master, method, Some(&agent), Value::Object(params)) {
        Ok(value) => {
            println!("{}", serde_json::to_string_pretty(&value).unwrap());
            0
        }
        Err(err) => {
            eprintln!("sp claim: {err}");
            2
        }
    }
}

fn instructions_command(master: &Path, args: &[String]) -> i32 {
    let config = workspace::load_config(master);
    if args.iter().any(|arg| arg == "--print") {
        print!("{}", crate::agents::render_instructions(master, &config));
        return 0;
    }
    match crate::agents::write_scaffolds(master, &config) {
        Ok(report) => {
            for path in &report.written {
                println!("wrote {}", relative_display(master, path));
            }
            for path in &report.skipped {
                println!("kept  {}", relative_display(master, path));
            }
            0
        }
        Err(err) => {
            eprintln!("sp instructions: {err}");
            2
        }
    }
}

fn config_command(master: &Path, args: &[String]) -> i32 {
    let mode = parse_flag(args, "--mode");
    let scope = parse_flag(args, "--barrier-scope");
    let timeout = parse_flag(args, "--barrier-timeout-ms");
    if mode.is_none() && scope.is_none() && timeout.is_none() {
        let config = workspace::load_config(master);
        println!("{}", serde_json::to_string_pretty(&config).unwrap());
        return 0;
    }
    let mut params = serde_json::Map::new();
    if let Some(mode) = &mode {
        match mode.parse::<Mode>() {
            Ok(parsed) => {
                params.insert("mode".to_string(), json!(parsed.to_string()));
            }
            Err(err) => {
                eprintln!("sp config: {err}");
                return 2;
            }
        }
    }
    if let Some(scope) = &scope {
        match crate::protocol::BarrierScope::parse(scope) {
            Ok(parsed) => {
                params.insert(
                    "barrier_scope".to_string(),
                    json!(match parsed {
                        crate::protocol::BarrierScope::Workspace => "workspace",
                        crate::protocol::BarrierScope::Component => "component",
                    }),
                );
            }
            Err(err) => {
                eprintln!("sp config: {err}");
                return 2;
            }
        }
    }
    if let Some(timeout) = &timeout {
        match timeout.parse::<u64>() {
            Ok(parsed) => {
                params.insert("barrier_timeout_ms".to_string(), json!(parsed));
            }
            Err(err) => {
                eprintln!("sp config: invalid --barrier-timeout-ms: {err}");
                return 2;
            }
        }
    }
    let params = Value::Object(params);
    if client::ensure_daemon(master).is_ok() {
        match client::call(master, "config.set", None, params.clone()) {
            Ok(value) => {
                println!("{}", serde_json::to_string_pretty(&value).unwrap());
                return 0;
            }
            Err(err) => {
                eprintln!("sp config: {err}");
                return 2;
            }
        }
    }
    let mut config = workspace::load_config(master);
    if let Some(mode) = mode {
        config.mode = mode.parse().unwrap_or(config.mode);
    }
    if let Some(scope) = scope {
        config.barrier_scope =
            crate::protocol::BarrierScope::parse(&scope).unwrap_or(config.barrier_scope);
    }
    if let Some(timeout) = timeout {
        config.barrier_timeout_ms = timeout.parse().unwrap_or(config.barrier_timeout_ms);
    }
    match workspace::save_config(master, &config) {
        Ok(()) => {
            println!("{}", serde_json::to_string_pretty(&config).unwrap());
            0
        }
        Err(err) => {
            eprintln!("sp config: {err}");
            2
        }
    }
}

fn session_repo_for_file(session: &SessionRecord, file: &Path) -> Option<(String, String)> {
    let file = workspace::canonical(file);
    let mut best: Option<(String, String, usize)> = None;
    for repo in &session.repos {
        let root = workspace::canonical(Path::new(&repo.worktree));
        if let Ok(relative) = file.strip_prefix(&root) {
            let prefix_len = root.to_string_lossy().len();
            if best
                .as_ref()
                .map(|(_, _, len)| prefix_len > *len)
                .unwrap_or(true)
            {
                best = Some((
                    repo.worktree.clone(),
                    relative.to_string_lossy().to_string(),
                    prefix_len,
                ));
            }
        }
    }
    best.map(|(root, rel, _)| (root, rel))
}

fn resequence_file(root: &Path, relative: &str, from: &str, to: &str) -> Result<(), String> {
    let path = root.join(relative);
    let name = path
        .file_name()
        .map(|n| n.to_string_lossy().to_string())
        .unwrap_or_default();
    if name.to_ascii_lowercase().ends_with(".sql") && name.to_ascii_lowercase().starts_with('v') {
        let new_name = name.replacen(&format!("V{from}__"), &format!("V{to}__"), 1);
        if new_name == name {
            return Err(format!(
                "no Flyway version {from} found in {}",
                path.display()
            ));
        }
        let new_path = path.with_file_name(new_name);
        fs::rename(&path, &new_path).map_err(|err| err.to_string())?;
        return Ok(());
    }
    let content = fs::read_to_string(&path).map_err(|err| err.to_string())?;
    let needle = format!("id=\"{from}\"");
    let replacement = format!("id=\"{to}\"");
    let mut changed = false;
    let mut output = String::with_capacity(content.len());
    for line in content.split_inclusive('\n') {
        if line.contains("<changeSet") && line.contains(&needle) {
            output.push_str(&line.replacen(&needle, &replacement, 1));
            changed = true;
        } else {
            output.push_str(line);
        }
    }
    if !changed {
        return Err(format!(
            "no changeSet id={from} found in {}",
            path.display()
        ));
    }
    workspace::atomic_write(&path, output.as_bytes()).map_err(|err| err.to_string())?;
    Ok(())
}

fn resequence_command(master: &Path, args: &[String]) -> i32 {
    let (Some(agent), Some(file), Some(from), Some(to)) = (
        parse_flag(args, "--agent"),
        parse_flag(args, "--file"),
        parse_flag(args, "--from"),
        parse_flag(args, "--to"),
    ) else {
        eprintln!("usage: sp resequence --agent <name> --file <relative> --from <key> --to <key>");
        return 2;
    };
    let session = match ensure_session(master, &agent) {
        Ok(session) => session,
        Err(err) => {
            eprintln!("sp resequence: {err}");
            return 2;
        }
    };
    let target = Path::new(&session.workspace).join(&file);
    let Some((repo_root, relative)) = session_repo_for_file(&session, &target) else {
        eprintln!(
            "sp resequence: file is outside the session workspace: {}",
            target.display()
        );
        return 2;
    };
    match resequence_file(Path::new(&repo_root), &relative, &from, &to) {
        Ok(()) => {}
        Err(err) => {
            eprintln!("sp resequence: {err}");
            return 2;
        }
    }
    if let Err(err) = git_engine::stage_path(Path::new(&repo_root), &relative) {
        eprintln!("sp resequence: edited but cannot stage {}: {err}", relative);
        return 2;
    }
    // The rewrite happens in the session worktree, but it is still a
    // coordination action: record it so RUNNING_REPORT.md shows the correction.
    let _ = client::call(
        master,
        "audit.record",
        Some(&agent),
        serde_json::json!({
            "kind": "sequence-resequenced",
            "severity": "info",
            "message": format!(
                "agent {agent} resequenced {file} from {from} to {to} and staged it"
            ),
        }),
    );
    let style = Style::stdout();
    println!(
        "{} {} {} {}",
        style.header("resequence"),
        style.dim(&format!("{agent}:{file}")),
        style.kv("from", &from),
        style.kv("to", &to)
    );
    println!("  {}", style.good("staged"));
    0
}

fn sequence_command(master: &Path, args: &[String]) -> i32 {
    if args.get(1).map(|s| s.as_str()) != Some("next") {
        eprintln!("usage: sp sequence next --namespace <name> --after <n>");
        return 2;
    }
    let Some(namespace) = parse_flag(args, "--namespace") else {
        eprintln!("sp sequence next: --namespace is required");
        return 2;
    };
    let after = parse_flag(args, "--after")
        .and_then(|value| value.parse::<u64>().ok())
        .unwrap_or(0);
    client::ensure_daemon(master).ok();
    match client::call(
        master,
        "sequence.next",
        None,
        json!({"namespace": namespace, "after": after}),
    ) {
        Ok(value) => {
            println!("{}", value.get("key").and_then(Value::as_str).unwrap_or(""));
            0
        }
        Err(err) => {
            eprintln!("sp sequence next: {err}");
            2
        }
    }
}

fn sync_command(master: &Path, args: &[String]) -> i32 {
    let Some(agent) = parse_flag(args, "--agent") else {
        eprintln!("usage: sp sync --rebase --agent <name> [--repo <name>]");
        return 2;
    };
    let session = match ensure_session(master, &agent) {
        Ok(session) => session,
        Err(err) => {
            eprintln!("sp sync: {err}");
            return 2;
        }
    };
    let filter = parse_flag(args, "--repo");
    let rebase = args.iter().any(|arg| arg == "--rebase");
    for repo in &session.repos {
        if let Some(filter) = &filter {
            if &repo.name != filter && &repo.relative_path != filter {
                continue;
            }
        }
        let root = Path::new(&repo.worktree);
        if !git_engine::is_clean(root).unwrap_or(false) {
            eprintln!(
                "sp sync: worktree for {} is dirty; commit or stash changes first",
                repo.name
            );
            return 2;
        }
        if rebase {
            let base = git_engine::base_branch(root);
            let onto = if git_engine::has_remote(root) {
                if git_engine::fetch(root).is_err() {
                    eprintln!("sp sync: fetch failed for {}", repo.name);
                    return 2;
                }
                format!("origin/{base}")
            } else {
                base.clone()
            };
            if let Err(err) = git_engine::rebase(root, &onto) {
                eprintln!("sp sync: rebase failed for {}: {err}", repo.name);
                return 2;
            }
            let refreshed = git_engine::rev_parse(root, &base).unwrap_or_default();
            if !refreshed.is_empty() {
                let _ = client::call(
                    master,
                    "session.refresh_base",
                    Some(&agent),
                    json!({
                        "agent": agent,
                        "repo": repo.relative_path,
                        "base_oid": refreshed,
                    }),
                );
            }
        }
    }
    let _ = client::call(
        master,
        "audit.record",
        Some(&agent),
        json!({
            "kind": "session-synchronized",
            "severity": "info",
            "message": format!(
                "agent {agent} synchronized{}",
                if rebase { " and rebased onto the base branch" } else { "" }
            ),
        }),
    );
    let style = Style::stdout();
    println!(
        "{} {}",
        style.header("sync"),
        style.good(&format!("session {agent} synchronized"))
    );
    0
}

fn barrier_command(master: &Path, args: &[String]) -> i32 {
    let Some(action) = args.get(1).map(|s| s.as_str()) else {
        eprintln!("usage: sp barrier <status|release>");
        return 2;
    };
    client::ensure_daemon(master).ok();
    let method = match action {
        "status" => "barrier.status",
        "release" => "barrier.release",
        "arrive" => "barrier.arrive",
        _ => {
            eprintln!("usage: sp barrier <status|arrive|release> [--agent <name>]");
            return 2;
        }
    };
    let agent = parse_flag(args, "--agent");
    let scope = parse_flag(args, "--scope").unwrap_or_else(|| "workspace".to_string());
    let component = parse_flag(args, "--component");
    match client::call(
        master,
        method,
        agent.as_deref(),
        json!({"agent": agent, "scope": scope, "component": component}),
    ) {
        Ok(value) => {
            println!("{}", serde_json::to_string_pretty(&value).unwrap());
            0
        }
        Err(err) => {
            eprintln!("sp barrier: {err}");
            2
        }
    }
}

fn evict_command(master: &Path, args: &[String]) -> i32 {
    let Some(agent) = parse_flag(args, "--agent") else {
        eprintln!("usage: sp evict --agent <name>");
        return 2;
    };
    client::ensure_daemon(master).ok();
    match client::call(
        master,
        "session.evict",
        Some(&agent),
        json!({"agent": agent}),
    ) {
        Ok(value) => {
            println!("{}", serde_json::to_string_pretty(&value).unwrap());
            0
        }
        Err(err) => {
            eprintln!("sp evict: {err}");
            2
        }
    }
}

fn notices_command(master: &Path, args: &[String]) -> i32 {
    client::ensure_daemon(master).ok();
    match client::call(
        master,
        "notice.list",
        None,
        json!({"after_seq": 0, "limit": 200}),
    ) {
        Ok(value) => {
            if args.iter().any(|arg| arg == "--json") {
                println!("{}", serde_json::to_string_pretty(&value).unwrap());
                return 0;
            }
            let style = Style::stdout();
            let notices = value
                .get("notices")
                .and_then(Value::as_array)
                .cloned()
                .unwrap_or_default();
            println!("{}", style.header("notices"));
            println!("  {}", style.dim(&format!("{} notice(s)", notices.len())));
            for notice in notices {
                let seq = notice.get("seq").and_then(Value::as_u64).unwrap_or(0);
                let severity = notice
                    .get("severity")
                    .and_then(Value::as_str)
                    .unwrap_or("-");
                let agent = notice.get("agent").and_then(Value::as_str).unwrap_or("-");
                let message = notice.get("message").and_then(Value::as_str).unwrap_or("");
                let tag = match severity {
                    "warning" => style.warn(severity),
                    "blocking" | "error" => style.bad(severity),
                    _ => style.dim(severity),
                };
                println!(
                    "  {} {} {} {}",
                    style.dim(&format!("#{seq}")),
                    tag,
                    style.bold(agent),
                    message
                );
            }
            0
        }
        Err(err) => {
            eprintln!("sp notices: {err}");
            2
        }
    }
}
