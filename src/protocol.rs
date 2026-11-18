use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::fmt;
use std::str::FromStr;
use std::time::{SystemTime, UNIX_EPOCH};

pub const PROTOCOL_VERSION: u16 = 1;
pub const SCHEMA_VERSION: &str = "sp.conflict.v1";

pub fn now_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}

#[derive(Serialize, Deserialize, Clone, Debug, Default, PartialEq, Eq)]
#[serde(rename_all = "kebab-case")]
pub enum Mode {
    #[default]
    Fcfs,
    BarrierStacked,
    BarrierIntegration,
}

impl fmt::Display for Mode {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            Mode::Fcfs => "fcfs",
            Mode::BarrierStacked => "barrier-stacked",
            Mode::BarrierIntegration => "barrier-integration",
        })
    }
}

impl FromStr for Mode {
    type Err = String;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        match s {
            "fcfs" => Ok(Mode::Fcfs),
            "barrier-stacked" => Ok(Mode::BarrierStacked),
            "barrier-integration" => Ok(Mode::BarrierIntegration),
            _ => Err(format!("unknown mode: {s}")),
        }
    }
}

#[derive(Serialize, Deserialize, Clone, Debug, PartialEq, Eq)]
#[serde(rename_all = "kebab-case")]
pub enum BarrierScope {
    Workspace,
    Component,
}

impl BarrierScope {
    pub fn parse(value: &str) -> Result<Self, String> {
        match value {
            "workspace" => Ok(BarrierScope::Workspace),
            "component" => Ok(BarrierScope::Component),
            other => Err(format!(
                "unknown barrier scope {other}; expected workspace or component"
            )),
        }
    }
}

#[derive(Serialize, Deserialize, Clone, Debug)]
pub struct Config {
    pub mode: Mode,
    pub barrier_scope: BarrierScope,
    pub barrier_timeout_ms: u64,
    pub lease_timeout_ms: u64,
    pub port_base_start: u16,
    pub port_band_width: u16,
    pub ui_port: u16,
    pub max_sessions: u16,
}

impl Default for Config {
    fn default() -> Self {
        Self {
            mode: Mode::Fcfs,
            barrier_scope: BarrierScope::Workspace,
            barrier_timeout_ms: 120_000,
            lease_timeout_ms: 300_000,
            port_base_start: 31_000,
            port_band_width: 100,
            ui_port: 19_200,
            max_sessions: 32,
        }
    }
}

#[derive(Serialize, Deserialize, Clone, Debug, PartialEq, Eq)]
#[serde(rename_all = "kebab-case")]
pub enum SessionStatus {
    Active,
    Abandoned,
}

#[derive(Serialize, Deserialize, Clone, Debug)]
pub struct SessionRepo {
    pub name: String,
    pub relative_path: String,
    pub worktree: String,
    pub branch: String,
    pub base_oid: String,
}

#[derive(Serialize, Deserialize, Clone, Debug)]
pub struct SessionRecord {
    pub agent: String,
    pub session_id: String,
    pub slot: u16,
    pub port_base: u16,
    pub workspace: String,
    pub created_ms: u64,
    pub last_heartbeat_ms: u64,
    pub status: SessionStatus,
    pub repos: Vec<SessionRepo>,
    /// Milliseconds until the lease expires, computed when the snapshot is
    /// published. `0` means the lease has already lapsed. Not persisted.
    #[serde(default)]
    pub lease_remaining_ms: u64,
    /// True when the session holds uncommitted edits or unpushed commits, so a
    /// reader can tell "idle" from "has work in flight". Not persisted.
    #[serde(default)]
    pub has_unpreserved_work: bool,
}

#[derive(Serialize, Deserialize, Clone, Debug, PartialEq, Eq)]
#[serde(rename_all = "kebab-case")]
pub enum ClaimState {
    Reserved,
    Committed,
    Released,
}

#[derive(Serialize, Deserialize, Clone, Debug)]
pub struct SequenceClaim {
    pub claim_id: u64,
    pub permit_id: String,
    pub agent: String,
    pub session_id: String,
    pub repo: String,
    pub rule: String,
    pub namespace: String,
    pub key: String,
    pub numeric: Option<u64>,
    pub path: String,
    pub line: u64,
    pub state: ClaimState,
    pub commit_oid: Option<String>,
    pub created_ms: u64,
    pub expires_ms: u64,
}

