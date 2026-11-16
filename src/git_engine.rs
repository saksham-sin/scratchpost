use std::collections::HashMap;
use std::ffi::OsStr;
use std::fs;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::{Command, Output, Stdio};

#[derive(Debug, Clone)]
pub struct GitError {
    pub message: String,
}

impl std::fmt::Display for GitError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.message)
    }
}

impl std::error::Error for GitError {}

impl From<std::io::Error> for GitError {
    fn from(value: std::io::Error) -> Self {
        Self {
            message: value.to_string(),
        }
    }
}

pub fn real_git() -> PathBuf {
    if let Ok(path) = std::env::var("SP_REAL_GIT") {
        if !path.is_empty() {
            return PathBuf::from(path);
        }
    }
    for candidate in [
        "/usr/bin/git",
        "/opt/homebrew/bin/git",
        "/usr/local/bin/git",
    ] {
        if Path::new(candidate).is_file() {
            return PathBuf::from(candidate);
        }
    }
    if let Ok(path) = std::env::var("PATH") {
        for dir in path.split(':') {
            if dir.contains(".scratchpost") {
                continue;
            }
            let candidate = Path::new(dir).join("git");
            if candidate.is_file() {
                return candidate;
            }
        }
    }
    PathBuf::from("git")
}

pub fn run(repo: &Path, args: &[&str]) -> Result<String, GitError> {
    let output = run_output(repo, args)?;
    if !output.status.success() {
        return Err(GitError {
            message: format!(
                "git {} failed: {}",
                args.join(" "),
                String::from_utf8_lossy(&output.stderr).trim()
            ),
        });
    }
    Ok(String::from_utf8_lossy(&output.stdout).trim().to_string())
}

pub fn run_output(repo: &Path, args: &[&str]) -> Result<Output, GitError> {
    let mut cmd = Command::new(real_git());
    cmd.arg("-C").arg(repo).args(args);
    cmd.env("GIT_TERMINAL_PROMPT", "0");
    cmd.env("GIT_CONFIG_NOSYSTEM", "1");
    cmd.env("GIT_PAGER", "cat");
    cmd.env("LC_ALL", "C");
    cmd.env_remove("GIT_DIR");
    cmd.env_remove("GIT_WORK_TREE");
    cmd.env_remove("GIT_INDEX_FILE");
    let output = cmd.output()?;
    Ok(output)
}

pub fn current_branch(repo: &Path) -> Result<String, GitError> {
    run(repo, &["symbolic-ref", "--quiet", "--short", "HEAD"]).map_err(|_| GitError {
        message: format!("repository {} has no checked-out branch", repo.display()),
    })
}

pub fn rev_parse(repo: &Path, rev: &str) -> Result<String, GitError> {
    run(repo, &["rev-parse", rev])
}

/// Resolves the coordination base branch name. Stable for the lifetime of a repository,
/// so callers cache the result instead of probing on every commit.
pub fn resolve_base_branch(repo: &Path) -> String {
    base_branch(repo)
}

pub fn base_branch(repo: &Path) -> String {
    let current = current_branch(repo).ok();
    if let Some(branch) = current.as_ref() {
        if branch == "main" || branch == "master" {
            return branch.clone();
        }
    }
    if run_output(repo, &["rev-parse", "--verify", "refs/heads/main"])
        .map(|o| o.status.success())
        .unwrap_or(false)
    {
        return "main".to_string();
    }
    if run_output(repo, &["rev-parse", "--verify", "refs/heads/master"])
        .map(|o| o.status.success())
        .unwrap_or(false)
    {
        return "master".to_string();
    }
    current.unwrap_or_else(|| "HEAD".to_string())
}

/// Walks up from `start` to the enclosing repository root without spawning Git.
pub fn discover_root(start: &Path) -> Option<PathBuf> {
    let mut current = Some(start);
    while let Some(dir) = current {
        if dir.join(".git").exists() {
            return Some(dir.to_path_buf());
        }
        current = dir.parent();
    }
    None
}

/// Resolves the Git directory for `repo`, following worktree `gitdir` pointers and
/// `commondir` indirection so ref lookups land in the shared object store.
fn git_dirs(repo: &Path) -> Option<(PathBuf, PathBuf)> {
    let dot_git = repo.join(".git");
    let git_dir = if dot_git.is_dir() {
        dot_git
    } else {
        let pointer = fs::read_to_string(&dot_git).ok()?;
        let target = pointer.trim().strip_prefix("gitdir:")?.trim();
        let path = Path::new(target);
        if path.is_absolute() {
            path.to_path_buf()
        } else {
            repo.join(path)
        }
    };
    let common = match fs::read_to_string(git_dir.join("commondir")).ok() {
        Some(value) => {
            let path = Path::new(value.trim());
            if path.is_absolute() {
                path.to_path_buf()
            } else {
                git_dir.join(path)
            }
        }
        None => git_dir.clone(),
    };
    Some((git_dir, common))
}

