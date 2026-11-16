use crate::client;
use crate::git_engine;
use crate::prompt;
use crate::protocol::{CommitDecision, ConflictReport};
use crate::workspace;
use serde_json::json;
use std::ffi::OsString;
use std::fs;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

fn real_git_command() -> Command {
    let mut cmd = Command::new(git_engine::real_git());
    cmd.env("GIT_TERMINAL_PROMPT", "0")
        .env("GIT_CONFIG_NOSYSTEM", "1")
        .env("GIT_PAGER", "cat")
        .env("LC_ALL", "C")
        .env_remove("GIT_DIR")
        .env_remove("GIT_WORK_TREE")
        .env_remove("GIT_INDEX_FILE");
    cmd
}

fn exec_real_git(args: &[String]) -> i32 {
    let mut cmd = real_git_command();
    cmd.args(args);
    match cmd.status() {
        Ok(status) => status.code().unwrap_or(2),
        Err(err) => {
            eprintln!("scratchpost: cannot execute git: {err}");
            2
        }
    }
}

fn session_context() -> Option<(PathBuf, PathBuf, String)> {
    let agent = std::env::var("SP_AGENT").ok()?;
    let master = std::env::var("SP_MASTER_ROOT").ok().map(PathBuf::from)?;
    let session_root = std::env::var("SP_SESSION_ROOT").ok().map(PathBuf::from)?;
    Some((
        workspace::canonical(&master),
        workspace::canonical(&session_root),
        agent,
    ))
}

fn print_plain_block(code: &str, message: &str) -> i32 {
    eprintln!("SP_BLOCKED v1 code={code} conflict_id=0000000000000000");
    eprintln!("{message}");
    eprintln!("Exit status: 1");
    1
}

fn repo_relative(repo_root: &Path, session_root: &Path) -> Option<String> {
    let relative = repo_root.strip_prefix(session_root).ok()?;
    if relative.as_os_str().is_empty() {
        Some(".".to_string())
    } else {
        Some(relative.to_string_lossy().replace('\\', "/"))
    }
}

fn capture_git(repo: &Path, args: &[&str], env: &[(String, String)]) -> Result<String, String> {
    let mut cmd = Command::new(git_engine::real_git());
    cmd.current_dir(repo)
        .args(args)
        .env("GIT_TERMINAL_PROMPT", "0")
        .env("GIT_CONFIG_NOSYSTEM", "1")
        .env("LC_ALL", "C")
        .env_remove("GIT_DIR")
        .env_remove("GIT_WORK_TREE")
        .env_remove("GIT_INDEX_FILE");
    for (key, value) in env {
        cmd.env(key, value);
    }
    let output = cmd.output().map_err(|err| err.to_string())?;
    if !output.status.success() {
        return Err(String::from_utf8_lossy(&output.stderr).trim().to_string());
    }
    Ok(String::from_utf8_lossy(&output.stdout).trim().to_string())
}

fn write_tree_from_index(repo: &Path, index_file: &Path) -> Result<String, String> {
    capture_git(
        repo,
        &["write-tree"],
        &[(
            "GIT_INDEX_FILE".to_string(),
            index_file.to_string_lossy().to_string(),
        )],
    )
}

fn copy_index(repo: &Path, destination: &Path) -> Result<(), String> {
    let index = git_engine::git_path(repo, "index").map_err(|err| err.to_string())?;
    if !index.exists() {
        return Err(format!(
            "repository index does not exist: {}",
            index.display()
        ));
    }
    if let Some(parent) = destination.parent() {
        fs::create_dir_all(parent).map_err(|err| err.to_string())?;
    }
    fs::copy(&index, destination).map_err(|err| err.to_string())?;
    Ok(())
}

/// Identity of the commit being gated: who is committing, where, and onto what.
struct CommitContext<'a> {
    master: &'a Path,
    agent: &'a str,
    repo_root: &'a Path,
    repo_relative: &'a str,
    branch: &'a str,
    head_oid: &'a str,
}