/// Advisory or mandatory reservation of a repository path by an agent.
///
/// Path claims let an agent announce intent *before* it has staged anything, so
/// peers can see contention while work is still in progress. A `Mandatory` claim
/// is enforced by the commit gate; an `Advisory` claim only feeds the conflict
/// graph and the notice stream.
#[derive(Serialize, Deserialize, Clone, Debug, PartialEq, Eq)]
#[serde(rename_all = "kebab-case")]
pub enum ClaimStrength {
    Advisory,
    Mandatory,
}

#[derive(Serialize, Deserialize, Clone, Debug)]
pub struct PathClaim {
    pub claim_id: u64,
    pub agent: String,
    pub repo: String,
    pub path: String,
    pub strength: ClaimStrength,
    pub note: Option<String>,
    pub created_ms: u64,
    pub expires_ms: u64,
}

#[derive(Serialize, Deserialize, Clone, Debug, PartialEq, Eq)]
#[serde(rename_all = "kebab-case")]
pub enum Severity {
    Info,
    Warning,
    Blocking,
}

#[derive(Serialize, Deserialize, Clone, Debug)]
pub struct Notice {
    pub seq: u64,
    pub severity: Severity,
    pub kind: String,
    pub agent: Option<String>,
    pub message: String,
    pub created_ms: u64,
}

#[derive(Serialize, Deserialize, Clone, Debug, PartialEq, Eq)]
#[serde(rename_all = "kebab-case")]
pub enum IntentStatus {
    Prepared,
    Committed,
    Blocked,
    Abandoned,
}

#[derive(Serialize, Deserialize, Clone, Debug)]
pub struct IntentRecord {
    pub intent_id: u64,
    pub agent: String,
    pub repo: String,
    pub branch: String,
    pub paths: Vec<String>,
    pub base_oid: String,
    pub index_tree: String,
    pub status: IntentStatus,
    pub permit_id: Option<String>,
    pub created_ms: u64,
}

#[derive(Serialize, Deserialize, Clone, Debug, Default)]
pub struct BarrierState {
    pub component: String,
    pub barrier_id: u64,
    pub open: bool,
    pub opened_ms: u64,
    pub deadline_ms: u64,
    pub participants: Vec<String>,
    pub arrived: Vec<String>,
    pub released: bool,
}

#[derive(Serialize, Deserialize, Clone, Debug)]
pub struct ConflictComponent {
    pub component_id: usize,
    pub agents: Vec<String>,
    pub reason: String,
}

#[derive(Serialize, Deserialize, Clone, Debug)]
pub struct LedgerSnapshot {
    pub daemon_pid: u32,
    pub started_ms: u64,
    pub master_root: String,
    pub config: Config,
    pub sessions: Vec<SessionRecord>,
    pub claims: Vec<SequenceClaim>,
    pub intents: Vec<IntentRecord>,
    pub notices: Vec<Notice>,
    pub barrier: BarrierState,
    pub barriers: Vec<BarrierState>,
    pub components: Vec<ConflictComponent>,
    pub path_claims: Vec<PathClaim>,
}

#[derive(Serialize, Deserialize, Clone, Debug, Default)]
pub struct Endpoint {
    pub socket: String,
    pub token: String,
}

#[derive(Serialize, Deserialize, Clone, Debug)]
pub struct RpcRequest {
    pub id: u64,
    pub token: String,
    pub method: String,
    pub agent: Option<String>,
    pub params: Value,
}

#[derive(Serialize, Deserialize, Clone, Debug)]
pub struct RpcResponse {
    pub id: u64,
    pub ok: bool,
    pub code: Option<String>,
    pub message: Option<String>,
    pub data: Value,
}

impl RpcResponse {
    pub fn ok(id: u64, data: Value) -> Self {
        Self {
            id,
            ok: true,
            code: None,
            message: None,
            data,
        }
    }

