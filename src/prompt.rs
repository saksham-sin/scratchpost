use crate::protocol::{
    default_forbidden_actions, default_retry_contract, ConflictEvidence, ConflictReport,
    ResolutionPlan, ResolutionStep, SequenceClaim,
};
use crate::rules::Observation;
use crate::style::Style;

pub fn render_stderr(report: &ConflictReport) -> String {
    let style = Style::stderr();
    let mut out = String::new();
    out.push_str(&format!(
        "SP_BLOCKED v1 code={} conflict_id={}\n",
        report.code, report.conflict_id
    ));
    out.push_str(&format!(
        "agent={} repo={} branch={} operation={}\n\n",
        report.agent, report.repo, report.branch, report.operation
    ));
    out.push_str(&format!(
        "{}\n",
        style.bad("Your commit was NOT created. A deterministic repository check failed.")
    ));
    out.push_str("This is a coordination conflict, not a transient Git error.\n\n");
    out.push_str(&format!("{}\n", style.section("EVIDENCE")));
    if let Some(rule) = &report.evidence.rule {
        out.push_str(&format!("  rule: {}\n", rule));
    }
    if let Some(namespace) = &report.evidence.namespace {
        out.push_str(&format!("  namespace: {}\n", namespace));
    }
    if let Some(path) = &report.evidence.path {
        out.push_str(&format!("  file: {}\n", path));
    }
    if let Some(line) = report.evidence.line {
        out.push_str(&format!("  line: {}\n", line));
    }
    if let Some(requested) = &report.evidence.requested {
        out.push_str(&format!("  requested value: {}\n", requested));
    }
    if let Some(holder) = &report.evidence.holder {
        out.push_str(&format!("  reserved by: {}\n", holder));
    }
    if let Some(lowest) = &report.evidence.lowest_free {
        out.push_str(&format!("  lowest_free: {}\n", lowest));
    }
    if let Some(base) = &report.evidence.base_oid {
        out.push_str(&format!("  base: {}\n", base));
    }
    if let Some(target) = &report.evidence.target_oid {
        out.push_str(&format!("  target: {}\n", target));
    }
    if !report.evidence.conflicting_paths.is_empty() {
        out.push_str(&format!(
            "  conflicting paths: {}\n",
            report.evidence.conflicting_paths.join(", ")
        ));
    }
    if let Some(detail) = &report.evidence.detail {
        out.push_str(&format!("  detail: {}\n", detail));
    }
    out.push_str(&format!("\n{}\n", style.section("WHY THIS MATTERS")));
    out.push_str("  Another agent or the base branch already owns part of this change.\n");
    out.push_str("  Committing it unchanged would produce an inconsistent repository state.\n\n");
    out.push_str("RESOLUTION\n");
    for step in &report.resolution.steps {
        out.push_str(&format!("  {}. {}\n", step.ordinal, step.action));
        if let Some(command) = &step.command {
            out.push_str(&format!("       {}\n", style.accent(command)));
        }
    }
    out.push_str(&format!("\n{}\n", style.section("RETRY CONTRACT")));
    out.push_str(&format!("  mode: {}\n", report.retry.mode));
    out.push_str(&format!("  validation: {}\n", report.retry.validation));
    out.push_str(&format!("  max_attempts: {}\n", report.retry.max_attempts));
    out.push_str(&format!("  idempotent: {}\n", report.retry.idempotent));
    out.push_str(&format!("\n{}\n", style.section("FORBIDDEN")));
    for action in &report.resolution.forbidden_actions {
        out.push_str(&format!("  - {}\n", action));
    }
    out.push_str("\nSP_CONFLICT_JSON=");
    out.push_str(&serde_json::to_string(report).unwrap_or_else(|_| "{}".to_string()));
    out.push_str("\n\nExit status: 1\n");
    out
}

/// The commit being rejected, as the prompt needs to describe it.
pub struct ConflictSubject<'a> {
    pub conflict_id: String,
    pub agent: &'a str,
    pub repo: &'a str,
    pub branch: &'a str,
    pub base_oid: &'a str,
    pub head_oid: &'a str,
}

