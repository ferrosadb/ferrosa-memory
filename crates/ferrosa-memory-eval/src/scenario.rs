use serde::{Deserialize, Serialize};
use serde_json::Value;
use sha2::{Digest, Sha256};

use crate::memory_quality::EvidenceGroundTruth;
use std::collections::HashMap;
use std::path::Path;

/// A single evaluation scenario loaded from TOML.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct EvalScenario {
    pub scenario: ScenarioMeta,
    pub steps: Vec<EvalStep>,
    #[serde(default)]
    pub grading: GradingConfig,
    #[serde(default)]
    pub retrieval_ground_truth: Option<EvidenceGroundTruth>,
    #[serde(default)]
    pub dikw: Option<DikwConfig>,
    #[serde(default)]
    pub semantic: Option<SemanticConfig>,
}

/// Scenario metadata.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ScenarioMeta {
    pub id: String,
    pub name: String,
    #[serde(default)]
    pub description: String,
    #[serde(default = "default_level")]
    pub level: u8,
    #[serde(default)]
    pub dikw_transition: Option<String>,
    #[serde(default)]
    pub tags: Vec<String>,
    #[serde(default = "default_timeout_ms")]
    pub timeout_ms: u64,
}

fn default_level() -> u8 {
    1
}

fn default_timeout_ms() -> u64 {
    30_000
}

/// A single step in an evaluation scenario.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct EvalStep {
    pub tool: String,
    #[serde(default)]
    pub arguments: HashMap<String, Value>,
    #[serde(default)]
    pub expect_in_response: Vec<String>,
    #[serde(default)]
    pub expect_action: Option<String>,
    #[serde(default)]
    pub expect_entity_name: Option<String>,
}

/// Configuration for which grading methods to apply.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct GradingConfig {
    #[serde(default)]
    pub methods: Vec<String>,
    #[serde(default)]
    pub claim_rubric: Option<ClaimRubricConfig>,
    #[serde(default)]
    pub llm_judge: Option<LlmJudgeConfig>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ClaimRubricConfig {
    pub claims: Vec<String>,
    #[serde(default = "default_threshold")]
    pub passing_threshold: f64,
}

fn default_threshold() -> f64 {
    0.75
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct LlmJudgeConfig {
    pub rubric: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DikwConfig {
    #[serde(default)]
    pub expect_entity_count_gte: Option<usize>,
    #[serde(default)]
    pub expect_edge_types: Vec<String>,
    #[serde(default)]
    pub expect_temporal_chain: Option<bool>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SemanticConfig {
    #[serde(default)]
    pub expect_type_consistency: Option<bool>,
    #[serde(default)]
    pub expect_dedup_on_update: Option<bool>,
}

/// Recorded trace of a tool call made during scenario execution.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ToolCallTrace {
    pub tool: String,
    pub arguments: HashMap<String, Value>,
    pub response: Value,
    pub latency_ms: u64,
    pub success: bool,
}

// ---------------------------------------------------------------------------
// T-039: Scenario Manifest (SHA-256 Checksums)
// ---------------------------------------------------------------------------

/// A single entry in the scenario manifest.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct ManifestEntry {
    /// Relative file path from the scenario directory.
    pub file: String,
    /// SHA-256 hex digest of the file contents.
    pub sha256: String,
}

/// The complete scenario manifest — checksums for all scenario files.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct ScenarioManifest {
    pub entries: Vec<ManifestEntry>,
}

impl ScenarioManifest {
    /// Compute a manifest by walking the scenario directory and hashing all
    /// `.toml` and `.json` files.
    pub fn compute(scenario_dir: &Path) -> Result<Self, std::io::Error> {
        let mut entries = Vec::new();
        Self::walk_dir(scenario_dir, scenario_dir, &mut entries)?;
        entries.sort_by(|a, b| a.file.cmp(&b.file));
        Ok(Self { entries })
    }

    fn walk_dir(
        base: &Path,
        dir: &Path,
        entries: &mut Vec<ManifestEntry>,
    ) -> Result<(), std::io::Error> {
        if !dir.is_dir() {
            return Ok(());
        }

        let mut dir_entries: Vec<_> = std::fs::read_dir(dir)?.filter_map(|e| e.ok()).collect();
        dir_entries.sort_by_key(|e| e.file_name());

        for entry in dir_entries {
            let path = entry.path();
            if path.is_dir() {
                Self::walk_dir(base, &path, entries)?;
            } else if let Some(ext) = path.extension()
                && (ext == "toml" || ext == "json")
            {
                let contents = std::fs::read(&path)?;
                let hash = Sha256::digest(&contents);
                let relative = path
                    .strip_prefix(base)
                    .unwrap_or(&path)
                    .to_string_lossy()
                    .to_string();
                entries.push(ManifestEntry {
                    file: relative,
                    sha256: hex::encode(hash),
                });
            }
        }
        Ok(())
    }

