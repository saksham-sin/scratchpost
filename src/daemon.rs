use crate::audit;
use crate::git_engine;
use crate::ledger::{read_snapshot, LedgerStore, Wal, WalEvent};
use crate::prompt;
use crate::protocol::{
    now_ms, BarrierState, ClaimState, ClaimStrength, CommitDecision, CommitPermit, Config,
    ConflictComponent, Endpoint, IntentRecord, IntentStatus, LedgerSnapshot, Mode, Notice,
    PathClaim, RpcRequest, RpcResponse, SequenceClaim, SessionRecord, SessionStatus, Severity,
};
use crate::rules::{self, Observation, RuleEngine};
use crate::session;
use crate::workspace;
use serde_json::{json, Value};
use std::collections::{BTreeMap, HashMap, HashSet, VecDeque};
use std::fs::{self, File, OpenOptions};
use std::io::{BufRead, BufReader, Write};
use std::os::unix::fs::PermissionsExt;
use std::os::unix::io::AsRawFd;
use std::os::unix::net::{UnixListener, UnixStream};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

const MAX_NOTICES: usize = 512;
const PERMIT_TTL_MS: u64 = 60_000;

/// Result of evaluating a candidate index tree against the session base.
struct Candidate {
    new_observations: Vec<Observation>,
    base_observations: Vec<Observation>,
    changed: Vec<String>,
    head: String,
    base_oid: String,
    current_base: String,
}

/// Per-agent staged paths: agent -> [(repo, [path, ...]), ...].
type StagedPaths = HashMap<String, Vec<(String, Vec<String>)>>;

pub struct Daemon {
    master: PathBuf,
    config: Config,
    sessions: BTreeMap<String, SessionRecord>,
    claims: Vec<SequenceClaim>,
    intents: Vec<IntentRecord>,
    notices: VecDeque<Notice>,
    barriers: BTreeMap<String, BarrierState>,
    barrier_completed: BTreeMap<String, Vec<String>>,
    next_claim_id: u64,
    next_intent_id: u64,
    next_notice_seq: u64,
    started_ms: u64,
    token: String,
    ledger: LedgerStore,
    wal: Wal,
    rules: RuleEngine,
    conflict_counter: u64,
    state_version: u64,
    components_cache: Option<(u64, Vec<ConflictComponent>)>,
    staged_cache: Option<(u64, StagedPaths)>,
    base_branch_cache: HashMap<String, String>,
    /// Staged paths reported by each agent's shim, refreshed on every commit attempt.
    /// Populated before the barrier check so conflict components exist pre-permit.
    pending_paths: StagedPaths,
    /// Advisory/mandatory path reservations announced before staging.
    path_claims: Vec<PathClaim>,
    next_path_claim_id: u64,
}

fn staged_contention(
    left: Option<&Vec<(String, Vec<String>)>>,
    right: Option<&Vec<(String, Vec<String>)>>,
) -> bool {
    let (Some(left), Some(right)) = (left, right) else {
        return false;
    };
    left.iter().any(|(repo, paths)| {
        right.iter().any(|(other_repo, other_paths)| {
            other_repo == repo && other_paths.iter().any(|p| paths.contains(p))
        })
    })
}

impl Daemon {
    fn new(master: &Path, token: String) -> Result<Self, String> {
        let config_path = workspace::config_path(master);
        let config = if config_path.exists() {
            workspace::load_config(master)
        } else {
            let config = Config::default();
            workspace::save_config(master, &config).map_err(|err| err.to_string())?;
            config
        };
        let mut daemon = Self {
            master: master.to_path_buf(),
            config,
            sessions: BTreeMap::new(),
            claims: Vec::new(),
            intents: Vec::new(),
            notices: VecDeque::new(),
            barriers: BTreeMap::new(),
            barrier_completed: BTreeMap::new(),
            next_claim_id: 1,
            next_intent_id: 1,
            next_notice_seq: 1,
            started_ms: now_ms(),
            token,
            ledger: LedgerStore::create(&workspace::mmap_path(master))
                .map_err(|err| format!("cannot create ledger snapshot: {err}"))?,
            wal: Wal::open(&workspace::wal_path(master))
                .map_err(|err| format!("cannot open WAL: {err}"))?,
            rules: RuleEngine::default(),
            conflict_counter: 0,
            state_version: 0,
            components_cache: None,
            staged_cache: None,
            base_branch_cache: HashMap::new(),
            pending_paths: HashMap::new(),
            path_claims: Vec::new(),
            next_path_claim_id: 1,
        };
        daemon.replay_wal();
        daemon.publish();
        Ok(daemon)
    }

    fn replay_wal(&mut self) {
        for event in crate::ledger::replay(&workspace::wal_path(&self.master)) {
            match event {
                WalEvent::Config { config } => self.config = config,
                WalEvent::Session { session } => {
                    self.sessions.insert(session.agent.clone(), session);
                }
                WalEvent::Claim { claim } => {
                    if let Some(existing) = self
                        .claims
                        .iter_mut()
                        .find(|existing| existing.claim_id == claim.claim_id)
                    {
                        *existing = claim;
                    } else {
                        self.claims.push(claim);
                    }
                }
                WalEvent::Intent { intent } => {
                    if let Some(existing) = self
                        .intents
                        .iter_mut()
                        .find(|existing| existing.intent_id == intent.intent_id)
                    {
                        *existing = intent;
                    } else {
                        self.intents.push(intent);
                    }
                }
                WalEvent::Notice { notice } => {
                    self.notices.push_back(notice);
                    while self.notices.len() > MAX_NOTICES {
                        self.notices.pop_front();
                    }
                }
                WalEvent::Barrier { barrier } => {
                    self.barriers.insert(barrier.component.clone(), barrier);
                }
                WalEvent::PathClaim { claim } => {
                    if let Some(existing) = self
                        .path_claims
                        .iter_mut()
                        .find(|existing| existing.claim_id == claim.claim_id)
                    {
                        *existing = claim;
                    } else {
                        self.path_claims.push(claim);
                    }
                }
            }
        }
        self.next_claim_id = self
            .claims
            .iter()
            .map(|claim| claim.claim_id)
            .max()
            .unwrap_or(0)
            + 1;
        self.next_intent_id = self
            .intents
            .iter()
            .map(|intent| intent.intent_id)
            .max()
            .unwrap_or(0)
            + 1;
        self.next_notice_seq = self
            .notices
            .iter()
            .map(|notice| notice.seq)
            .max()
            .unwrap_or(0)
            + 1;
        self.next_path_claim_id = self
            .path_claims
            .iter()
            .map(|claim| claim.claim_id)
            .max()
            .unwrap_or(0)
            + 1;
    }

    fn log(&self, event: &str, detail: &str) {
        eprintln!(
            "ts={} level=info event={} master={} {}",
            now_ms(),
            event,
            self.master.display(),
            detail
        );
    }

    fn snapshot(&mut self) -> LedgerSnapshot {
        let now = now_ms();
        let timeout = self.config.lease_timeout_ms;
        let mut sessions: Vec<SessionRecord> = self
            .sessions
            .values()
            .map(|session| {
                let mut session = session.clone();
                let elapsed = now.saturating_sub(session.last_heartbeat_ms);
                session.lease_remaining_ms = timeout.saturating_sub(elapsed);
                session.has_unpreserved_work =
                    session::has_unpreserved_work(&self.master, &session);
                session
            })
            .collect();
        sessions.sort_by(|a, b| a.agent.cmp(&b.agent));
        let mut claims = self.claims.clone();
        claims.sort_by_key(|claim| claim.claim_id);
        let mut intents = self.intents.clone();
        intents.sort_by_key(|intent| intent.intent_id);
        LedgerSnapshot {
            daemon_pid: std::process::id(),
            started_ms: self.started_ms,
            master_root: self.master.to_string_lossy().to_string(),
            config: self.config.clone(),
            sessions,
            claims,
            intents,
            notices: self.notices.iter().cloned().collect(),
            barrier: self.primary_barrier(),
            barriers: self.barriers.values().cloned().collect(),
            components: self.components(),
            path_claims: self.live_path_claims(),
        }
    }

    fn publish(&mut self) {
        let snapshot = self.snapshot();
        if let Err(err) = self.ledger.publish(&snapshot) {
            eprintln!(
                "ts={} level=error event=ledger.publish error={}",
                now_ms(),
                err
            );
        }
        self.write_bulletin(&snapshot);
        if let Err(err) = audit::write_report(&self.master, &snapshot) {
            eprintln!(
                "ts={} level=error event=audit.report error={}",
                now_ms(),
                err
            );
        }
    }

