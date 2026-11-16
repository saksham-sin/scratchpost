use crate::git_engine;
use crate::protocol::{now_ms, ClaimState, SequenceClaim};
use quick_xml::events::Event;
use quick_xml::Reader;
use std::collections::HashSet;
use std::path::Path;

#[derive(Debug, Clone)]
pub struct Observation {
    pub rule: String,
    pub namespace: String,
    pub key: String,
    pub numeric: Option<u64>,
    pub path: String,
    pub line: u64,
}

pub trait SequenceRule: Send + Sync {
    fn id(&self) -> &'static str;
    fn matches(&self, path: &str) -> bool;
    fn scan(&self, repo: &str, path: &str, content: &str) -> Vec<Observation>;
}

pub struct RuleEngine {
    rules: Vec<Box<dyn SequenceRule>>,
}

impl Default for RuleEngine {
    fn default() -> Self {
        Self {
            rules: vec![Box::new(LiquibaseRule), Box::new(FlywayRule)],
        }
    }
}

/// Rules selected for one path: (path, [rule, ...]).
type SelectedRules<'a> = (String, Vec<&'a Box<dyn SequenceRule>>);
/// Rules selected for one tree: (tree, [(path, [rule, ...]), ...]).
type RulePlan<'a> = (String, Vec<SelectedRules<'a>>);

impl RuleEngine {
    /// Scans several trees in one pass, sharing a single `git cat-file --batch` process.
    pub fn scan_trees(
        &self,
        repo_path: &Path,
        repo_name: &str,
        trees: &[(&str, &[String])],
    ) -> Vec<Vec<Observation>> {
        let mut specs: Vec<(String, String)> = Vec::new();
        let mut plan: Vec<RulePlan<'_>> = Vec::with_capacity(trees.len());
        for (tree, paths) in trees {
            let mut selected: Vec<SelectedRules<'_>> = Vec::new();
            for path in paths.iter() {
                let lower = path.to_ascii_lowercase();
                let matching: Vec<&Box<dyn SequenceRule>> = self
                    .rules
                    .iter()
                    .filter(|rule| rule.matches(&lower))
                    .collect();
                if !matching.is_empty() {
                    selected.push((path.clone(), matching));
                    specs.push((tree.to_string(), path.clone()));
                }
            }
            plan.push((tree.to_string(), selected));
        }
        let blobs = git_engine::read_blobs_multi(repo_path, &specs);
        let mut results: Vec<Vec<Observation>> = Vec::with_capacity(trees.len());
        for (tree, selected) in plan {
            let mut observations = Vec::new();
            for (path, matching) in selected {
                let Some(bytes) = blobs.get(&(tree.clone(), path.clone())) else {
                    continue;
                };
                let content = String::from_utf8_lossy(bytes);
                for rule in matching {
                    observations.extend(rule.scan(repo_name, &path, &content));
                }
            }
            results.push(observations);
        }
        results
    }
}

pub fn newly_introduced(candidate: &[Observation], base: &[Observation]) -> Vec<Observation> {
    let base_keys: HashSet<(&str, &str, &str)> = base
        .iter()
        .map(|o| (o.rule.as_str(), o.namespace.as_str(), o.key.as_str()))
        .collect();
    let mut seen = HashSet::new();
    candidate
        .iter()
        .filter(|o| {
            let key = (o.rule.as_str(), o.namespace.as_str(), o.key.as_str());
            !base_keys.contains(&key) && seen.insert(key)
        })
        .cloned()
        .collect()
}

pub fn find_collision<'a>(
    observations: &[Observation],
    claims: &'a [SequenceClaim],
    agent: &str,
) -> Option<(&'a SequenceClaim, Observation)> {
    for observation in observations {
        for claim in claims {
            if claim.rule == observation.rule
                && claim.namespace == observation.namespace
                && claim.key == observation.key
                && claim.agent != agent
                && (claim.state == ClaimState::Committed
                    || (claim.state == ClaimState::Reserved && claim.expires_ms > now_ms()))
            {
                return Some((claim, observation.clone()));
            }
        }
    }
    None
}

pub fn lowest_free(
    observations: &[Observation],
    claims: &[SequenceClaim],
    namespace: &str,
    requested: &Observation,
) -> String {
    let mut used = HashSet::new();
    for observation in observations {
        if observation.namespace == namespace {
            if let Some(n) = observation.numeric {
                used.insert(n);
            }
        }
    }
    for claim in claims {
        if claim.namespace == namespace
            && (claim.state == ClaimState::Committed
                || (claim.state == ClaimState::Reserved && claim.expires_ms > now_ms()))
        {
            if let Some(n) = claim.numeric {
                used.insert(n);
            }
        }
    }
    if let Some(mut current) = requested.numeric {
        current += 1;
        while used.contains(&current) {
            current += 1;
        }
        current.to_string()
    } else {
        let mut n = 1u64;
        while used.contains(&n) {
            n += 1;
        }
        if let Some((pos, _)) = requested
            .key
            .char_indices()
            .rev()
            .find(|(_, c)| c.is_ascii_digit())
        {
            let prefix = &requested.key[..=pos];
            format!("{prefix}{n}")
        } else {
            format!("{}-next", requested.key)
        }
    }
}