fn prepare_commit(ctx: &CommitContext<'_>, index_tree: &str) -> Result<CommitDecision, String> {
    let staged_paths = if ctx.head_oid.is_empty() {
        git_engine::staged_names(ctx.repo_root).unwrap_or_default()
    } else {
        git_engine::diff_names(ctx.repo_root, ctx.head_oid, index_tree).unwrap_or_default()
    };
    let data = client::call(
        ctx.master,
        "commit.prepare",
        Some(ctx.agent),
        json!({
            "agent": ctx.agent,
            "repo": ctx.repo_relative,
            "branch": ctx.branch,
            "index_tree": index_tree,
            "head_oid": ctx.head_oid,
            "staged_paths": staged_paths
        }),
    )
    .map_err(|err| err.to_string())?;
    serde_json::from_value::<CommitDecision>(data).map_err(|err| err.to_string())
}

fn record_commit(
    master: &Path,
    agent: &str,
    permit_id: &str,
    repo: &Path,
    repo_relative: &str,
    index_tree: &str,
) -> Result<(), String> {
    let commit = git_engine::head_oid_fast(repo)
        .ok_or_else(|| "cannot resolve HEAD after commit".to_string())?;
    let tree = index_tree.to_string();
    client::call(
        master,
        "commit.record",
        Some(agent),
        json!({
            "agent": agent,
            "permit_id": permit_id,
            "commit_oid": commit,
            "tree_oid": tree,
            "repo": repo_relative
        }),
    )
    .map_err(|err| err.to_string())?;
    Ok(())
}

fn blocked_report(decision: &CommitDecision) -> Option<&ConflictReport> {
    decision.conflict.as_ref()
}

fn commit_allowed(
    ctx: &CommitContext<'_>,
    index_file: &Path,
    args: &[String],
) -> Result<i32, String> {
    let index_tree = write_tree_from_index(ctx.repo_root, index_file)?;
    let decision = prepare_commit(ctx, &index_tree)?;
    if !decision.allowed {
        if let Some(report) = blocked_report(&decision) {
            eprint!("{}", prompt::render_stderr(report));
        } else {
            eprintln!("SP_BLOCKED v1 code=SP_INTENT_CONFLICT conflict_id=0000000000000000");
            eprintln!("commit blocked by coordination policy");
        }
        return Ok(1);
    }
    let permit = decision
        .permit
        .ok_or_else(|| "daemon allowed commit without issuing a permit".to_string())?;

    let mut cmd = real_git_command();
    cmd.current_dir(ctx.repo_root)
        .args(args)
        .env("GIT_INDEX_FILE", index_file.to_string_lossy().to_string())
        .env("SP_COMMIT_PERMIT", &permit.permit_id)
        .env("SP_MANAGED_GATE", "1")
        .stdin(Stdio::inherit())
        .stdout(Stdio::inherit())
        .stderr(Stdio::inherit());
    let status = cmd.status().map_err(|err| err.to_string())?;
    if status.success() {
        if let Err(err) = record_commit(
            ctx.master,
            ctx.agent,
            &permit.permit_id,
            ctx.repo_root,
            ctx.repo_relative,
            &permit.index_tree,
        ) {
            eprintln!("scratchpost: commit succeeded but ledger recording failed: {err}");
            return Ok(2);
        }
        Ok(0)
    } else {
        let _ = client::call(
            ctx.master,
            "commit.abort",
            Some(ctx.agent),
            json!({"agent": ctx.agent, "permit_id": permit.permit_id}),
        );
        Ok(status.code().unwrap_or(1))
    }
}