    /// Verify this manifest against an expected manifest loaded from JSON.
    /// Returns a list of mismatched files (empty if all match).
    pub fn verify_against(&self, expected: &ScenarioManifest) -> Vec<ManifestMismatch> {
        let mut mismatches = Vec::new();

        let expected_map: HashMap<&str, &str> = expected
            .entries
            .iter()
            .map(|e| (e.file.as_str(), e.sha256.as_str()))
            .collect();

        let actual_map: HashMap<&str, &str> = self
            .entries
            .iter()
            .map(|e| (e.file.as_str(), e.sha256.as_str()))
            .collect();

        // Check for modified or missing files
        for entry in &expected.entries {
            match actual_map.get(entry.file.as_str()) {
                Some(actual_hash) if *actual_hash != entry.sha256.as_str() => {
                    mismatches.push(ManifestMismatch {
                        file: entry.file.clone(),
                        kind: MismatchKind::Modified {
                            expected: entry.sha256.clone(),
                            actual: actual_hash.to_string(),
                        },
                    });
                }
                None => {
                    mismatches.push(ManifestMismatch {
                        file: entry.file.clone(),
                        kind: MismatchKind::Missing,
                    });
                }
                _ => {} // matches
            }
        }

        // Check for new files not in expected
        for entry in &self.entries {
            if !expected_map.contains_key(entry.file.as_str()) {
                mismatches.push(ManifestMismatch {
                    file: entry.file.clone(),
                    kind: MismatchKind::Added,
                });
            }
        }

        mismatches
    }

    /// Load a manifest from a JSON file.
    pub fn load_from_file(path: &Path) -> Result<Self, std::io::Error> {
        let contents = std::fs::read_to_string(path)?;
        serde_json::from_str(&contents)
            .map_err(|e| std::io::Error::new(std::io::ErrorKind::InvalidData, e.to_string()))
    }

    /// Save this manifest to a JSON file.
    pub fn save_to_file(&self, path: &Path) -> Result<(), std::io::Error> {
        let json = serde_json::to_string_pretty(self)
            .map_err(|e| std::io::Error::new(std::io::ErrorKind::InvalidData, e.to_string()))?;
        std::fs::write(path, json)
    }
}

/// A mismatch between expected and actual manifest entries.
#[derive(Debug, Clone, PartialEq)]
pub struct ManifestMismatch {
    pub file: String,
    pub kind: MismatchKind,
}

/// The kind of manifest mismatch.
#[derive(Debug, Clone, PartialEq)]
pub enum MismatchKind {
    Modified { expected: String, actual: String },
    Missing,
    Added,
}

// ===========================================================================
// Tests
// ===========================================================================

#[cfg(test)]
mod tests {
    use super::*;

    // -------------------------------------------------------------------
    // T-039: Scenario manifest tests
    // -------------------------------------------------------------------

    #[test]
    fn manifest_computes_hashes_for_toml_files() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(
            dir.path().join("test.toml"),
            "[scenario]\nid = \"test\"\nname = \"Test\"\n",
        )
        .unwrap();