pub struct LiquibaseRule;

impl SequenceRule for LiquibaseRule {
    fn id(&self) -> &'static str {
        "liquibase.changeset_id"
    }

    fn matches(&self, path: &str) -> bool {
        path.ends_with(".xml") && path.contains("changelog")
    }

    fn scan(&self, repo: &str, path: &str, content: &str) -> Vec<Observation> {
        let mut observations = Vec::new();
        let mut reader = Reader::from_str(content);
        reader.config_mut().trim_text(false);
        loop {
            match reader.read_event() {
                Ok(Event::Start(event)) | Ok(Event::Empty(event)) => {
                    if event.local_name().as_ref() == b"changeSet" {
                        let mut id = None;
                        for attr in event.attributes().flatten() {
                            if attr.key.local_name().as_ref() == b"id" {
                                if let Ok(value) = attr.unescape_value() {
                                    id = Some(value.to_string());
                                }
                            }
                        }
                        if let Some(key) = id {
                            let position = reader.buffer_position() as usize;
                            let line = content[..position.min(content.len())]
                                .bytes()
                                .filter(|b| *b == b'\n')
                                .count() as u64
                                + 1;
                            observations.push(Observation {
                                rule: self.id().to_string(),
                                namespace: format!("{repo}:{path}"),
                                key: key.clone(),
                                numeric: key.parse::<u64>().ok(),
                                path: path.to_string(),
                                line,
                            });
                        }
                    }
                }
                Ok(Event::Eof) => break,
                Err(_) => break,
                _ => {}
            }
        }
        observations
    }
}

pub struct FlywayRule;

impl SequenceRule for FlywayRule {
    fn id(&self) -> &'static str {
        "flyway.versioned_migration"
    }

    fn matches(&self, path: &str) -> bool {
        let name = path.rsplit('/').next().unwrap_or(path);
        name.starts_with('v') && name.contains("__") && name.ends_with(".sql")
    }

    fn scan(&self, repo: &str, path: &str, _content: &str) -> Vec<Observation> {
        let name = path.rsplit('/').next().unwrap_or(path);
        let Some(version) = parse_flyway_version(name) else {
            return Vec::new();
        };
        let namespace = format!(
            "{repo}:{}",
            path.rsplit_once('/')
                .map(|(parent, _)| parent)
                .filter(|parent| !parent.is_empty())
                .unwrap_or(".")
        );
        vec![Observation {
            rule: self.id().to_string(),
            namespace,
            key: version.clone(),
            numeric: version.parse::<u64>().ok(),
            path: path.to_string(),
            line: 1,
        }]
    }
}

fn parse_flyway_version(name: &str) -> Option<String> {
    let lower = name.to_ascii_lowercase();
    if !lower.starts_with('v') || !lower.ends_with(".sql") {
        return None;
    }
    let without_prefix = &name[1..name.len() - 4];
    let version = without_prefix.split("__").next().unwrap_or("").trim();
    if version.is_empty()
        || !version
            .bytes()
            .all(|b| b.is_ascii_digit() || b == b'.' || b == b'_')
    {
        return None;
    }
    Some(version.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_liquibase_changesets() {
        let xml = r#"<?xml version="1.0"?>
<databaseChangeLog>
  <changeSet id="200" author="agent_1">
    <createTable tableName="x"/>
  </changeSet>
  <changeSet id="201" author="agent_2"/>
</databaseChangeLog>"#;
        let engine = RuleEngine::default();
        let observations = engine.rules[0].scan("auth-service", "dbchangelog.xml", xml);
        assert_eq!(observations.len(), 2);
        assert_eq!(observations[0].key, "200");
        assert_eq!(observations[0].numeric, Some(200));
        assert_eq!(observations[1].line, 6);
    }

    #[test]
    fn parses_flyway_versions() {
        let rule = FlywayRule;
        let observations = rule.scan("auth-service", "db/migration/V200__init.sql", "");
        assert_eq!(observations.len(), 1);
        assert_eq!(observations[0].key, "200");
        assert_eq!(observations[0].namespace, "auth-service:db/migration");
    }

    #[test]
    fn finds_new_sequence_keys() {
        let base = vec![Observation {
            rule: "liquibase.changeset_id".to_string(),
            namespace: "auth-service:dbchangelog.xml".to_string(),
            key: "100".to_string(),
            numeric: Some(100),
            path: "dbchangelog.xml".to_string(),
            line: 1,
        }];
        let candidate = vec![
            base[0].clone(),
            Observation {
                rule: "liquibase.changeset_id".to_string(),
                namespace: "auth-service:dbchangelog.xml".to_string(),
                key: "200".to_string(),
                numeric: Some(200),
                path: "dbchangelog.xml".to_string(),
                line: 5,
            },
        ];
        let new = newly_introduced(&candidate, &base);
        assert_eq!(new.len(), 1);
        assert_eq!(new[0].key, "200");
        assert_eq!(
            lowest_free(&candidate, &[], "auth-service:dbchangelog.xml", &new[0]),
            "201"
        );
    }
}
