use crate::git_engine;
use crate::protocol::{now_ms, SessionRecord, SessionRepo, SessionStatus};
use crate::workspace;
use std::fs;
use std::path::{Path, PathBuf};

pub fn session_dir(master: &Path, agent: &str) -> PathBuf {
    workspace::sessions_dir(master).join(agent)
}

pub fn session_metadata(master: &Path, agent: &str) -> PathBuf {
    session_dir(master, agent).join("session.json")
}

pub fn save_session(master: &Path, session: &SessionRecord) -> std::io::Result<()> {
    workspace::atomic_write(
        &session_metadata(master, &session.agent),
        &serde_json::to_vec_pretty(session).map_err(std::io::Error::other)?,
    )
}

pub struct Provisioned {
    pub session: SessionRecord,
}

pub fn provision(
    master: &Path,
    agent: &str,
    slot: u16,
    port_base: u16,
    session_id: String,
    branch_id: String,
) -> Result<Provisioned, String> {
    let base = session_dir(master, agent);
    let workspace = base.join("workspace");
    if base.exists() {
        fs::remove_dir_all(&base).map_err(|err| {
            format!(
                "cannot remove stale session directory {}: {err}",
                base.display()
            )
        })?;
    }
    fs::create_dir_all(&workspace).map_err(|err| err.to_string())?;
    for cache in ["m2", "gradle", "npm", "cargo", "tmp"] {
        fs::create_dir_all(base.join("caches").join(cache)).map_err(|err| err.to_string())?;
    }

    let branch = format!("sp/{agent}/{branch_id}");
    let repos = git_engine::discover_repos(master);
    if repos.is_empty() {
        return Err(format!(
            "no Git repositories found under master workspace {}",
            master.display()
        ));
    }

    let mut session_repos = Vec::new();
    for (name, repo_path) in repos {
        let default_branch = git_engine::base_branch(&repo_path);
        let base_oid = git_engine::rev_parse(&repo_path, &default_branch)
            .map_err(|err| format!("cannot resolve {}: {err}", default_branch))?;
        let relative = if name == "." {
            ".".to_string()
        } else {
            name.clone()
        };
        let worktree = if relative == "." {
            workspace.clone()
        } else {
            workspace.join(&relative)
        };
        git_engine::add_worktree(&repo_path, &worktree, &branch, &default_branch)
            .map_err(|err| format!("cannot provision worktree for {}: {err}", name))?;
        git_engine::install_repo_config(master, &repo_path)
            .map_err(|err| format!("cannot configure repository {}: {err}", name))?;
        session_repos.push(SessionRepo {
            name: if name == "." {
                master
                    .file_name()
                    .map(|s| s.to_string_lossy().to_string())
                    .unwrap_or_else(|| "workspace".to_string())
            } else {
                name
            },
            relative_path: relative,
            worktree: worktree.to_string_lossy().to_string(),
            branch: branch.clone(),
            base_oid,
        });
    }

    copy_root_context(master, &workspace)?;

    let session = SessionRecord {
        agent: agent.to_string(),
        session_id,
        slot,
        port_base,
        workspace: workspace.to_string_lossy().to_string(),
        created_ms: now_ms(),
        last_heartbeat_ms: now_ms(),
        status: SessionStatus::Active,
        repos: session_repos,
    };
    save_session(master, &session).map_err(|err| err.to_string())?;
    Ok(Provisioned { session })
}

fn copy_root_context(master: &Path, workspace: &Path) -> Result<(), String> {
    let entries = fs::read_dir(master).map_err(|err| err.to_string())?;
    for entry in entries.flatten() {
        let path = entry.path();
        let name = entry.file_name();
        if name == ".git" || name == ".scratchpost" {
            continue;
        }
        if !path.is_file() {
            continue;
        }
        let dest = workspace.join(&name);
        workspace::copy_cow_file(&path, &dest).map_err(|err| {
            format!(
                "cannot copy root context {} to {}: {err}",
                path.display(),
                dest.display()
            )
        })?;
    }
    Ok(())
}

pub fn resource_env(session: &SessionRecord, master: &Path) -> Vec<(String, String)> {
    let base = session_dir(master, &session.agent);
    let caches = base.join("caches");
    let compose = format!(
        "sp_{}_{}",
        session.agent.to_ascii_lowercase().replace(['.', '-'], "_"),
        session.slot
    );
    vec![
        (
            "SP_MASTER_ROOT".to_string(),
            master.to_string_lossy().to_string(),
        ),
        ("SP_AGENT".to_string(), session.agent.clone()),
        ("SP_SESSION_ID".to_string(), session.session_id.clone()),
        ("SP_SESSION_ROOT".to_string(), session.workspace.clone()),
        ("SP_PORT_BASE".to_string(), session.port_base.to_string()),
        ("SERVER_PORT".to_string(), session.port_base.to_string()),
        ("COMPOSE_PROJECT_NAME".to_string(), compose),
        (
            "TMPDIR".to_string(),
            caches.join("tmp").to_string_lossy().to_string(),
        ),
        (
            "MAVEN_OPTS".to_string(),
            format!("-Dmaven.repo.local={}", caches.join("m2").display()),
        ),
        (
            "GRADLE_USER_HOME".to_string(),
            caches.join("gradle").to_string_lossy().to_string(),
        ),
        (
            "npm_config_cache".to_string(),
            caches.join("npm").to_string_lossy().to_string(),
        ),
        (
            "CARGO_TARGET_DIR".to_string(),
            caches.join("cargo").to_string_lossy().to_string(),
        ),
    ]
}

pub fn remove_session_worktrees(master: &Path, session: &SessionRecord) {
    for repo in &session.repos {
        if repo.relative_path == "." {
            continue;
        }
        let root_repo = master.join(&repo.relative_path);
        if root_repo.join(".git").exists() {
            git_engine::remove_worktree(&root_repo, Path::new(&repo.worktree));
        }
    }
}