    fn write_bulletin(&self, snapshot: &LedgerSnapshot) {
        let mut text = String::new();
        text.push_str("# ScratchPost Bulletin\n\n");
        text.push_str(&format!(
            "Generated: {}  \nMode: {}  \nSessions: {}  \n\n",
            now_ms(),
            snapshot.config.mode,
            snapshot.sessions.len()
        ));
        text.push_str("## Active Sessions\n\n");
        text.push_str("| Agent | Branch | Port base | Workspace |\n|---|---|---:|---|\n");
        for session in &snapshot.sessions {
            let branch = session
                .repos
                .first()
                .map(|repo| repo.branch.as_str())
                .unwrap_or("-");
            text.push_str(&format!(
                "| {} | {} | {} | {} |\n",
                session.agent, branch, session.port_base, session.workspace
            ));
        }
        text.push_str("\n## Sequence Claims\n\n");
        text.push_str("| Agent | Rule | Namespace | Key | State |\n|---|---|---|---|---|\n");
        for claim in &snapshot.claims {
            text.push_str(&format!(
                "| {} | {} | {} | {} | {:?} |\n",
                claim.agent, claim.rule, claim.namespace, claim.key, claim.state
            ));
        }
        text.push_str("\n## Recent Mutations\n\n");
        for notice in snapshot.notices.iter().rev().take(20) {
            text.push_str(&format!("- {}: {}\n", notice.created_ms, notice.message));
        }
        let _ = workspace::atomic_write(&workspace::bulletin_path(&self.master), text.as_bytes());
    }

    fn persist(&mut self, event: WalEvent) {
        self.state_version += 1;
        if let Err(err) = self.wal.append_durable(&event) {
            eprintln!("ts={} level=error event=wal.append error={}", now_ms(), err);
        }
    }

    fn add_notice(
        &mut self,
        severity: Severity,
        kind: &str,
        agent: Option<String>,
        message: String,
        persist: bool,
    ) {
        let notice = Notice {
            seq: self.next_notice_seq,
            severity,
            kind: kind.to_string(),
            agent,
            message,
            created_ms: now_ms(),
        };
        self.next_notice_seq += 1;
        self.notices.push_back(notice.clone());
        while self.notices.len() > MAX_NOTICES {
            self.notices.pop_front();
        }
        // Every coordination action funnels through here, so this is the single
        // place the audit ledger needs to observe. A failed audit write must not
        // fail the action it describes.
        if let Err(err) = audit::record(&self.master, &notice) {
            eprintln!(
                "ts={} level=error event=audit.record error={}",
                now_ms(),
                err
            );
        }
        if persist {
            self.persist(WalEvent::Notice { notice });
        }
    }

    fn new_conflict_id(&mut self) -> String {
        self.conflict_counter += 1;
        let value =
            now_ms().rotate_left(17) ^ ((std::process::id() as u64) << 32) ^ self.conflict_counter;
        format!("{value:016x}")
    }

    fn active_sessions(&self) -> Vec<&SessionRecord> {
        self.sessions
            .values()
            .filter(|session| session.status == SessionStatus::Active)
            .collect()
    }

    fn prune_leases(&mut self) -> bool {
        let now = now_ms();
        let timeout = self.config.lease_timeout_ms;
        let mut changed = false;
        let mut expired = Vec::new();
        for session in self.sessions.values_mut() {
            if session.status == SessionStatus::Active
                && now.saturating_sub(session.last_heartbeat_ms) > timeout
            {
                session.status = SessionStatus::Abandoned;
                expired.push(session.agent.clone());
                changed = true;
            }
        }
        let now = now_ms();
        let mut claim_events = Vec::new();
        for claim in self.claims.iter_mut() {
            if claim.state == ClaimState::Reserved && claim.expires_ms <= now {
                claim.state = ClaimState::Released;
                claim_events.push(claim.clone());
                changed = true;
            }
        }
        for claim in claim_events {
            self.persist(WalEvent::Claim { claim });
        }
        let mut intent_events = Vec::new();
        for intent in self.intents.iter_mut() {
            if intent.status == IntentStatus::Prepared
                && now.saturating_sub(intent.created_ms) > PERMIT_TTL_MS
            {
                intent.status = IntentStatus::Abandoned;
                intent_events.push(intent.clone());
                changed = true;
            }
        }
        for intent in intent_events {
            self.persist(WalEvent::Intent { intent });
        }
        let mut path_claim_events = Vec::new();
        for claim in self.path_claims.iter_mut() {
            if claim.expires_ms <= now {
                path_claim_events.push(claim.clone());
                changed = true;
            }
        }
        if !path_claim_events.is_empty() {
            let expired_ids: HashSet<u64> = path_claim_events
                .iter()
                .map(|claim| claim.claim_id)
                .collect();
            self.path_claims
                .retain(|claim| !expired_ids.contains(&claim.claim_id));
            for claim in path_claim_events {
                self.persist(WalEvent::PathClaim { claim });
            }
        }
        for agent in expired {
            self.release_agent_claims(&agent);
            self.add_notice(
                Severity::Warning,
                "lease-expired",
                Some(agent.clone()),
                format!("agent {agent} lease expired; session marked abandoned"),
                true,
            );
        }
        changed
    }

    fn release_agent_claims(&mut self, agent: &str) {
        let released_paths: Vec<PathClaim> = self
            .path_claims
            .iter()
            .filter(|claim| claim.agent == agent)
            .cloned()
            .collect();
        if !released_paths.is_empty() {
            self.path_claims.retain(|claim| claim.agent != agent);
            for claim in released_paths {
                self.persist(WalEvent::PathClaim { claim });
            }
        }
        let mut claim_events = Vec::new();
        for claim in self.claims.iter_mut() {
            if claim.agent == agent && claim.state == ClaimState::Reserved {
                claim.state = ClaimState::Released;
                claim_events.push(claim.clone());
            }
        }
        for claim in claim_events {
            self.persist(WalEvent::Claim { claim });
        }
        let mut intent_events = Vec::new();
        for intent in self.intents.iter_mut() {
            if intent.agent == agent && intent.status == IntentStatus::Prepared {
                intent.status = IntentStatus::Abandoned;
                intent_events.push(intent.clone());
            }
        }
        for intent in intent_events {
            self.persist(WalEvent::Intent { intent });
        }
    }

    fn staged_index(&mut self) -> HashMap<String, Vec<(String, Vec<String>)>> {
        if let Some((version, cached)) = &self.staged_cache {
            if *version == self.state_version {
                return cached.clone();
            }
        }
        // Staged paths are reported by each agent's shim, so the daemon never has to
        // spawn git to learn what an agent has staged.
        let mut computed: HashMap<String, Vec<(String, Vec<String>)>> = HashMap::new();
        for intent in &self.intents {
            if intent.status != IntentStatus::Prepared || intent.paths.is_empty() {
                continue;
            }
            computed
                .entry(intent.agent.clone())
                .or_default()
                .push((intent.repo.clone(), intent.paths.clone()));
        }
        for (agent, entries) in &self.pending_paths {
            let slot = computed.entry(agent.clone()).or_default();
            for (repo, paths) in entries {
                if paths.is_empty() {
                    continue;
                }
                match slot.iter_mut().find(|(existing, _)| existing == repo) {
                    Some((_, existing)) => {
                        for path in paths {
                            if !existing.contains(path) {
                                existing.push(path.clone());
                            }
                        }
                    }
                    None => slot.push((repo.clone(), paths.clone())),
                }
            }
        }
        self.staged_cache = Some((self.state_version, computed.clone()));
        computed
    }

    fn component_groups(&mut self) -> Vec<(Vec<String>, String)> {
        let staged = self.staged_index();
        let active = self.active_sessions();
        if active.is_empty() {
            return Vec::new();
        }
        let mut parent: Vec<usize> = (0..active.len()).collect();
        let mut reasons: HashMap<usize, Vec<String>> = HashMap::new();
        fn find(parent: &mut [usize], x: usize) -> usize {
            if parent[x] != x {
                parent[x] = find(parent, parent[x]);
            }
            parent[x]
        }
        for i in 0..active.len() {
            for j in (i + 1)..active.len() {
                let a = active[i].agent.as_str();
                let b = active[j].agent.as_str();
                let shared_sequence = self.claims.iter().any(|left| {
                    left.agent == a
                        && left.state == ClaimState::Reserved
                        && self.claims.iter().any(|right| {
                            right.agent == b
                                && right.state == ClaimState::Reserved
                                && right.namespace == left.namespace
                        })
                });
                let shared_path = self.intents.iter().any(|left| {
                    left.agent == a
                        && left.status == IntentStatus::Prepared
                        && self.intents.iter().any(|right| {
                            right.agent == b
                                && right.status == IntentStatus::Prepared
                                && right.repo == left.repo
                                && right.paths.iter().any(|p| left.paths.contains(p))
                        })
                }) || staged_contention(staged.get(a), staged.get(b));
                if shared_sequence || shared_path {
                    let ri = find(&mut parent, i);
                    let rj = find(&mut parent, j);
                    if ri != rj {
                        parent[ri] = rj;
                    }
                    let reason = if shared_sequence {
                        "shared sequence namespace".to_string()
                    } else {
                        "overlapping touched paths".to_string()
                    };
                    for idx in [i, j] {
                        let root = find(&mut parent, idx);
                        reasons.entry(root).or_default().push(reason.clone());
                    }
                }
            }
        }
        let mut groups: BTreeMap<usize, Vec<String>> = BTreeMap::new();
        for (idx, session) in active.iter().enumerate() {
            let root = find(&mut parent, idx);
            groups.entry(root).or_default().push(session.agent.clone());
        }
        groups
            .into_iter()
            .map(|(root, mut agents)| {
                agents.sort();
                let reason = reasons
                    .get(&root)
                    .and_then(|v| v.first().cloned())
                    .unwrap_or_else(|| "independent".to_string());
                (agents, reason)
            })
            .collect()
    }