pub fn sequence_conflict(
    subject: &ConflictSubject<'_>,
    observation: &Observation,
    claim: &SequenceClaim,
    lowest_free: &str,
) -> ConflictReport {
    let ConflictSubject {
        conflict_id,
        agent,
        repo,
        branch,
        base_oid,
        head_oid,
    } = subject;
    let resequence = format!(
        "sp resequence --agent {} --file {}/{} --from {} --to {}",
        agent, repo, observation.path, observation.key, lowest_free
    );
    let stage = format!("git -C {} add -- {}", repo, observation.path);
    ConflictReport {
        schema: crate::protocol::SCHEMA_VERSION.to_string(),
        code: crate::protocol::ConflictCode::SequenceCollision
            .as_str()
            .to_string(),
        conflict_id: conflict_id.to_string(),
        operation: "git.commit".to_string(),
        agent: agent.to_string(),
        repo: repo.to_string(),
        branch: branch.to_string(),
        base_oid: base_oid.to_string(),
        head_oid: head_oid.to_string(),
        paths: vec![observation.path.clone()],
        evidence: ConflictEvidence {
            kind: "sequence_collision".to_string(),
            rule: Some(observation.rule.clone()),
            namespace: Some(observation.namespace.clone()),
            path: Some(observation.path.clone()),
            line: Some(observation.line),
            requested: Some(observation.key.clone()),
            holder: Some(format!("{} (intent/claim {})", claim.agent, claim.claim_id)),
            holder_intent: Some(claim.claim_id),
            lowest_free: Some(lowest_free.to_string()),
            base_oid: Some(base_oid.to_string()),
            target_oid: None,
            conflicting_paths: vec![observation.path.clone()],
            detail: Some(format!(
                "sequence namespace {} is already reserved",
                observation.namespace
            )),
        },
        resolution: ResolutionPlan {
            summary: "Reassign the duplicate sequence value and retry the commit.".to_string(),
            steps: vec![
                ResolutionStep {
                    ordinal: 1,
                    action: format!(
                        "Change the value at {}:{} from {} to {}.",
                        observation.path, observation.line, observation.key, lowest_free
                    ),
                    command: Some(resequence),
                },
                ResolutionStep {
                    ordinal: 2,
                    action: "Re-stage the edited file.".to_string(),
                    command: Some(stage),
                },
                ResolutionStep {
                    ordinal: 3,
                    action: "Retry the original commit.".to_string(),
                    command: Some("git commit -m \"<your original message>\"".to_string()),
                },
            ],
            safe_retry_command: None,
            forbidden_actions: default_forbidden_actions(),
        },
        retry: default_retry_contract(),
    }
}

pub fn barrier_wait(
    conflict_id: String,
    agent: &str,
    barrier: &crate::protocol::BarrierState,
) -> ConflictReport {
    let waiting = barrier
        .participants
        .iter()
        .filter(|participant| !barrier.arrived.contains(participant))
        .cloned()
        .collect::<Vec<_>>();
    ConflictReport {
        schema: crate::protocol::SCHEMA_VERSION.to_string(),
        code: crate::protocol::ConflictCode::BarrierWait
            .as_str()
            .to_string(),
        conflict_id,
        operation: "git.commit".to_string(),
        agent: agent.to_string(),
        repo: "multiple".to_string(),
        branch: "session".to_string(),
        base_oid: String::new(),
        head_oid: String::new(),
        paths: Vec::new(),
        evidence: ConflictEvidence {
            kind: "barrier_wait".to_string(),
            rule: None,
            namespace: None,
            path: None,
            line: None,
            requested: None,
            holder: None,
            holder_intent: None,
            lowest_free: None,
            base_oid: None,
            target_oid: None,
            conflicting_paths: Vec::new(),
            detail: Some(format!(
                "barrier {} for component {} is waiting for: {}",
                barrier.barrier_id,
                barrier.component,
                if waiting.is_empty() {
                    "release".to_string()
                } else {
                    waiting.join(", ")
                }
            )),
        },
        resolution: ResolutionPlan {
            summary: "Wait for the active coordination barrier, then retry the commit.".to_string(),
            steps: vec![
                ResolutionStep {
                    ordinal: 1,
                    action: "Inspect barrier participants and status.".to_string(),
                    command: Some("sp barrier status".to_string()),
                },
                ResolutionStep {
                    ordinal: 2,
                    action: "Retry the original commit after the barrier releases or the deadline degrades to FCFS.".to_string(),
                    command: Some("git commit -m \"<your original message>\"".to_string()),
                },
            ],
            safe_retry_command: None,
            forbidden_actions: default_forbidden_actions(),
        },
        retry: default_retry_contract(),
    }
}

