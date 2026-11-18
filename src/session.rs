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
    /// Compose files that pin `container_name:`, which defeats
    /// `COMPOSE_PROJECT_NAME` and lets two sessions collide on the same
    /// containers. Reported to the operator at attach time.
    pub pinned_containers: Vec<String>,
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
    let mut copied_config = Vec::new();
    for repo in &session_repos {
        let root_repo = if repo.relative_path == "." {
            master.to_path_buf()
        } else {
            master.join(&repo.relative_path)
        };
        let worktree = PathBuf::from(&repo.worktree);
        copied_config.extend(copy_ignored_config(&root_repo, &worktree));
    }
    if !copied_config.is_empty() {
        let manifest = base.join("local-config.txt");
        let body = copied_config.join("\n") + "\n";
        let _ = workspace::atomic_write(&manifest, body.as_bytes());
    }
    let mut pinned_containers = Vec::new();
    for repo in &session_repos {
        let root_repo = if repo.relative_path == "." {
            master.to_path_buf()
        } else {
            master.join(&repo.relative_path)
        };
        pinned_containers.extend(pinned_container_names(&root_repo));
    }

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
        lease_remaining_ms: 0,
        has_unpreserved_work: false,
    };
    save_session(master, &session).map_err(|err| err.to_string())?;
    Ok(Provisioned {
        session,
        pinned_containers,
    })
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

/// True when the session workspace holds work that a re-provision would destroy:
/// uncommitted edits in any worktree, or commits on the session branch that are
/// not reachable from the repository's base branch.
pub fn has_unpreserved_work(master: &Path, session: &SessionRecord) -> bool {
    for repo in &session.repos {
        let worktree = Path::new(&repo.worktree);
        if !worktree.is_dir() {
            continue;
        }
        if let Ok(false) = git_engine::is_clean(worktree) {
            return true;
        }
        let root_repo = if repo.relative_path == "." {
            master.to_path_buf()
        } else {
            master.join(&repo.relative_path)
        };
        if !root_repo.join(".git").exists() {
            continue;
        }
        let base = git_engine::base_branch(&root_repo);
        let ahead = git_engine::run(
            &root_repo,
            &["rev-list", "--count", &format!("{base}..{}", repo.branch)],
        );
        if let Ok(count) = ahead {
            if count.trim() != "0" && !count.trim().is_empty() {
                return true;
            }
        }
    }
    false
}

pub fn remove_session_worktrees(master: &Path, session: &SessionRecord) {
    for repo in &session.repos {
        // A single-repository workspace has `relative_path == "."`; skipping it
        // left a stale worktree registration behind that later blocked
        // `git worktree add` / `gh pr checkout` for every other agent.
        let root_repo = if repo.relative_path == "." {
            master.to_path_buf()
        } else {
            master.join(&repo.relative_path)
        };
        if root_repo.join(".git").exists() {
            git_engine::remove_worktree(&root_repo, Path::new(&repo.worktree));
        }
    }
}

/// Copies gitignored local configuration from a canonical repository into a fresh
/// worktree.
///
/// A worktree only materialises tracked files, so anything a developer keeps out
/// of Git — `application-dev.yaml`, `.env`, local keystores — is absent. Services
/// then fail with errors that look like code bugs (`UnknownHostException:
/// ${DB_HOST}`) rather than a provisioning gap. Copying the ignored files that
/// look like configuration closes that hole; the list is written to
/// `local-config.txt` so the agent can see what it inherited.
fn copy_ignored_config(repo: &Path, worktree: &Path) -> Vec<String> {
    let mut copied = Vec::new();
    // No `--directory`: it collapses ignored directories to a single entry, which
    // would hide `config/application-dev.yaml` behind `config/`.
    let Ok(output) = git_engine::run_output(
        repo,
        &["ls-files", "--others", "--ignored", "--exclude-standard"],
    ) else {
        return copied;
    };
    if !output.status.success() {
        return copied;
    }
    for line in String::from_utf8_lossy(&output.stdout).lines() {
        let relative = line.trim().trim_end_matches('/');
        if relative.is_empty() || !looks_like_config(relative) {
            continue;
        }
        let src = repo.join(relative);
        let dst = worktree.join(relative);
        if !src.is_file() || dst.exists() {
            continue;
        }
        if workspace::copy_cow_file(&src, &dst).is_ok() {
            copied.push(relative.to_string());
        }
    }
    copied
}