    fn component_members(&mut self, agent: &str) -> Vec<String> {
        for (agents, _) in self.component_groups() {
            if agents.iter().any(|member| member == agent) {
                return agents;
            }
        }
        vec![agent.to_string()]
    }

    fn component_key(&mut self, agent: &str) -> String {
        self.component_members(agent).join("+")
    }

    fn primary_barrier(&self) -> BarrierState {
        self.barriers
            .values()
            .find(|barrier| barrier.open)
            .cloned()
            .or_else(|| self.barriers.values().next().cloned())
            .unwrap_or_default()
    }

    fn components(&mut self) -> Vec<ConflictComponent> {
        if let Some((version, cached)) = &self.components_cache {
            if *version == self.state_version {
                return cached.clone();
            }
        }
        let computed = self.compute_components();
        self.components_cache = Some((self.state_version, computed.clone()));
        computed
    }

    fn compute_components(&mut self) -> Vec<ConflictComponent> {
        let mut result = Vec::new();
        for (agents, reason) in self.component_groups() {
            if agents.len() < 2 {
                continue;
            }
            result.push(ConflictComponent {
                component_id: result.len(),
                agents,
                reason,
            });
        }
        result
    }

    fn find_session(&self, agent: &str) -> Result<&SessionRecord, (String, String)> {
        self.sessions
            .get(agent)
            .filter(|session| session.status == SessionStatus::Active)
            .ok_or_else(|| {
                let hint = if self.sessions.contains_key(agent) {
                    "its lease expired; re-attach with `sp run --agent <name> -- <command>` to revive it"
                } else {
                    "attach first with `sp run --agent <name> -- <command>` (or `sp shell --agent <name>`)"
                };
                (
                    "SP_SESSION_NOT_ATTACHED".to_string(),
                    format!("no active session for agent {agent}: {hint}"),
                )
            })
    }