pub fn run_git_shim(args: &[String]) -> i32 {
    let Some((master, session_root, agent)) = session_context() else {
        return exec_real_git(args);
    };
    let cwd = std::env::current_dir().unwrap_or_else(|_| session_root.clone());
    let cwd = workspace::canonical(&cwd);
    if !cwd.starts_with(&session_root) {
        return print_plain_block(
            "SP_PATH_OUTSIDE_SESSION",
            &format!(
                "git was invoked outside the session workspace.\nrequested: {}\nsession_root: {}\ncd into the session workspace before running git.",
                cwd.display(),
                session_root.display()
            ),
        );
    }
    if let Err(err) = client::ensure_daemon(&master) {
        return print_plain_block(
            "SP_DAEMON_UNAVAILABLE",
            &format!("cannot reach scratchpostd: {err}\nCommit is blocked to preserve coordination state."),
        );
    }
    client::emit_notices(&master, Some(&agent));
    if git_engine::is_commit(args) {
        if args.iter().any(|arg| arg == "--no-verify") {
            return print_plain_block(
                "SP_GIT_INVOCATION_REJECTED",
                "`git commit --no-verify` is not permitted in a ScratchPost session.",
            );
        }
        if args
            .iter()
            .any(|arg| arg == "-a" || arg == "--all" || arg == "--git-dir" || arg == "-C")
        {
            return print_plain_block(
                "SP_GIT_INVOCATION_REJECTED",
                "commit arguments that bypass the immutable index snapshot are not permitted; stage files explicitly.",
            );
        }
        let repo_root = git_engine::discover_root(&cwd)
            .map(|root| workspace::canonical(&root))
            .unwrap_or_else(|| cwd.clone());
        let head_oid = git_engine::head_oid_fast(&repo_root).unwrap_or_default();
        let branch = git_engine::current_branch(&repo_root).unwrap_or_else(|_| "HEAD".to_string());
        let repo_relative = match repo_relative(&repo_root, &session_root) {
            Some(value) => value,
            None => {
                return print_plain_block(
                    "SP_PATH_OUTSIDE_SESSION",
                    "the Git repository is outside the session workspace",
                );
            }
        };
        let index_file = workspace::sessions_dir(&master)
            .join(&agent)
            .join(format!("index-{}.snapshot", std::process::id()));
        if let Err(err) = copy_index(&repo_root, &index_file) {
            return print_plain_block(
                "SP_REPOSITORY_UNAVAILABLE",
                &format!("cannot snapshot the staged index: {err}"),
            );
        }
        let ctx = CommitContext {
            master: &master,
            agent: &agent,
            repo_root: &repo_root,
            repo_relative: &repo_relative,
            branch: &branch,
            head_oid: &head_oid,
        };
        let result = commit_allowed(&ctx, &index_file, args);
        let _ = fs::remove_file(&index_file);
        match result {
            Ok(code) => code,
            Err(err) => {
                print_plain_block("SP_INTERNAL", &format!("commit gate failed closed: {err}"))
            }
        }
    } else if git_engine::is_push(args) {
        let branch =
            capture_git(&cwd, &["rev-parse", "--abbrev-ref", "HEAD"], &[]).unwrap_or_default();
        if branch == "main" || branch == "master" {
            return print_plain_block(
                "SP_GIT_INVOCATION_REJECTED",
                "direct pushes to the canonical branch are not permitted; publish through a ScratchPost session branch.",
            );
        }
        std::env::set_var("SP_MANAGED_GATE", "1");
        exec_real_git(args)
    } else {
        exec_real_git(args)
    }
}