pub fn stale_base_conflict(
    conflict_id: String,
    agent: &str,
    repo: &str,
    branch: &str,
    base_oid: &str,
    observed_oid: &str,
    paths: Vec<String>,
) -> ConflictReport {
    ConflictReport {
        schema: crate::protocol::SCHEMA_VERSION.to_string(),
        code: crate::protocol::ConflictCode::StaleBase
            .as_str()
            .to_string(),
        conflict_id,
        operation: "git.commit".to_string(),
        agent: agent.to_string(),
        repo: repo.to_string(),
        branch: branch.to_string(),
        base_oid: base_oid.to_string(),
        head_oid: observed_oid.to_string(),
        paths,
        evidence: ConflictEvidence {
            kind: "stale_base".to_string(),
            base_oid: Some(base_oid.to_string()),
            target_oid: Some(observed_oid.to_string()),
            detail: Some(
                "the canonical base branch advanced after this session was provisioned".to_string(),
            ),
            ..ConflictEvidence::default()
        },
        resolution: ResolutionPlan {
            summary: "Synchronize the session branch with the current base and retry.".to_string(),
            steps: vec![
                ResolutionStep {
                    ordinal: 1,
                    action: "Run the managed synchronization command.".to_string(),
                    command: Some(format!("sp sync --rebase --agent {}", agent)),
                },
                ResolutionStep {
                    ordinal: 2,
                    action: "Resolve any reported files and re-stage them.".to_string(),
                    command: None,
                },
                ResolutionStep {
                    ordinal: 3,
                    action: "Retry the original command.".to_string(),
                    command: Some("git commit -m \"<your original message>\"".to_string()),
                },
            ],
            safe_retry_command: Some(format!("sp sync --rebase --agent {}", agent)),
            forbidden_actions: default_forbidden_actions(),
        },
        retry: default_retry_contract(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::protocol::ClaimState;

    #[test]
    fn prompt_contains_required_contract_fields() {
        let observation = Observation {
            rule: "liquibase.changeset_id".to_string(),
            namespace: "auth-service:dbchangelog.xml".to_string(),
            key: "200".to_string(),
            numeric: Some(200),
            path: "dbchangelog.xml".to_string(),
            line: 7,
        };
        let claim = SequenceClaim {
            claim_id: 4,
            permit_id: "p1".to_string(),
            agent: "agent_1".to_string(),
            session_id: "s1".to_string(),
            repo: "auth-service".to_string(),
            rule: observation.rule.clone(),
            namespace: observation.namespace.clone(),
            key: "200".to_string(),
            numeric: Some(200),
            path: observation.path.clone(),
            line: 7,
            state: ClaimState::Committed,
            commit_oid: Some("abc".to_string()),
            created_ms: 1,
            expires_ms: 2,
        };
        let report = sequence_conflict(
            &ConflictSubject {
                conflict_id: "cid".to_string(),
                agent: "agent_2",
                repo: "auth-service",
                branch: "sp/agent_2/main",
                base_oid: "base",
                head_oid: "head",
            },
            &observation,
            &claim,
            "201",
        );
        let text = render_stderr(&report);
        assert!(text.starts_with("SP_BLOCKED v1 code=SP_SEQUENCE_COLLISION"));
        assert!(text.contains("lowest_free: 201"));
        assert!(text.contains("SP_CONFLICT_JSON="));
        assert!(text.contains("--no-verify"));
        assert!(text.contains("rerun_original_command"));
    }
}