/// Heuristic gate for "this ignored file is local configuration, not build output".
/// Copying a `target/` or `node_modules/` tree would defeat the point of a cheap
/// worktree, so the match is deliberately narrow.
fn looks_like_config(relative: &str) -> bool {
    const DIRS: [&str; 3] = ["config/", "configs/", "secrets/"];
    if DIRS.iter().any(|dir| relative.starts_with(dir)) {
        return true;
    }
    let name = relative.rsplit('/').next().unwrap_or(relative);
    const EXACT: [&str; 6] = [
        ".env",
        ".env.local",
        "application-local.yaml",
        "application-local.yml",
        "application-local.properties",
        "local.properties",
    ];
    if EXACT.contains(&name) {
        return true;
    }
    const SUFFIXES: [&str; 6] = [".env", ".pem", ".p12", ".jks", ".keystore", ".local.yaml"];
    if SUFFIXES.iter().any(|suffix| name.ends_with(suffix)) {
        return true;
    }
    // `application-dev.yaml`, `application-prod.yml`, `bootstrap-local.properties`
    (name.starts_with("application-") || name.starts_with("bootstrap-"))
        && (name.ends_with(".yaml") || name.ends_with(".yml") || name.ends_with(".properties"))
}

/// Compose files that hardcode `container_name:` cannot be isolated by
/// `COMPOSE_PROJECT_NAME`; two sessions will fight over the same containers and
/// therefore the same database. Detect them so the operator is told at attach
/// time instead of discovering it as a mysterious name conflict.
fn pinned_container_names(repo: &Path) -> Vec<String> {
    let mut found = Vec::new();
    for name in [
        "docker-compose.yml",
        "docker-compose.yaml",
        "compose.yml",
        "compose.yaml",
    ] {
        let path = repo.join(name);
        let Ok(body) = fs::read_to_string(&path) else {
            continue;
        };
        let pinned = body
            .lines()
            .filter_map(|line| line.trim().strip_prefix("container_name:"))
            .map(|value| value.trim().trim_matches(['"', '\'']).to_string())
            .filter(|value| !value.is_empty())
            .collect::<Vec<_>>();
        if !pinned.is_empty() {
            found.push(format!("{name} pins {}", pinned.join(", ")));
        }
    }
    found
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn config_heuristic_takes_local_config_but_not_build_output() {
        for wanted in [
            "config/application-dev.yaml",
            "configs/db.yml",
            "secrets/token",
            ".env",
            ".env.local",
            "application-local.yaml",
            "local.properties",
            "certs/client.pem",
            "keys/signing.p12",
            "application-prod.yaml",
            "bootstrap-ci.yml",
        ] {
            assert!(looks_like_config(wanted), "{wanted} should be copied");
        }
        for unwanted in [
            "target/classes/App.class",
            "node_modules/left-pad/index.js",
            "build/libs/app.jar",
            "src/main/java/App.java",
            "README.md",
        ] {
            assert!(
                !looks_like_config(unwanted),
                "{unwanted} must not be copied"
            );
        }
    }

    #[test]
    fn pinned_container_names_reads_compose_files() {
        let dir = std::env::temp_dir().join(format!("sp-compose-{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();
        fs::write(
            dir.join("docker-compose.yml"),
            "services:\n  db:\n    container_name: pdh-postgres\n  sftp:\n    container_name: \"pdh-sftp\"\n",
        )
        .unwrap();
        let found = pinned_container_names(&dir);
        assert_eq!(found.len(), 1);
        assert!(found[0].contains("pdh-postgres"));
        assert!(found[0].contains("pdh-sftp"));
        let _ = fs::remove_dir_all(&dir);
    }
}
