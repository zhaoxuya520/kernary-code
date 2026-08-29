#![forbid(unsafe_code)]

//! Rust 迁移使用的 TypeScript oracle fixtures。

use std::error::Error;
use std::fmt::{Display, Formatter};
use std::sync::atomic::{AtomicU64, Ordering};

use harness_types::{Clock, IdGenerator};
use serde::Deserialize;
use sha2::{Digest, Sha256};

const MISSION: &str =
    include_str!("../../../fixtures/ts-oracle/mission-parallel-approval-join.v1.json");
const MISSION_COMMANDS: &str =
    include_str!("../../../fixtures/ts-oracle/mission-command-decisions.v1.json");
const CONTEXT: &str =
    include_str!("../../../fixtures/ts-oracle/context-budget-cache-compaction.v1.json");
const MEMORY: &str = include_str!("../../../fixtures/ts-oracle/memory-vector-dual-path.v1.json");
const PERMISSION: &str =
    include_str!("../../../fixtures/ts-oracle/permission-path-and-grants.v1.json");
const MANIFEST: &str = include_str!("../../../fixtures/ts-oracle/fixture-manifest.v1.json");

const FIXTURES: &[(&str, &str)] = &[
    ("context-budget-cache-compaction.v1.json", CONTEXT),
    ("memory-vector-dual-path.v1.json", MEMORY),
    ("mission-command-decisions.v1.json", MISSION_COMMANDS),
    ("mission-parallel-approval-join.v1.json", MISSION),
    ("permission-path-and-grants.v1.json", PERMISSION),
];

/// Fixture 完整性错误。
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum FixtureError {
    InvalidManifest(String),
    MissingManifestEntry(String),
    HashMismatch {
        name: String,
        expected: String,
        actual: String,
    },
}

impl Display for FixtureError {
    fn fmt(&self, formatter: &mut Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::InvalidManifest(message) => {
                write!(formatter, "invalid-fixture-manifest: {message}")
            }
            Self::MissingManifestEntry(name) => {
                write!(formatter, "missing-fixture-manifest-entry: {name}")
            }
            Self::HashMismatch {
                name,
                expected,
                actual,
            } => write!(
                formatter,
                "fixture-hash-mismatch: {name} expected={expected} actual={actual}"
            ),
        }
    }
}

impl Error for FixtureError {}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct FixtureManifest {
    schema_version: u32,
    source: String,
    generated_by: String,
    fixtures: Vec<FixtureManifestEntry>,
}

#[derive(Debug, Deserialize)]
struct FixtureManifestEntry {
    name: String,
    sha256: String,
}

/// 返回 Mission oracle 原文。
#[must_use]
pub fn mission_fixture() -> &'static str {
    MISSION
}

/// 返回 Mission Command/Decision oracle 原文。
#[must_use]
pub fn mission_command_fixture() -> &'static str {
    MISSION_COMMANDS
}

/// 返回 Context oracle 原文。
#[must_use]
pub fn context_fixture() -> &'static str {
    CONTEXT
}

/// 返回 Memory oracle 原文。
#[must_use]
pub fn memory_fixture() -> &'static str {
    MEMORY
}

/// 返回 Permission oracle 原文。
#[must_use]
pub fn permission_fixture() -> &'static str {
    PERMISSION
}

/// 在 Rust 测试进程中再次验证 TypeScript manifest 的 SHA-256。
pub fn verify_ts_oracle_manifest() -> Result<(), FixtureError> {
    let manifest: FixtureManifest = serde_json::from_str(MANIFEST)
        .map_err(|error| FixtureError::InvalidManifest(error.to_string()))?;
    if manifest.schema_version != 1 {
        return Err(FixtureError::InvalidManifest(format!(
            "unsupported schema version {}",
            manifest.schema_version
        )));
    }
    if manifest.source != "TypeScript D0 oracle"
        || manifest.generated_by != "scripts/export-ts-oracle.ts"
    {
        return Err(FixtureError::InvalidManifest(
            "unexpected source or generator".to_owned(),
        ));
    }

    for (name, content) in FIXTURES {
        let expected = manifest
            .fixtures
            .iter()
            .find(|entry| entry.name == *name)
            .ok_or_else(|| FixtureError::MissingManifestEntry((*name).to_owned()))?;
        let actual = format!("{:x}", Sha256::digest(content.as_bytes()));
        if actual != expected.sha256 {
            return Err(FixtureError::HashMismatch {
                name: (*name).to_owned(),
                expected: expected.sha256.clone(),
                actual,
            });
        }
    }
    Ok(())
}

/// 测试使用的固定 UTC 毫秒时钟。
#[derive(Clone, Copy, Debug)]
pub struct FixedClock {
    now_unix_millis: i64,
}

impl FixedClock {
    #[must_use]
    pub const fn new(now_unix_millis: i64) -> Self {
        Self { now_unix_millis }
    }
}

impl Clock for FixedClock {
    fn now_unix_millis(&self) -> i64 {
        self.now_unix_millis
    }
}

/// 线程安全且确定性的测试 ID 序列。
#[derive(Debug, Default)]
pub struct SequenceIdGenerator {
    next: AtomicU64,
}

impl SequenceIdGenerator {
    #[must_use]
    pub const fn starting_at(next: u64) -> Self {
        Self {
            next: AtomicU64::new(next),
        }
    }
}

impl IdGenerator for SequenceIdGenerator {
    fn next_id(&self, prefix: &str) -> String {
        let value = self.next.fetch_add(1, Ordering::SeqCst);
        format!("{prefix}:{value}")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn manifest_matches_embedded_fixtures() {
        verify_ts_oracle_manifest().expect("内嵌 fixture 哈希必须匹配 manifest");
    }

    #[test]
    fn every_fixture_is_valid_json() {
        for (name, content) in FIXTURES {
            serde_json::from_str::<serde_json::Value>(content)
                .unwrap_or_else(|error| panic!("{name} 不是合法 JSON: {error}"));
        }
    }

    #[test]
    fn fixed_clock_and_sequence_ids_are_deterministic() {
        let clock = FixedClock::new(42);
        assert_eq!(clock.now_unix_millis(), 42);
        let ids = SequenceIdGenerator::starting_at(7);
        assert_eq!(ids.next_id("goal"), "goal:7");
        assert_eq!(ids.next_id("goal"), "goal:8");
    }
}