pub fn run_hook(name: &str, _args: &[String]) -> i32 {
    let Some((master, session_root, agent)) = session_context() else {
        return 0;
    };
    let cwd = std::env::current_dir().unwrap_or_else(|_| session_root.clone());
    if let Err(err) = client::ensure_daemon(&master) {
        eprintln!("SP_BLOCKED v1 code=SP_DAEMON_UNAVAILABLE conflict_id=0000000000000000");
        eprintln!("scratchpostd is unavailable: {err}");
        eprintln!("Exit status: 1");
        return 1;
    }
    if name == "pre-push" {
        if std::env::var("SP_MANAGED_GATE").ok().as_deref() != Some("1") {
            eprintln!("SP_BLOCKED v1 code=SP_GIT_INVOCATION_REJECTED conflict_id=0000000000000000");
            eprintln!("pushes must originate from the managed ScratchPost git shim");
            eprintln!("Exit status: 1");
            return 1;
        }
        let branch =
            capture_git(&cwd, &["rev-parse", "--abbrev-ref", "HEAD"], &[]).unwrap_or_default();
        if branch == "main" || branch == "master" {
            eprintln!("SP_BLOCKED v1 code=SP_GIT_INVOCATION_REJECTED conflict_id=0000000000000000");
            eprintln!("direct push to {branch} is not permitted");
            eprintln!("Exit status: 1");
            return 1;
        }
        return 0;
    }
    if name != "pre-commit" {
        return 0;
    }
    if std::env::var("SP_MANAGED_GATE").ok().as_deref() != Some("1") {
        eprintln!("SP_BLOCKED v1 code=SP_GIT_INVOCATION_REJECTED conflict_id=0000000000000000");
        eprintln!("commits must originate from the managed ScratchPost git shim; direct git invocations are rejected inside a session");
        eprintln!("Exit status: 1");
        return 1;
    }
    if let Ok(permit) = std::env::var("SP_COMMIT_PERMIT") {
        if !permit.is_empty() {
            match client::call(
                &master,
                "permit.verify",
                Some(&agent),
                json!({"permit_id": permit}),
            ) {
                Ok(_) => return 0,
                Err(err) => {
                    eprintln!(
                        "SP_BLOCKED v1 code=SP_COMMIT_PERMIT_INVALID conflict_id=0000000000000000"
                    );
                    eprintln!("{err}");
                    eprintln!("Exit status: 1");
                    return 1;
                }
            }
        }
    }
    let repo_root = capture_git(&cwd, &["rev-parse", "--show-toplevel"], &[])
        .ok()
        .map(PathBuf::from)
        .unwrap_or_else(|| cwd.clone());
    let repo_relative = repo_relative(&repo_root, &session_root).unwrap_or_else(|| ".".to_string());
    let branch = capture_git(&repo_root, &["rev-parse", "--abbrev-ref", "HEAD"], &[])
        .unwrap_or_else(|_| "HEAD".to_string());
    let index_file = workspace::sessions_dir(&master)
        .join(&agent)
        .join(format!("hook-index-{}.snapshot", std::process::id()));
    if let Err(err) = copy_index(&repo_root, &index_file) {
        eprintln!("SP_BLOCKED v1 code=SP_REPOSITORY_UNAVAILABLE conflict_id=0000000000000000");
        eprintln!("cannot inspect staged index: {err}");
        eprintln!("Exit status: 1");
        return 1;
    }
    let tree = match write_tree_from_index(&repo_root, &index_file) {
        Ok(tree) => tree,
        Err(err) => {
            let _ = fs::remove_file(&index_file);
            eprintln!("SP_BLOCKED v1 code=SP_REPOSITORY_UNAVAILABLE conflict_id=0000000000000000");
            eprintln!("cannot compute staged tree: {err}");
            eprintln!("Exit status: 1");
            return 1;
        }
    };
    let data = client::call(
        &master,
        "commit.validate",
        Some(&agent),
        json!({
            "agent": agent,
            "repo": repo_relative,
            "branch": branch,
            "index_tree": tree,
        }),
    );
    let _ = fs::remove_file(&index_file);
    match data {
        Ok(value) => {
            if value.get("allowed").and_then(|v| v.as_bool()) == Some(true) {
                0
            } else {
                let decision: CommitDecision =
                    serde_json::from_value(value).unwrap_or(CommitDecision {
                        allowed: false,
                        permit: None,
                        conflict: None,
                    });
                if let Some(report) = decision.conflict {
                    eprint!("{}", prompt::render_stderr(&report));
                } else {
                    eprintln!("SP_BLOCKED v1 code=SP_INTENT_CONFLICT conflict_id=0000000000000000");
                    eprintln!("commit blocked by coordination policy");
                }
                1
            }
        }
        Err(err) => {
            eprintln!(
                "SP_BLOCKED v1 code={} conflict_id=0000000000000000",
                err.code
            );
            eprintln!("{}", err.message);
            eprintln!("Exit status: 1");
            1
        }
    }
}

pub fn shell_path(master: &Path) -> OsString {
    let mut parts = vec![workspace::bin_dir(master).to_string_lossy().to_string()];
    if let Ok(path) = std::env::var("PATH") {
        parts.push(path);
    }
    OsString::from(parts.join(":"))
}