/// Reads a branch commit id straight from the ref files, avoiding a Git subprocess.
pub fn ref_oid_fast(repo: &Path, branch: &str) -> Option<String> {
    let (_, common) = git_dirs(repo)?;
    fs::read_to_string(common.join("refs").join("heads").join(branch))
        .ok()
        .map(|value| value.trim().to_string())
}

/// Reads the `HEAD` commit id straight from the ref files, avoiding a Git subprocess.
/// Returns `None` for packed refs or detached symbolic chains so callers can fall back.
pub fn head_oid_fast(repo: &Path) -> Option<String> {
    let (git_dir, common) = git_dirs(repo)?;
    let head = fs::read_to_string(git_dir.join("HEAD")).ok()?;
    let head = head.trim();
    let reference = match head.strip_prefix("ref:") {
        Some(reference) => reference.trim(),
        None => return Some(head.to_string()),
    };
    let loose = fs::read_to_string(common.join(reference)).ok();
    loose.map(|value| value.trim().to_string())
}

pub fn is_clean(repo: &Path) -> Result<bool, GitError> {
    Ok(run(repo, &["status", "--porcelain"])?.is_empty())
}

pub fn discover_repos(master: &Path) -> Vec<(String, PathBuf)> {
    let mut repos = Vec::new();
    if master.join(".git").exists() {
        repos.push((".".to_string(), master.to_path_buf()));
    }
    if let Ok(entries) = fs::read_dir(master) {
        let mut children = Vec::new();
        for entry in entries.flatten() {
            let path = entry.path();
            if !path.is_dir() {
                continue;
            }
            let name = entry.file_name().to_string_lossy().to_string();
            if name == ".scratchpost" || name.starts_with('.') {
                continue;
            }
            if path.join(".git").exists() {
                children.push((name, path));
            }
        }
        children.sort_by(|a, b| a.0.cmp(&b.0));
        repos.extend(children);
    }
    repos
}

pub fn install_repo_config(master: &Path, repo: &Path) -> Result<(), GitError> {
    let hooks = crate::workspace::hooks_dir(master);
    run(
        repo,
        &["config", "core.hooksPath", hooks.to_string_lossy().as_ref()],
    )?;
    run(repo, &["config", "gc.auto", "0"])?;
    run(repo, &["config", "maintenance.auto", "false"])?;
    Ok(())
}

pub fn add_worktree(
    repo: &Path,
    worktree: &Path,
    branch: &str,
    base: &str,
) -> Result<String, GitError> {
    if worktree.join(".git").exists() {
        let checkout = current_branch(worktree)?;
        if checkout == branch {
            return rev_parse(worktree, "HEAD");
        }
    }
    if worktree.exists() {
        let _ = Command::new(real_git())
            .arg("-C")
            .arg(repo)
            .args(["worktree", "remove", "--force"])
            .arg(worktree)
            .output();
        let _ = Command::new(real_git())
            .arg("-C")
            .arg(repo)
            .args(["worktree", "prune"])
            .output();
    }
    if let Some(parent) = worktree.parent() {
        fs::create_dir_all(parent)?;
    }
    let branch_exists = run_output(
        repo,
        &[
            "show-ref",
            "--verify",
            "--quiet",
            &format!("refs/heads/{branch}"),
        ],
    )
    .map(|o| o.status.success())
    .unwrap_or(false);
    let args: Vec<String> = if branch_exists {
        vec![
            "worktree".to_string(),
            "add".to_string(),
            worktree.to_string_lossy().to_string(),
            branch.to_string(),
        ]
    } else {
        vec![
            "worktree".to_string(),
            "add".to_string(),
            "-b".to_string(),
            branch.to_string(),
            worktree.to_string_lossy().to_string(),
            base.to_string(),
        ]
    };
    let output = Command::new(real_git())
        .arg("-C")
        .arg(repo)
        .args(args.iter().map(|s| s.as_str()))
        .env("GIT_TERMINAL_PROMPT", "0")
        .env("GIT_CONFIG_NOSYSTEM", "1")
        .env("LC_ALL", "C")
        .output()?;
    if !output.status.success() {
        return Err(GitError {
            message: format!(
                "git worktree add failed: {}",
                String::from_utf8_lossy(&output.stderr).trim()
            ),
        });
    }
    rev_parse(worktree, "HEAD")
}

pub fn remove_worktree(repo: &Path, worktree: &Path) {
    let _ = Command::new(real_git())
        .arg("-C")
        .arg(repo)
        .args(["worktree", "remove", "--force"])
        .arg(worktree)
        .output();
    let _ = Command::new(real_git())
        .arg("-C")
        .arg(repo)
        .args(["worktree", "prune"])
        .output();
}