    fn find_session_repo<'a>(
        &self,
        session: &'a SessionRecord,
        repo: &str,
    ) -> Result<&'a crate::protocol::SessionRepo, (String, String)> {
        session
            .repos
            .iter()
            .find(|candidate| candidate.relative_path == repo || candidate.name == repo)
            .ok_or_else(|| {
                (
                    "SP_REPOSITORY_NOT_REGISTERED".to_string(),
                    format!("agent {} has no repository {repo}", session.agent),
                )
            })
    }

    fn ensure_session(&mut self, agent: &str) -> Result<SessionRecord, (String, String)> {
        let agent = workspace::sanitize_agent(agent)
            .map_err(|message| ("SP_SESSION_IDENTITY_CONFLICT".to_string(), message))?;
        if let Some(existing) = self.sessions.get_mut(&agent) {
            if existing.status == SessionStatus::Active && Path::new(&existing.workspace).is_dir() {
                existing.last_heartbeat_ms = now_ms();
                let session = existing.clone();
                let _ = session::save_session(&self.master, &session);
                self.publish();
                return Ok(session);
            }
            if existing.status == SessionStatus::Abandoned {
                let old = existing.clone();
                // A lease can expire while the agent is still mid-task. Re-attaching
                // must never silently discard that work, and must not mint a second
                // session under the same agent name: if the workspace is still on
                // disk, revive the existing session in place so the branch, port band
                // and session id stay stable across re-attaches.
                if Path::new(&old.workspace).is_dir() {
                    let unpreserved = session::has_unpreserved_work(&self.master, &old);
                    let mut revived = old.clone();
                    revived.status = SessionStatus::Active;
                    revived.last_heartbeat_ms = now_ms();
                    self.sessions.insert(agent.clone(), revived.clone());
                    self.persist(WalEvent::Session {
                        session: revived.clone(),
                    });
                    let detail = if unpreserved {
                        "holding unpreserved work"
                    } else {
                        "workspace intact"
                    };
                    self.add_notice(
                        Severity::Warning,
                        "session-revived",
                        Some(agent.clone()),
                        format!(
                            "agent {agent} re-attached to expired session {detail}; workspace preserved at {}",
                            revived.workspace
                        ),
                        true,
                    );
                    self.publish();
                    return Ok(revived);
                }
                session::remove_session_worktrees(&self.master, &old);
                self.sessions.remove(&agent);
            }
        }
        let used: HashSet<u16> = self
            .sessions
            .values()
            .filter(|session| session.status == SessionStatus::Active)
            .map(|session| session.slot)
            .collect();
        let slot = (1..=self.config.max_sessions)
            .find(|slot| !used.contains(slot))
            .ok_or_else(|| {
                (
                    "SP_SESSION_LIMIT".to_string(),
                    format!(
                        "maximum active sessions ({}) reached",
                        self.config.max_sessions
                    ),
                )
            })?;
        let port_base = self
            .config
            .port_base_start
            .saturating_add((slot - 1).saturating_mul(self.config.port_band_width));
        let branch_id = format!("{:x}", now_ms() ^ (std::process::id() as u64));
        let session_id = format!("{agent}-{branch_id}");
        self.log("session.provision.begin", &format!("agent={agent}"));
        let provisioned =
            session::provision(&self.master, &agent, slot, port_base, session_id, branch_id)
                .map_err(|message| ("SP_SESSION_PROVISION_FAILED".to_string(), message))?;
        let record = provisioned.session;
        if !provisioned.pinned_containers.is_empty() {
            self.add_notice(
                Severity::Warning,
                "compose-isolation-degraded",
                Some(agent.clone()),
                format!(
                    "agent {agent}: COMPOSE_PROJECT_NAME cannot isolate containers pinned by name ({}); two sessions will share these containers and their databases",
                    provisioned.pinned_containers.join("; ")
                ),
                true,
            );
        }
        self.sessions.insert(agent.clone(), record.clone());
        self.persist(WalEvent::Session {
            session: record.clone(),
        });
        self.add_notice(
            Severity::Info,
            "session-active",
            Some(agent.clone()),
            format!(
                "agent {} attached in {} with port base {}",
                agent, record.workspace, record.port_base
            ),
            true,
        );
        self.publish();
        self.log(
            "session.provision.complete",
            &format!("agent={agent} slot={slot} port_base={port_base}"),
        );
        Ok(record)
    }

    /// Resolves a repository to its canonical location in the master workspace.
    ///
    /// Used for base-branch resolution and object reads that must observe the
    /// authoritative repository, not an agent's worktree.
    fn repo_path(&self, relative_path: &str) -> PathBuf {
        if relative_path == "." {
            self.master.clone()
        } else {
            self.master.join(relative_path)
        }
    }

    /// Resolves a repository to the agent's own worktree.
    ///
    /// Candidate inspection must read the agent's tree: a root-level repository
    /// (`relative_path == "."`) would otherwise resolve to the master checkout
    /// and every agent would be scanned against the same, wrong tree.
    fn session_repo_path(&self, session: &crate::protocol::SessionRecord, repo: &str) -> PathBuf {
        session
            .repos
            .iter()
            .find(|candidate| candidate.relative_path == repo || candidate.name == repo)
            .map(|candidate| PathBuf::from(&candidate.worktree))
            .unwrap_or_else(|| self.repo_path(repo))
    }

    fn check_candidate(
        &mut self,
        agent: &str,
        repo: &str,
        index_tree: &str,
        reported_head: &str,
        reported_paths: &[String],
    ) -> Result<Candidate, (String, String)> {
        let session = self.find_session(agent)?.clone();
        let repo_record = self.find_session_repo(&session, repo)?.clone();
        let repo_path = self.session_repo_path(&session, repo);
        let base_branch = match self.base_branch_cache.get(&repo_record.relative_path) {
            Some(branch) => branch.clone(),
            None => {
                let branch = git_engine::resolve_base_branch(&repo_path);
                self.base_branch_cache
                    .insert(repo_record.relative_path.clone(), branch.clone());
                branch
            }
        };
        let current_base = match git_engine::ref_oid_fast(&repo_path, &base_branch) {
            Some(oid) => oid,
            None => git_engine::rev_parse(&repo_path, &base_branch).map_err(|err| {
                (
                    "SP_REPOSITORY_UNAVAILABLE".to_string(),
                    format!("cannot resolve current base branch: {err}"),
                )
            })?,
        };
        let head = String::new();
        let head = if reported_head.is_empty() {
            head
        } else {
            reported_head.to_string()
        };
        let diff_base = if head.is_empty() {
            repo_record.base_oid.clone()
        } else {
            head.clone()
        };
        let changed = if reported_paths.is_empty() {
            git_engine::diff_names(&repo_path, &diff_base, index_tree).map_err(|err| {
                (
                    "SP_REPOSITORY_UNAVAILABLE".to_string(),
                    format!("cannot inspect staged tree: {err}"),
                )
            })?
        } else {
            reported_paths.to_vec()
        };
        let mut scanned = self.rules.scan_trees(
            &repo_path,
            &repo_record.name,
            &[(index_tree, &changed), (&diff_base, &changed)],
        );
        let base = scanned.pop().unwrap_or_default();
        let candidate = scanned.pop().unwrap_or_default();
        let new_observations = rules::newly_introduced(&candidate, &base);
        Ok(Candidate {
            new_observations,
            base_observations: base,
            changed,
            head,
            base_oid: repo_record.base_oid,
            current_base,
        })
    }

    fn abandon_prepared(&mut self, agent: &str, repo: &str) {
        let mut permits = Vec::new();
        let mut intent_events = Vec::new();
        for intent in self.intents.iter_mut() {
            if intent.agent == agent
                && intent.repo == repo
                && intent.status == IntentStatus::Prepared
            {
                intent.status = IntentStatus::Abandoned;
                if let Some(permit) = &intent.permit_id {
                    permits.push(permit.clone());
                }
                intent_events.push(intent.clone());
            }
        }
        let mut claim_events = Vec::new();
        for claim in self.claims.iter_mut() {
            if claim.agent == agent
                && claim.state == ClaimState::Reserved
                && permits.contains(&claim.permit_id)
            {
                claim.state = ClaimState::Released;
                claim_events.push(claim.clone());
            }
        }
        for intent in intent_events {
            self.persist(WalEvent::Intent { intent });
        }
        for claim in claim_events {
            self.persist(WalEvent::Claim { claim });
        }
    }

    fn abort_commit(&mut self, agent: &str, params: &Value) -> Result<Value, (String, String)> {
        let permit_id = params
            .get("permit_id")
            .and_then(Value::as_str)
            .ok_or_else(|| {
                (
                    "SP_BAD_REQUEST".to_string(),
                    "missing permit_id".to_string(),
                )
            })?;
        let intent_index = self.intents.iter().position(|intent| {
            intent.permit_id.as_deref() == Some(permit_id)
                && intent.agent == agent
                && intent.status == IntentStatus::Prepared
        });
        let Some(index) = intent_index else {
            return Ok(json!({"aborted": false, "reason": "no prepared permit"}));
        };
        self.intents[index].status = IntentStatus::Abandoned;
        let intent = self.intents[index].clone();
        self.persist(WalEvent::Intent { intent });
        let mut claim_events = Vec::new();
        for claim in self.claims.iter_mut() {
            if claim.permit_id == permit_id && claim.state == ClaimState::Reserved {
                claim.state = ClaimState::Released;
                claim_events.push(claim.clone());
            }
        }
        for claim in claim_events {
            self.persist(WalEvent::Claim { claim });
        }
        self.add_notice(
            Severity::Warning,
            "commit-aborted",
            Some(agent.to_string()),
            format!(
                "commit permit {} aborted; sequence claims released",
                permit_id
            ),
            true,
        );
        self.publish();
        Ok(json!({"aborted": true, "permit_id": permit_id}))
    }

    fn stale_base_decision(
        &mut self,
        agent: &str,
        repo: &str,
        branch: &str,
        candidate: &Candidate,
    ) -> Result<Option<Value>, (String, String)> {
        if candidate.base_oid.is_empty() || candidate.current_base == candidate.base_oid {
            return Ok(None);
        }
        let conflict_id = self.new_conflict_id();
        let report = prompt::stale_base_conflict(
            conflict_id,
            agent,
            repo,
            branch,
            &candidate.base_oid,
            &candidate.current_base,
            Vec::new(),
        );
        self.add_notice(
            Severity::Blocking,
            "commit-blocked-stale-base",
            Some(agent.to_string()),
            format!("base advanced for {repo}; session must synchronize before committing"),
            true,
        );
        let decision = CommitDecision {
            allowed: false,
            permit: None,
            conflict: Some(report),
        };
        serde_json::to_value(decision)
            .map(Some)
            .map_err(|err| ("SP_INTERNAL".to_string(), err.to_string()))
    }

    fn prepare_commit(&mut self, agent: &str, params: &Value) -> Result<Value, (String, String)> {
        let repo = params
            .get("repo")
            .and_then(Value::as_str)
            .ok_or_else(|| ("SP_BAD_REQUEST".to_string(), "missing repo".to_string()))?;
        let branch = params
            .get("branch")
            .and_then(Value::as_str)
            .unwrap_or("HEAD");
        let index_tree = params
            .get("index_tree")
            .and_then(Value::as_str)
            .ok_or_else(|| {
                (
                    "SP_BAD_REQUEST".to_string(),
                    "missing index_tree".to_string(),
                )
            })?;
        let reported_head = params
            .get("head_oid")
            .and_then(Value::as_str)
            .unwrap_or_default()
            .to_string();
        let reported_paths: Vec<String> = params
            .get("staged_paths")
            .and_then(Value::as_array)
            .map(|values| {
                values
                    .iter()
                    .filter_map(Value::as_str)
                    .map(ToString::to_string)
                    .collect()
            })
            .unwrap_or_default();
        self.abandon_prepared(agent, repo);
        let candidate =
            self.check_candidate(agent, repo, index_tree, &reported_head, &reported_paths)?;
        self.pending_paths.insert(
            agent.to_string(),
            vec![(repo.to_string(), candidate.changed.clone())],
        );
        self.state_version += 1;
        if let Some(decision) = self.stale_base_decision(agent, repo, branch, &candidate)? {
            return Ok(decision);
        }
        if self.config.mode != Mode::Fcfs {
            let global = self.config.barrier_scope == crate::protocol::BarrierScope::Workspace;
            let barrier = self.arrive_barrier(agent, global)?;
            if !barrier.released {
                let conflict_id = self.new_conflict_id();
                let conflict = prompt::barrier_wait(conflict_id, agent, &barrier);
                return serde_json::to_value(CommitDecision {
                    allowed: false,
                    permit: None,
                    conflict: Some(conflict),
                })
                .map_err(|err| ("SP_INTERNAL".to_string(), err.to_string()));
            }
        }
        let Candidate {
            new_observations,
            base_observations,
            changed,
            head,
            base_oid,
            ..
        } = candidate;
        let all_observations = base_observations
            .iter()
            .cloned()
            .chain(new_observations.iter().cloned())
            .collect::<Vec<_>>();
        if let Some((claim, observation)) =
            rules::find_collision(&new_observations, &self.claims, agent)
                .map(|(claim, observation)| (claim.clone(), observation))
        {
            let lowest = rules::lowest_free(
                &all_observations,
                &self.claims,
                &observation.namespace,
                &observation,
            );
            let conflict_id = self.new_conflict_id();
            let conflict = prompt::sequence_conflict(
                &prompt::ConflictSubject {
                    conflict_id,
                    agent,
                    repo,
                    branch,
                    base_oid: &base_oid,
                    head_oid: &head,
                },
                &observation,
                &claim,
                &lowest,
            );
            self.add_notice(
                Severity::Blocking,
                "commit-blocked",
                Some(agent.to_string()),
                format!(
                    "{} is reserved by {} for {}",
                    observation.key, claim.agent, observation.namespace
                ),
                true,
            );
            self.publish();
            let decision = CommitDecision {
                allowed: false,
                permit: None,
                conflict: Some(conflict),
            };
            return serde_json::to_value(decision)
                .map_err(|err| ("SP_INTERNAL".to_string(), err.to_string()));
        }
        let permit_id = workspace::random_token()[..32].to_string();
        let intent_id = self.next_intent_id;
        self.next_intent_id += 1;
        let expires = now_ms() + PERMIT_TTL_MS;
        let mut claim_ids = Vec::new();
        for observation in &new_observations {
            let claim_id = self.next_claim_id;
            self.next_claim_id += 1;
            let claim = SequenceClaim {
                claim_id,
                permit_id: permit_id.clone(),
                agent: agent.to_string(),
                session_id: self
                    .sessions
                    .get(agent)
                    .map(|s| s.session_id.clone())
                    .unwrap_or_default(),
                repo: repo.to_string(),
                rule: observation.rule.clone(),
                namespace: observation.namespace.clone(),
                key: observation.key.clone(),
                numeric: observation.numeric,
                path: observation.path.clone(),
                line: observation.line,
                state: ClaimState::Reserved,
                commit_oid: None,
                created_ms: now_ms(),
                expires_ms: expires,
            };
            self.claims.push(claim.clone());
            self.persist(WalEvent::Claim { claim });
            claim_ids.push(claim_id);
        }
        let intent = IntentRecord {
            intent_id,
            agent: agent.to_string(),
            repo: repo.to_string(),
            branch: branch.to_string(),
            paths: if reported_paths.is_empty() {
                changed
            } else {
                reported_paths
            },
            base_oid: base_oid.clone(),
            index_tree: index_tree.to_string(),
            status: IntentStatus::Prepared,
            permit_id: Some(permit_id.clone()),
            created_ms: now_ms(),
        };
        self.intents.push(intent.clone());
        self.persist(WalEvent::Intent { intent });
        self.add_notice(
            Severity::Info,
            "commit-permit",
            Some(agent.to_string()),
            format!(
                "commit permit {} issued for {} ({} sequence claim(s))",
                permit_id,
                repo,
                claim_ids.len()
            ),
            false,
        );
        self.publish();
        let permit = CommitPermit {
            permit_id,
            intent_id,
            agent: agent.to_string(),
            repo: repo.to_string(),
            branch: branch.to_string(),
            base_oid,
            index_tree: index_tree.to_string(),
            expires_ms: expires,
        };
        serde_json::to_value(CommitDecision {
            allowed: true,
            permit: Some(permit),
            conflict: None,
        })
        .map_err(|err| ("SP_INTERNAL".to_string(), err.to_string()))
    }

    fn validate_commit(&mut self, agent: &str, params: &Value) -> Result<Value, (String, String)> {
        let repo = params
            .get("repo")
            .and_then(Value::as_str)
            .ok_or_else(|| ("SP_BAD_REQUEST".to_string(), "missing repo".to_string()))?;
        let branch = params
            .get("branch")
            .and_then(Value::as_str)
            .unwrap_or("HEAD");
        let index_tree = params
            .get("index_tree")
            .and_then(Value::as_str)
            .ok_or_else(|| {
                (
                    "SP_BAD_REQUEST".to_string(),
                    "missing index_tree".to_string(),
                )
            })?;
        let reported_head = params
            .get("head_oid")
            .and_then(Value::as_str)
            .unwrap_or_default();
        let reported_paths: Vec<String> = params
            .get("staged_paths")
            .and_then(Value::as_array)
            .map(|values| {
                values
                    .iter()
                    .filter_map(Value::as_str)
                    .map(ToString::to_string)
                    .collect()
            })
            .unwrap_or_default();
        let candidate =
            self.check_candidate(agent, repo, index_tree, reported_head, &reported_paths)?;
        if let Some(decision) = self.stale_base_decision(agent, repo, branch, &candidate)? {
            return Ok(decision);
        }
        let Candidate {
            new_observations,
            base_observations,
            changed,
            head,
            base_oid,
            ..
        } = candidate;
        let all = base_observations
            .iter()
            .cloned()
            .chain(new_observations.iter().cloned())
            .collect::<Vec<_>>();
        if let Some((claim, observation)) =
            rules::find_collision(&new_observations, &self.claims, agent)
                .map(|(claim, observation)| (claim.clone(), observation))
        {
            let lowest =
                rules::lowest_free(&all, &self.claims, &observation.namespace, &observation);
            let conflict_id = self.new_conflict_id();
            let conflict = prompt::sequence_conflict(
                &prompt::ConflictSubject {
                    conflict_id,
                    agent,
                    repo,
                    branch,
                    base_oid: &base_oid,
                    head_oid: &head,
                },
                &observation,
                &claim,
                &lowest,
            );
            return serde_json::to_value(CommitDecision {
                allowed: false,
                permit: None,
                conflict: Some(conflict),
            })
            .map_err(|err| ("SP_INTERNAL".to_string(), err.to_string()));
        }
        serde_json::to_value(json!({
            "allowed": true,
            "new_observations": new_observations.len(),
            "changed_paths": changed
        }))
        .map_err(|err| ("SP_INTERNAL".to_string(), err.to_string()))
    }

    fn record_commit(&mut self, agent: &str, params: &Value) -> Result<Value, (String, String)> {
        let permit_id = params
            .get("permit_id")
            .and_then(Value::as_str)
            .ok_or_else(|| {
                (
                    "SP_BAD_REQUEST".to_string(),
                    "missing permit_id".to_string(),
                )
            })?;
        let commit_oid = params
            .get("commit_oid")
            .and_then(Value::as_str)
            .ok_or_else(|| {
                (
                    "SP_BAD_REQUEST".to_string(),
                    "missing commit_oid".to_string(),
                )
            })?;
        let tree_oid = params.get("tree_oid").and_then(Value::as_str).unwrap_or("");
        let intent_index = self
            .intents
            .iter()
            .position(|intent| {
                intent.permit_id.as_deref() == Some(permit_id) && intent.agent == agent
            })
            .ok_or_else(|| {
                (
                    "SP_COMMIT_PERMIT_INVALID".to_string(),
                    format!("no prepared intent for permit {permit_id}"),
                )
            })?;
        if self.intents[intent_index].status != IntentStatus::Prepared {
            return Err((
                "SP_COMMIT_PERMIT_INVALID".to_string(),
                "commit permit is no longer prepared".to_string(),
            ));
        }
        if !tree_oid.is_empty() && tree_oid != self.intents[intent_index].index_tree {
            self.intents[intent_index].status = IntentStatus::Blocked;
            let intent = self.intents[intent_index].clone();
            self.persist(WalEvent::Intent { intent });
            return Err((
                "SP_COMMIT_POSTCONDITION_FAILED".to_string(),
                "committed tree does not match the permit-bound index tree".to_string(),
            ));
        }
        self.intents[intent_index].status = IntentStatus::Committed;
        let intent = self.intents[intent_index].clone();
        self.persist(WalEvent::Intent { intent });
        let repo = self.intents[intent_index].repo.clone();
        let mut claim_events = Vec::new();
        for claim in self.claims.iter_mut() {
            if claim.permit_id == permit_id && claim.agent == agent {
                claim.state = ClaimState::Committed;
                claim.commit_oid = Some(commit_oid.to_string());
                claim_events.push(claim.clone());
            }
        }
        for claim in claim_events {
            self.persist(WalEvent::Claim { claim });
        }
        if self.config.mode != Mode::Fcfs {
            let key = self.component_key(agent);
            if let Some(barrier) = self.barriers.get(&key).cloned() {
                if barrier.released && barrier.participants.iter().any(|p| p == agent) {
                    let completed = self.barrier_completed.entry(key.clone()).or_default();
                    if !completed.iter().any(|p| p == agent) {
                        completed.push(agent.to_string());
                    }
                    if barrier
                        .participants
                        .iter()
                        .all(|participant| completed.iter().any(|p| p == participant))
                    {
                        let next = BarrierState {
                            component: key.clone(),
                            barrier_id: barrier.barrier_id + 1,
                            ..BarrierState::default()
                        };
                        self.barriers.insert(key.clone(), next.clone());
                        self.barrier_completed.remove(&key);
                        self.persist(WalEvent::Barrier { barrier: next });
                    }
                }
            }
        }
        self.add_notice(
            Severity::Info,
            "mutation-published",
            Some(agent.to_string()),
            format!(
                "agent {} committed {} ({}); peers must rebase before publication",
                agent, repo, commit_oid
            ),
            true,
        );
        self.publish();
        Ok(json!({"recorded": true, "commit_oid": commit_oid}))
    }

    fn live_path_claims(&self) -> Vec<PathClaim> {
        let now = now_ms();
        let mut claims: Vec<PathClaim> = self
            .path_claims
            .iter()
            .filter(|claim| claim.expires_ms > now)
            .cloned()
            .collect();
        claims.sort_by_key(|claim| claim.claim_id);
        claims
    }

    fn claim_path(&mut self, agent: &str, params: &Value) -> Result<Value, (String, String)> {
        self.find_session(agent)?;
        let repo = params
            .get("repo")
            .and_then(Value::as_str)
            .ok_or_else(|| ("SP_BAD_REQUEST".to_string(), "missing repo".to_string()))?;
        let path = params
            .get("path")
            .and_then(Value::as_str)
            .ok_or_else(|| ("SP_BAD_REQUEST".to_string(), "missing path".to_string()))?;
        let strength = match params
            .get("strength")
            .and_then(Value::as_str)
            .unwrap_or("advisory")
        {
            "advisory" => ClaimStrength::Advisory,
            "mandatory" => ClaimStrength::Mandatory,
            other => {
                return Err((
                    "SP_BAD_REQUEST".to_string(),
                    format!("unknown claim strength {other}; expected advisory or mandatory"),
                ))
            }
        };
        let ttl_ms = params
            .get("ttl_ms")
            .and_then(Value::as_u64)
            .unwrap_or(self.config.lease_timeout_ms)
            .clamp(1_000, 3_600_000);
        let note = params
            .get("note")
            .and_then(Value::as_str)
            .map(ToString::to_string);

        if let Some(existing) = self
            .path_claims
            .iter()
            .find(|claim| claim.agent == agent && claim.repo == repo && claim.path == path)
        {
            return Ok(json!({"claim": existing, "created": false}));
        }

        let claim = PathClaim {
            claim_id: self.next_path_claim_id,
            agent: agent.to_string(),
            repo: repo.to_string(),
            path: path.to_string(),
            strength,
            note,
            created_ms: now_ms(),
            expires_ms: now_ms() + ttl_ms,
        };
        self.next_path_claim_id += 1;
        self.path_claims.push(claim.clone());
        self.persist(WalEvent::PathClaim {
            claim: claim.clone(),
        });
        self.add_notice(
            Severity::Info,
            "path-claimed",
            Some(agent.to_string()),
            format!(
                "agent {agent} claimed {repo}:{path} ({})",
                match claim.strength {
                    ClaimStrength::Advisory => "advisory",
                    ClaimStrength::Mandatory => "mandatory",
                }
            ),
            false,
        );
        self.state_version += 1;
        self.publish();
        Ok(json!({"claim": claim, "created": true}))
    }

    fn release_path_claim(
        &mut self,
        agent: &str,
        params: &Value,
    ) -> Result<Value, (String, String)> {
        let repo = params.get("repo").and_then(Value::as_str);
        let path = params.get("path").and_then(Value::as_str);
        let before = self.path_claims.len();
        let released: Vec<PathClaim> = self
            .path_claims
            .iter()
            .filter(|claim| {
                claim.agent == agent
                    && repo.map(|value| claim.repo == value).unwrap_or(true)
                    && path.map(|value| claim.path == value).unwrap_or(true)
            })
            .cloned()
            .collect();
        self.path_claims.retain(|claim| {
            !(claim.agent == agent
                && repo.map(|value| claim.repo == value).unwrap_or(true)
                && path.map(|value| claim.path == value).unwrap_or(true))
        });
        for claim in &released {
            self.persist(WalEvent::PathClaim {
                claim: claim.clone(),
            });
        }
        if self.path_claims.len() != before {
            self.state_version += 1;
            self.publish();
        }
        Ok(json!({"released": released}))
    }

    fn set_mode(&mut self, params: &Value) -> Result<Value, (String, String)> {
        let mode = params
            .get("mode")
            .and_then(Value::as_str)
            .ok_or_else(|| ("SP_BAD_REQUEST".to_string(), "missing mode".to_string()))?;
        let mode: Mode = mode
            .parse()
            .map_err(|message| ("SP_BAD_REQUEST".to_string(), message))?;
        let previous = self.config.mode.clone();
        self.config.mode = mode.clone();
        if previous != mode && mode != Mode::Fcfs {
            self.barriers.clear();
            self.barrier_completed.clear();
        }
        if let Some(timeout) = params.get("barrier_timeout_ms").and_then(Value::as_u64) {
            self.config.barrier_timeout_ms = timeout;
        }
        if let Some(scope) = params.get("barrier_scope").and_then(Value::as_str) {
            self.config.barrier_scope = crate::protocol::BarrierScope::parse(scope)
                .map_err(|message| ("SP_BAD_REQUEST".to_string(), message))?;
        }
        workspace::save_config(&self.master, &self.config)
            .map_err(|err| ("SP_INTERNAL".to_string(), err.to_string()))?;
        self.persist(WalEvent::Config {
            config: self.config.clone(),
        });
        self.add_notice(
            Severity::Info,
            "mode-changed",
            None,
            format!("coordination mode set to {}", self.config.mode),
            true,
        );
        self.publish();
        Ok(json!(self.config))
    }

    fn arrive_barrier(
        &mut self,
        agent: &str,
        global: bool,
    ) -> Result<BarrierState, (String, String)> {
        self.find_session(agent)?;
        let key = if global {
            "workspace".to_string()
        } else {
            self.component_key(agent)
        };
        let participants = if global {
            self.active_sessions()
                .iter()
                .map(|session| session.agent.clone())
                .collect::<Vec<_>>()
        } else {
            self.component_members(agent)
        };
        if self.config.mode == Mode::Fcfs || participants.len() < 2 {
            return Ok(BarrierState {
                component: key,
                released: true,
                participants,
                ..BarrierState::default()
            });
        }
        let mut barrier = self
            .barriers
            .get(&key)
            .cloned()
            .unwrap_or_else(|| BarrierState {
                component: key.clone(),
                ..BarrierState::default()
            });
        if barrier.released && !barrier.open {
            return Ok(barrier);
        }
        if barrier.open && now_ms() >= barrier.deadline_ms {
            barrier.released = true;
            barrier.open = false;
            self.barriers.insert(key.clone(), barrier.clone());
            self.add_notice(
                Severity::Warning,
                "barrier-timeout",
                None,
                format!("barrier {} deadline expired; coordination degraded", key),
                true,
            );
        }
        if !barrier.open {
            barrier = BarrierState {
                component: key.clone(),
                barrier_id: barrier.barrier_id + 1,
                open: true,
                opened_ms: now_ms(),
                deadline_ms: now_ms() + self.config.barrier_timeout_ms,
                participants: participants.clone(),
                arrived: Vec::new(),
                released: false,
            };
            self.barrier_completed.remove(&key);
        }
        if !barrier.arrived.iter().any(|p| p == agent) {
            barrier.arrived.push(agent.to_string());
        }
        if barrier.arrived.len() >= barrier.participants.len() {
            barrier.released = true;
            barrier.open = false;
            self.add_notice(
                Severity::Info,
                "barrier-released",
                None,
                format!("all participants of component {} reached the barrier", key),
                false,
            );
        }
        self.barriers.insert(key.clone(), barrier.clone());
        self.persist(WalEvent::Barrier {
            barrier: barrier.clone(),
        });
        self.publish();
        Ok(barrier)
    }

    fn handle_barrier(
        &mut self,
        method: &str,
        request: &RpcRequest,
    ) -> Result<Value, (String, String)> {
        let agent = request
            .agent
            .as_deref()
            .or_else(|| request.params.get("agent").and_then(Value::as_str));
        let requested_component = request
            .params
            .get("component")
            .and_then(Value::as_str)
            .map(ToString::to_string);
        match method {
            "barrier.arrive" => {
                let agent = agent
                    .ok_or_else(|| ("SP_BAD_REQUEST".to_string(), "missing agent".to_string()))?;
                let global = request
                    .params
                    .get("scope")
                    .and_then(Value::as_str)
                    .map(|scope| scope == "workspace")
                    .unwrap_or(
                        self.config.barrier_scope == crate::protocol::BarrierScope::Workspace,
                    );
                self.arrive_barrier(agent, global)
                    .map(|barrier| json!(barrier))
            }
            "barrier.release" => {
                let keys: Vec<String> = match requested_component.clone() {
                    Some(component) => vec![component],
                    None => self.barriers.keys().cloned().collect(),
                };
                let mut released = Vec::new();
                for key in keys {
                    if let Some(barrier) = self.barriers.get_mut(&key) {
                        barrier.released = true;
                        barrier.open = false;
                        let snapshot = barrier.clone();
                        self.persist(WalEvent::Barrier {
                            barrier: snapshot.clone(),
                        });
                        released.push(snapshot);
                    }
                }
                self.add_notice(
                    Severity::Warning,
                    "barrier-forced-release",
                    agent.map(ToString::to_string),
                    format!("operator released {} active barrier(s)", released.len()),
                    true,
                );
                self.publish();
                Ok(json!({"released": released}))
            }
            "barrier.status" => {
                let mut expired = Vec::new();
                for (key, barrier) in self.barriers.iter_mut() {
                    if barrier.open && now_ms() >= barrier.deadline_ms {
                        barrier.released = true;
                        barrier.open = false;
                        expired.push(key.clone());
                    }
                }
                for key in expired.iter() {
                    if let Some(barrier) = self.barriers.get(key).cloned() {
                        self.persist(WalEvent::Barrier { barrier });
                    }
                    self.add_notice(
                        Severity::Warning,
                        "barrier-timeout",
                        None,
                        format!("barrier {} deadline expired; coordination degraded", key),
                        true,
                    );
                }
                if !expired.is_empty() {
                    self.publish();
                }
                let barriers: Vec<BarrierState> = match requested_component {
                    Some(component) => self.barriers.get(&component).cloned().into_iter().collect(),
                    None => self.barriers.values().cloned().collect(),
                };
                Ok(json!({
                    "barrier": self.primary_barrier(),
                    "barriers": barriers,
                    "components": self.components(),
                }))
            }
            _ => Err((
                "SP_METHOD_NOT_FOUND".to_string(),
                format!("unknown barrier method {method}"),
            )),
        }
    }

    fn handle(&mut self, request: &RpcRequest) -> RpcResponse {
        if request.token != self.token {
            return RpcResponse::err(request.id, "SP_UNAUTHORIZED", "invalid daemon token");
        }
        if self.prune_leases() {
            self.publish();
        }
        let result: Result<Value, (String, String)> = match request.method.as_str() {
            "ping" => Ok(json!({
                "version": crate::protocol::PROTOCOL_VERSION,
                "pid": std::process::id(),
                "master_root": self.master.to_string_lossy(),
                "started_ms": self.started_ms
            })),
            "status" => Ok(serde_json::to_value(self.snapshot()).unwrap_or(Value::Null)),
            "session.ensure" => {
                let Some(agent) = request
                    .agent
                    .as_deref()
                    .or_else(|| request.params.get("agent").and_then(Value::as_str))
                else {
                    return RpcResponse::err(request.id, "SP_BAD_REQUEST", "missing agent");
                };
                self.ensure_session(agent)
                    .map(|session| serde_json::to_value(session).unwrap_or(Value::Null))
            }
            "session.get" => {
                let Some(agent) = request
                    .agent
                    .as_deref()
                    .or_else(|| request.params.get("agent").and_then(Value::as_str))
                else {
                    return RpcResponse::err(request.id, "SP_BAD_REQUEST", "missing agent");
                };
                self.find_session(agent)
                    .map(|session| serde_json::to_value(session).unwrap_or(Value::Null))
            }
            "session.list" => {
                Ok(serde_json::to_value(self.snapshot().sessions).unwrap_or(Value::Null))
            }
            "session.heartbeat" => {
                let Some(agent) = request
                    .agent
                    .as_deref()
                    .or_else(|| request.params.get("agent").and_then(Value::as_str))
                else {
                    return RpcResponse::err(request.id, "SP_BAD_REQUEST", "missing agent");
                };
                if let Ok(session) = self.find_session(agent) {
                    let mut session = session.clone();
                    session.last_heartbeat_ms = now_ms();
                    self.sessions.insert(agent.to_string(), session.clone());
                    let _ = session::save_session(&self.master, &session);
                    self.publish();
                    Ok(json!({"heartbeat": true, "agent": agent}))
                } else {
                    Err((
                        "SP_SESSION_NOT_ATTACHED".to_string(),
                        format!("no active session for agent {agent}"),
                    ))
                }
            }
            "session.evict" => {
                let Some(agent) = request
                    .agent
                    .as_deref()
                    .or_else(|| request.params.get("agent").and_then(Value::as_str))
                else {
                    return RpcResponse::err(request.id, "SP_BAD_REQUEST", "missing agent");
                };
                let evicted = if let Some(session) = self.sessions.get_mut(agent) {
                    session.status = SessionStatus::Abandoned;
                    Some(session.clone())
                } else {
                    None
                };
                if let Some(session) = evicted {
                    self.persist(WalEvent::Session { session });
                }
                self.release_agent_claims(agent);
                // Eviction is an operator action, so it reclaims the workspace and
                // unregisters the worktrees. Leaving them behind strands disk space
                // and, worse, stale registrations that block later checkouts.
                let reclaimed = if let Some(session) = self.sessions.get(agent) {
                    let session = session.clone();
                    session::remove_session_worktrees(&self.master, &session);
                    let dir = session::session_dir(&self.master, agent);
                    let freed = workspace::dir_size(&dir);
                    let _ = fs::remove_dir_all(&dir);
                    self.sessions.remove(agent);
                    Some(freed)
                } else {
                    None
                };
                self.add_notice(
                    Severity::Warning,
                    "session-evicted",
                    Some(agent.to_string()),
                    match reclaimed {
                        Some(bytes) => format!(
                            "operator evicted agent {agent}; reclaimed {} and unregistered its worktrees",
                            workspace::human_bytes(bytes)
                        ),
                        None => format!("operator evicted agent {agent}"),
                    },
                    true,
                );
                self.publish();
                Ok(json!({"evicted": true, "agent": agent, "reclaimed_bytes": reclaimed}))
            }
            "session.refresh_base" => {
                let Some(agent) = request
                    .agent
                    .as_deref()
                    .or_else(|| request.params.get("agent").and_then(Value::as_str))
                else {
                    return RpcResponse::err(request.id, "SP_BAD_REQUEST", "missing agent");
                };
                let Some(repo) = request.params.get("repo").and_then(Value::as_str) else {
                    return RpcResponse::err(request.id, "SP_BAD_REQUEST", "missing repo");
                };
                let Some(base_oid) = request.params.get("base_oid").and_then(Value::as_str) else {
                    return RpcResponse::err(request.id, "SP_BAD_REQUEST", "missing base_oid");
                };
                let Some(session) = self.sessions.get(agent).cloned() else {
                    return RpcResponse::err(request.id, "SP_SESSION_NOT_ATTACHED", "no session");
                };
                let mut updated = session;
                let found = updated
                    .repos
                    .iter_mut()
                    .find(|candidate| candidate.relative_path == repo || candidate.name == repo);
                match found {
                    Some(record) => {
                        record.base_oid = base_oid.to_string();
                        self.sessions.insert(agent.to_string(), updated.clone());
                        let _ = session::save_session(&self.master, &updated);
                        self.persist(WalEvent::Session {
                            session: updated.clone(),
                        });
                        self.add_notice(
                            Severity::Info,
                            "session-base-refreshed",
                            Some(agent.to_string()),
                            format!("agent {} refreshed {} base to {}", agent, repo, base_oid),
                            false,
                        );
                        self.publish();
                        Ok(json!({"refreshed": true, "repo": repo, "base_oid": base_oid}))
                    }
                    None => Err((
                        "SP_REPOSITORY_NOT_FOUND".to_string(),
                        format!("repository {repo} is not attached to agent {agent}"),
                    )),
                }
            }
            "commit.prepare" => {
                let Some(agent) = request
                    .agent
                    .as_deref()
                    .or_else(|| request.params.get("agent").and_then(Value::as_str))
                else {
                    return RpcResponse::err(request.id, "SP_BAD_REQUEST", "missing agent");
                };
                self.prepare_commit(agent, &request.params)
            }
            "commit.validate" => {
                let Some(agent) = request
                    .agent
                    .as_deref()
                    .or_else(|| request.params.get("agent").and_then(Value::as_str))
                else {
                    return RpcResponse::err(request.id, "SP_BAD_REQUEST", "missing agent");
                };
                self.validate_commit(agent, &request.params)
            }
            "commit.record" => {
                let Some(agent) = request
                    .agent
                    .as_deref()
                    .or_else(|| request.params.get("agent").and_then(Value::as_str))
                else {
                    return RpcResponse::err(request.id, "SP_BAD_REQUEST", "missing agent");
                };
                self.record_commit(agent, &request.params)
            }
            "commit.abort" => {
                let Some(agent) = request
                    .agent
                    .as_deref()
                    .or_else(|| request.params.get("agent").and_then(Value::as_str))
                else {
                    return RpcResponse::err(request.id, "SP_BAD_REQUEST", "missing agent");
                };
                self.abort_commit(agent, &request.params)
            }
            "permit.verify" => {
                let permit_id = request
                    .params
                    .get("permit_id")
                    .and_then(Value::as_str)
                    .unwrap_or("");
                let agent = request
                    .agent
                    .as_deref()
                    .or_else(|| request.params.get("agent").and_then(Value::as_str))
                    .unwrap_or("");
                let now = now_ms();
                let valid = self.intents.iter().any(|intent| {
                    intent.permit_id.as_deref() == Some(permit_id)
                        && intent.agent == agent
                        && intent.status == IntentStatus::Prepared
                        && now.saturating_sub(intent.created_ms) <= PERMIT_TTL_MS
                });
                if valid {
                    Ok(json!({"valid": true}))
                } else {
                    Err((
                        "SP_COMMIT_PERMIT_INVALID".to_string(),
                        "permit is missing, expired, or already consumed".to_string(),
                    ))
                }
            }
            "claim.list" => Ok(serde_json::to_value(&self.claims).unwrap_or(Value::Null)),
            "claim.path" => match request
                .agent
                .as_deref()
                .or_else(|| request.params.get("agent").and_then(Value::as_str))
            {
                Some(agent) => self.claim_path(agent, &request.params),
                None => Err(("SP_BAD_REQUEST".to_string(), "missing agent".to_string())),
            },
            "claim.release" => match request
                .agent
                .as_deref()
                .or_else(|| request.params.get("agent").and_then(Value::as_str))
            {
                Some(agent) => self.release_path_claim(agent, &request.params),
                None => Err(("SP_BAD_REQUEST".to_string(), "missing agent".to_string())),
            },
            "claim.paths" => Ok(json!({"path_claims": self.live_path_claims()})),
            "intent.list" => Ok(serde_json::to_value(&self.intents).unwrap_or(Value::Null)),
            "sequence.next" => {
                let namespace = request
                    .params
                    .get("namespace")
                    .and_then(Value::as_str)
                    .unwrap_or("");
                let after = request
                    .params
                    .get("after")
                    .and_then(Value::as_u64)
                    .unwrap_or(0);
                let mut used = HashSet::new();
                for claim in &self.claims {
                    if claim.namespace == namespace
                        && (claim.state == ClaimState::Committed
                            || (claim.state == ClaimState::Reserved && claim.expires_ms > now_ms()))
                    {
                        if let Some(value) = claim.numeric {
                            used.insert(value);
                        }
                    }
                }
                let mut next = after.saturating_add(1);
                while used.contains(&next) {
                    next += 1;
                }
                Ok(json!({"key": next.to_string(), "numeric": next}))
            }
            "config.get" => Ok(serde_json::to_value(&self.config).unwrap_or(Value::Null)),
            "config.set" => self.set_mode(&request.params),
            method if method.starts_with("barrier.") => self.handle_barrier(method, request),
            "notice.list" => {
                let after = request
                    .params
                    .get("after_seq")
                    .and_then(Value::as_u64)
                    .unwrap_or(0);
                let limit = request
                    .params
                    .get("limit")
                    .and_then(Value::as_u64)
                    .unwrap_or(100) as usize;
                let notices: Vec<Notice> = self
                    .notices
                    .iter()
                    .filter(|notice| notice.seq > after)
                    .take(limit)
                    .cloned()
                    .collect();
                Ok(json!({"notices": notices}))
            }
            // Client-side coordination actions (resequence, sync) are performed
            // by the CLI against the session worktree, but they are still
            // coordination actions and belong in the audit ledger.
            "audit.record" => {
                let Some(kind) = request.params.get("kind").and_then(Value::as_str) else {
                    return RpcResponse::err(request.id, "SP_BAD_REQUEST", "missing kind");
                };
                let message = request
                    .params
                    .get("message")
                    .and_then(Value::as_str)
                    .unwrap_or("")
                    .to_string();
                let severity = match request.params.get("severity").and_then(Value::as_str) {
                    Some("warning") => Severity::Warning,
                    Some("blocking") => Severity::Blocking,
                    _ => Severity::Info,
                };
                let agent = request.agent.clone().or_else(|| {
                    request
                        .params
                        .get("agent")
                        .and_then(Value::as_str)
                        .map(str::to_string)
                });
                self.add_notice(severity, kind, agent, message, true);
                self.publish();
                Ok(json!({"recorded": true, "kind": kind}))
            }
            "shutdown" => {
                Ok(json!({"shutdown": false, "message": "daemon shutdown is signal-driven"}))
            }
            _ => Err((
                "SP_METHOD_NOT_FOUND".to_string(),
                format!("unknown method {}", request.method),
            )),
        };
        match result {
            Ok(data) => RpcResponse::ok(request.id, data),
            Err((code, message)) => RpcResponse::err(request.id, &code, message),
        }
    }
}