    pub fn err(id: u64, code: &str, message: impl Into<String>) -> Self {
        Self {
            id,
            ok: false,
            code: Some(code.to_string()),
            message: Some(message.into()),
            data: Value::Null,
        }
    }
}

#[derive(Serialize, Deserialize, Clone, Debug, PartialEq, Eq)]
#[serde(rename_all = "kebab-case")]
pub enum ConflictCode {
    SequenceCollision,
    StaleBase,
    FileOverlap,
    MergeConflict,
    IntentConflict,
    BarrierWait,
    BarrierTimeout,
    CommitPostconditionFailed,
}

impl ConflictCode {
    pub fn as_str(&self) -> &'static str {
        match self {
            ConflictCode::SequenceCollision => "SP_SEQUENCE_COLLISION",
            ConflictCode::StaleBase => "SP_STALE_BASE",
            ConflictCode::FileOverlap => "SP_INTENT_CONFLICT",
            ConflictCode::MergeConflict => "SP_MERGE_CONFLICT",
            ConflictCode::IntentConflict => "SP_INTENT_CONFLICT",
            ConflictCode::BarrierWait => "SP_BARRIER_WAIT",
            ConflictCode::BarrierTimeout => "SP_BARRIER_TIMEOUT",
            ConflictCode::CommitPostconditionFailed => "SP_COMMIT_POSTCONDITION_FAILED",
        }
    }
}

#[derive(Serialize, Deserialize, Clone, Debug, Default)]
pub struct ConflictEvidence {
    pub kind: String,
    pub rule: Option<String>,
    pub namespace: Option<String>,
    pub path: Option<String>,
    pub line: Option<u64>,
    pub requested: Option<String>,
    pub holder: Option<String>,
    pub holder_intent: Option<u64>,
    pub lowest_free: Option<String>,
    pub base_oid: Option<String>,
    pub target_oid: Option<String>,
    pub conflicting_paths: Vec<String>,
    pub detail: Option<String>,
}

#[derive(Serialize, Deserialize, Clone, Debug)]
pub struct ResolutionStep {
    pub ordinal: u8,
    pub action: String,
    pub command: Option<String>,
}

#[derive(Serialize, Deserialize, Clone, Debug)]
pub struct ResolutionPlan {
    pub summary: String,
    pub steps: Vec<ResolutionStep>,
    pub safe_retry_command: Option<String>,
    pub forbidden_actions: Vec<String>,
}

#[derive(Serialize, Deserialize, Clone, Debug)]
pub struct RetryContract {
    pub mode: String,
    pub idempotent: bool,
    pub retry_after_ms: Option<u64>,
    pub max_attempts: u8,
    pub validation: String,
}

#[derive(Serialize, Deserialize, Clone, Debug)]
pub struct ConflictReport {
    pub schema: String,
    pub code: String,
    pub conflict_id: String,
    pub operation: String,
    pub agent: String,
    pub repo: String,
    pub branch: String,
    pub base_oid: String,
    pub head_oid: String,
    pub paths: Vec<String>,
    pub evidence: ConflictEvidence,
    pub resolution: ResolutionPlan,
    pub retry: RetryContract,
}

pub fn default_forbidden_actions() -> Vec<String> {
    vec![
        "git commit --no-verify".to_string(),
        "editing .scratchpost state".to_string(),
        "force-pushing another agent branch".to_string(),
    ]
}

pub fn default_retry_contract() -> RetryContract {
    RetryContract {
        mode: "rerun_original_command".to_string(),
        idempotent: true,
        retry_after_ms: None,
        max_attempts: 3,
        validation: "automatic_on_retry".to_string(),
    }
}

#[derive(Serialize, Deserialize, Clone, Debug)]
pub struct CommitPermit {
    pub permit_id: String,
    pub intent_id: u64,
    pub agent: String,
    pub repo: String,
    pub branch: String,
    pub base_oid: String,
    pub index_tree: String,
    pub expires_ms: u64,
}

#[derive(Serialize, Deserialize, Clone, Debug)]
pub struct CommitDecision {
    pub allowed: bool,
    pub permit: Option<CommitPermit>,
    pub conflict: Option<ConflictReport>,
}