pub fn diff_names(repo: &Path, base: &str, candidate: &str) -> Result<Vec<String>, GitError> {
    let output = run_output(
        repo,
        &[
            "diff-tree",
            "-r",
            "--name-only",
            "--no-commit-id",
            base,
            candidate,
        ],
    )?;
    if !output.status.success() {
        return Err(GitError {
            message: String::from_utf8_lossy(&output.stderr).trim().to_string(),
        });
    }
    Ok(String::from_utf8_lossy(&output.stdout)
        .lines()
        .map(str::trim)
        .filter(|line| !line.is_empty())
        .map(ToString::to_string)
        .collect())
}

pub fn staged_names(repo: &Path) -> Result<Vec<String>, GitError> {
    let output = run_output(repo, &["diff", "--cached", "--name-only"])?;
    if !output.status.success() {
        return Err(GitError {
            message: String::from_utf8_lossy(&output.stderr).trim().to_string(),
        });
    }
    Ok(String::from_utf8_lossy(&output.stdout)
        .lines()
        .map(str::trim)
        .filter(|line| !line.is_empty())
        .map(ToString::to_string)
        .collect())
}

/// Reads many blobs across several trees using a single `git cat-file --batch` process.
/// Returns a map keyed by `(tree, path)`.
pub fn read_blobs_multi(
    repo: &Path,
    specs: &[(String, String)],
) -> HashMap<(String, String), Vec<u8>> {
    let mut result = HashMap::new();
    if specs.is_empty() {
        return result;
    }
    let mut cmd = Command::new(real_git());
    cmd.arg("-C")
        .arg(repo)
        .args(["cat-file", "--batch"])
        .env("GIT_TERMINAL_PROMPT", "0")
        .env("GIT_CONFIG_NOSYSTEM", "1")
        .env("LC_ALL", "C")
        .env_remove("GIT_DIR")
        .env_remove("GIT_WORK_TREE")
        .env_remove("GIT_INDEX_FILE")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::null());
    let Ok(mut child) = cmd.spawn() else {
        return result;
    };
    let mut request = String::new();
    for (tree, path) in specs {
        request.push_str(tree);
        request.push(':');
        request.push_str(path);
        request.push('\n');
    }
    if let Some(mut stdin) = child.stdin.take() {
        let _ = stdin.write_all(request.as_bytes());
    }
    let Ok(output) = child.wait_with_output() else {
        return result;
    };
    // `cat-file --batch` echoes the resolved object id rather than the requested spec,
    // so responses are matched back to requests positionally.
    let mut cursor = 0usize;
    let bytes = output.stdout;
    let mut index = 0usize;
    while cursor < bytes.len() && index < specs.len() {
        let header_end = match bytes[cursor..].iter().position(|b| *b == b'\n') {
            Some(offset) => cursor + offset,
            None => break,
        };
        let header = String::from_utf8_lossy(&bytes[cursor..header_end]).to_string();
        cursor = header_end + 1;
        let mut parts = header.split(' ');
        let _oid = parts.next().unwrap_or_default();
        let kind = parts.next().unwrap_or_default();
        let size: usize = parts.next().and_then(|v| v.parse().ok()).unwrap_or(0);
        let key = specs[index].clone();
        index += 1;
        if kind != "blob" {
            continue;
        }
        if cursor + size > bytes.len() {
            break;
        }
        result.insert(key, bytes[cursor..cursor + size].to_vec());
        cursor += size + 1;
    }
    result
}

pub fn git_path(repo: &Path, name: &str) -> Result<PathBuf, GitError> {
    let value = run(
        repo,
        &["rev-parse", "--path-format=absolute", "--git-path", name],
    )?;
    Ok(PathBuf::from(value))
}

pub fn stage_path(repo: &Path, path: &str) -> Result<(), GitError> {
    run(repo, &["add", "--", path])?;
    Ok(())
}

pub fn rebase(repo: &Path, onto: &str) -> Result<(), GitError> {
    run(repo, &["rebase", onto])?;
    Ok(())
}

pub fn fetch(repo: &Path) -> Result<(), GitError> {
    let output = run_output(repo, &["fetch", "--prune", "origin"])?;
    if !output.status.success() {
        return Err(GitError {
            message: String::from_utf8_lossy(&output.stderr).trim().to_string(),
        });
    }
    Ok(())
}

pub fn has_remote(repo: &Path) -> bool {
    run(repo, &["remote"])
        .map(|s| !s.is_empty())
        .unwrap_or(false)
}

pub fn is_commit(args: &[String]) -> bool {
    args.first().map(|s| s == "commit").unwrap_or(false)
}

pub fn is_push(args: &[String]) -> bool {
    args.first().map(|s| s == "push").unwrap_or(false)
}

pub fn _unused_osstr(value: &OsStr) -> String {
    value.to_string_lossy().to_string()
}