fn handle_connection(
    stream: UnixStream,
    state: &Arc<Mutex<Daemon>>,
    active: &Arc<AtomicUsize>,
    max_connections: usize,
) {
    if active.fetch_add(1, Ordering::AcqRel) >= max_connections {
        active.fetch_sub(1, Ordering::AcqRel);
        let response = RpcResponse::err(0, "SP_REPOSITORY_BUSY", "daemon connection limit reached");
        let _ = write_response(stream, &response);
        return;
    }
    let _ = stream.set_read_timeout(Some(Duration::from_secs(10)));
    let _ = stream.set_write_timeout(Some(Duration::from_secs(10)));
    if let Ok(clone) = stream.try_clone() {
        let mut reader = BufReader::new(clone);
        let mut line = String::new();
        match reader.read_line(&mut line) {
            Ok(size) if size > 0 && size < 2 * 1024 * 1024 => {
                let response = match serde_json::from_str::<RpcRequest>(&line) {
                    Ok(request) => state.lock().unwrap().handle(&request),
                    Err(err) => RpcResponse::err(0, "SP_PROTOCOL_MISMATCH", err.to_string()),
                };
                let _ = write_response(stream, &response);
            }
            Ok(_) => {}
            Err(err) => {
                let response = RpcResponse::err(0, "SP_TRANSPORT_TIMEOUT", err.to_string());
                let _ = write_response(stream, &response);
            }
        }
    }
    active.fetch_sub(1, Ordering::AcqRel);
}