        let manifest = ScenarioManifest::compute(dir.path()).unwrap();
        assert_eq!(manifest.entries.len(), 1);
        assert_eq!(manifest.entries[0].file, "test.toml");
        assert_eq!(
            manifest.entries[0].sha256.len(),
            64,
            "SHA-256 hex = 64 chars"
        );
    }

    #[test]
    fn manifest_computes_hashes_for_json_files() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(
            dir.path().join("ground_truth.json"),
            r#"{"expected": true}"#,
        )
        .unwrap();

        let manifest = ScenarioManifest::compute(dir.path()).unwrap();
        assert_eq!(manifest.entries.len(), 1);
        assert_eq!(manifest.entries[0].file, "ground_truth.json");
    }

    #[test]
    fn manifest_ignores_non_toml_non_json_files() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("readme.md"), "# Not hashed").unwrap();
        std::fs::write(dir.path().join("data.csv"), "a,b,c").unwrap();

        let manifest = ScenarioManifest::compute(dir.path()).unwrap();
        assert!(manifest.entries.is_empty());
    }

    #[test]
    fn manifest_recurses_into_subdirectories() {
        let dir = tempfile::tempdir().unwrap();
        let sub = dir.path().join("level1");
        std::fs::create_dir(&sub).unwrap();
        std::fs::write(sub.join("scenario.toml"), "[scenario]\nid = \"sub\"").unwrap();

        let manifest = ScenarioManifest::compute(dir.path()).unwrap();
        assert_eq!(manifest.entries.len(), 1);
        assert_eq!(manifest.entries[0].file, "level1/scenario.toml");
    }

    #[test]
    fn manifest_entries_sorted_by_filename() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("z.toml"), "z").unwrap();
        std::fs::write(dir.path().join("a.toml"), "a").unwrap();
        std::fs::write(dir.path().join("m.json"), "m").unwrap();

        let manifest = ScenarioManifest::compute(dir.path()).unwrap();
        assert_eq!(manifest.entries.len(), 3);
        assert_eq!(manifest.entries[0].file, "a.toml");
        assert_eq!(manifest.entries[1].file, "m.json");
        assert_eq!(manifest.entries[2].file, "z.toml");
    }

    #[test]
    fn manifest_detects_file_modification() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("test.toml"), "original").unwrap();

        let manifest_before = ScenarioManifest::compute(dir.path()).unwrap();

        // Modify the file
        std::fs::write(dir.path().join("test.toml"), "modified").unwrap();

        let manifest_after = ScenarioManifest::compute(dir.path()).unwrap();

        assert_ne!(
            manifest_before.entries[0].sha256, manifest_after.entries[0].sha256,
            "hash should change when file is modified"
        );
    }

    #[test]
    fn manifest_verify_detects_modified_file() {
        let expected = ScenarioManifest {
            entries: vec![ManifestEntry {
                file: "test.toml".to_string(),
                sha256: "aaa".to_string(),
            }],
        };
        let actual = ScenarioManifest {
            entries: vec![ManifestEntry {
                file: "test.toml".to_string(),
                sha256: "bbb".to_string(),
            }],
        };

        let mismatches = actual.verify_against(&expected);
        assert_eq!(mismatches.len(), 1);
        assert_eq!(mismatches[0].file, "test.toml");
        match &mismatches[0].kind {
            MismatchKind::Modified {
                expected: e,
                actual: a,
            } => {
                assert_eq!(e, "aaa");
                assert_eq!(a, "bbb");
            }
            other => panic!("expected Modified, got: {other:?}"),
        }
    }

    #[test]
    fn manifest_verify_detects_missing_file() {
        let expected = ScenarioManifest {
            entries: vec![ManifestEntry {
                file: "deleted.toml".to_string(),
                sha256: "aaa".to_string(),
            }],
        };
        let actual = ScenarioManifest { entries: vec![] };

        let mismatches = actual.verify_against(&expected);
        assert_eq!(mismatches.len(), 1);
        assert_eq!(mismatches[0].kind, MismatchKind::Missing);
    }

    #[test]
    fn manifest_verify_detects_added_file() {
        let expected = ScenarioManifest { entries: vec![] };
        let actual = ScenarioManifest {
            entries: vec![ManifestEntry {
                file: "new.toml".to_string(),
                sha256: "ccc".to_string(),
            }],
        };

        let mismatches = actual.verify_against(&expected);
        assert_eq!(mismatches.len(), 1);
        assert_eq!(mismatches[0].kind, MismatchKind::Added);
    }

    #[test]
    fn manifest_verify_no_mismatches_when_identical() {
        let entries = vec![ManifestEntry {
            file: "test.toml".to_string(),
            sha256: "same".to_string(),
        }];
        let expected = ScenarioManifest {
            entries: entries.clone(),
        };
        let actual = ScenarioManifest { entries };

        let mismatches = actual.verify_against(&expected);
        assert!(mismatches.is_empty());
    }

    #[test]
    fn manifest_round_trips_through_json() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("test.toml"), "content").unwrap();

        let manifest = ScenarioManifest::compute(dir.path()).unwrap();
        let json_path = dir.path().join("manifest.json");
        manifest.save_to_file(&json_path).unwrap();

        let loaded = ScenarioManifest::load_from_file(&json_path).unwrap();
        assert_eq!(manifest, loaded);
    }

    #[test]
    fn manifest_serializes_to_expected_json_structure() {
        let manifest = ScenarioManifest {
            entries: vec![ManifestEntry {
                file: "level1/memo_cache.toml".to_string(),
                sha256: "abc123".to_string(),
            }],
        };

        let json = serde_json::to_value(&manifest).unwrap();
        let entry = &json["entries"][0];
        assert_eq!(entry["file"], "level1/memo_cache.toml");
        assert_eq!(entry["sha256"], "abc123");
    }
}