fn write_response(mut stream: UnixStream, response: &RpcResponse) -> std::io::Result<()> {
    let payload = serde_json::to_vec(response).map_err(std::io::Error::other)?;
    stream.write_all(&payload)?;
    stream.write_all(b"\n")?;
    stream.flush()?;
    Ok(())
}

pub fn run_daemon() -> Result<(), String> {
    let cwd = std::env::current_dir().map_err(|err| err.to_string())?;
    let master = std::env::var("SP_MASTER_ROOT")
        .map(PathBuf::from)
        .map(|path| workspace::canonical(&path))
        .unwrap_or_else(|_| workspace::find_master_root(&cwd));
    workspace::ensure_layout(&master).map_err(|err| err.to_string())?;

    let lock_path = workspace::state_dir(&master).join("daemon.lock");
    let lock_file = OpenOptions::new()
        .create(true)
        .truncate(false)
        .read(true)
        .write(true)
        .open(&lock_path)
        .map_err(|err| format!("cannot open daemon lock: {err}"))?;
    let rc = unsafe { libc::flock(lock_file.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) };
    if rc != 0 {
        eprintln!(
            "ts={} level=info event=daemon.already_running master={}",
            now_ms(),
            master.display()
        );
        return Ok(());
    }

    let socket_path = workspace::socket_path(&master);
    if socket_path.as_os_str().len() >= 100 {
        return Err(format!(
            "Unix socket path is too long for macOS: {}",
            socket_path.display()
        ));
    }
    if let Some(parent) = socket_path.parent() {
        fs::create_dir_all(parent).map_err(|err| err.to_string())?;
        let mut perms = fs::metadata(parent)
            .map_err(|err| err.to_string())?
            .permissions();
        perms.set_mode(0o700);
        fs::set_permissions(parent, perms).map_err(|err| err.to_string())?;
    }
    let _ = fs::remove_file(&socket_path);
    let listener = UnixListener::bind(&socket_path)
        .map_err(|err| format!("cannot bind {}: {err}", socket_path.display()))?;
    let mut socket_perms = fs::metadata(&socket_path)
        .map_err(|err| err.to_string())?
        .permissions();
    socket_perms.set_mode(0o600);
    fs::set_permissions(&socket_path, socket_perms).ok();

    let token = workspace::random_token();
    let endpoint = Endpoint {
        socket: socket_path.to_string_lossy().to_string(),
        token: token.clone(),
    };
    workspace::write_endpoint(&master, &endpoint).map_err(|err| err.to_string())?;
    let endpoint_path = workspace::endpoint_path(&master);
    let pid_path = workspace::pid_path(&master);
    let _ = workspace::atomic_write(&pid_path, std::process::id().to_string().as_bytes());

    let mut daemon = Daemon::new(&master, token)?;
    daemon.log("daemon.ready", &format!("socket={}", socket_path.display()));
    daemon.add_notice(
        Severity::Info,
        "daemon-started",
        None,
        format!("daemon started for {}", master.display()),
        false,
    );
    daemon.publish();

    let state = Arc::new(Mutex::new(daemon));
    let active = Arc::new(AtomicUsize::new(0));
    let lock_guard = LockGuard {
        _file: lock_file,
        socket: socket_path.clone(),
        endpoint: endpoint_path,
    };
    let _ = &lock_guard;
    let max_connections = 16usize;
    // Fixed worker pool: a thread per connection would let a burst of concurrent
    // shims grow the resident set without bound (each thread reserves a stack).
    let mut workers = Vec::with_capacity(max_connections);
    for index in 0..max_connections {
        let state = Arc::clone(&state);
        let active = Arc::clone(&active);
        let listener = listener.try_clone().map_err(|err| err.to_string())?;
        let handle = std::thread::Builder::new()
            .name(format!("sp-rpc-{index}"))
            .stack_size(256 * 1024)
            .spawn(move || {
                for stream in listener.incoming() {
                    match stream {
                        Ok(stream) => handle_connection(stream, &state, &active, max_connections),
                        Err(err) => {
                            eprintln!(
                                "ts={} level=error event=daemon.accept error={}",
                                now_ms(),
                                err
                            );
                        }
                    }
                }
            })
            .map_err(|err| err.to_string())?;
        workers.push(handle);
    }
    for worker in workers {
        let _ = worker.join();
    }
    drop(listener);
    let _ = fs::remove_file(&socket_path);
    let _ = fs::remove_file(workspace::endpoint_path(&master));
    let _ = fs::remove_file(workspace::token_path(&master));
    let _ = fs::remove_file(workspace::pid_path(&master));
    Ok(())
}

struct LockGuard {
    _file: File,
    socket: PathBuf,
    endpoint: PathBuf,
}

impl Drop for LockGuard {
    fn drop(&mut self) {
        let _ = fs::remove_file(&self.socket);
        let _ = fs::remove_file(&self.endpoint);
    }
}

pub fn status_snapshot(master: &Path) -> Option<(LedgerSnapshot, u64)> {
    read_snapshot(master)
}
