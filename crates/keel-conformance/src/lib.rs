//! Shared pieces of the conformance harness (`conformance/scenarios/*.json`):
//! the typed scenario model, the scripted effect, and the subset matcher.
//! The sync (stub) and async (real core) harnesses drive their cores
//! differently but interpret scenarios through this one model, so the
//! scenario format cannot drift between them.
//!
//! Format and normative semantics: `conformance/README.md`.

use std::path::{Path, PathBuf};

use keel_core_api::{AttemptResult, ENVELOPE_VERSION, Request};
use serde::Deserialize;
use serde_json::Value;

/// One scenario file.
#[derive(Debug, Clone, Deserialize)]
pub struct Scenario {
    pub name: String,
    #[serde(default)]
    pub description: String,
    /// keel.toml as JSON, per contracts/policy.schema.json.
    pub policy: Value,
    /// When set, `configure` must fail with exactly this code and no steps run.
    #[serde(default)]
    pub expect_configure_error: Option<String>,
    #[serde(default)]
    pub steps: Vec<Step>,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(untagged)]
pub enum Step {
    Call { call: CallStep },
    Advance { advance_ms: u64 },
    ReportExpect { report_expect: Value },
}

#[derive(Debug, Clone, Deserialize)]
pub struct CallStep {
    pub target: String,
    #[serde(default)]
    pub request: RequestSpec,
    /// `AttemptResult` envelopes, one per attempt, in order.
    #[serde(default)]
    pub effect: Vec<AttemptResult>,
    /// Subset-matched against the `Outcome` envelope.
    #[serde(default)]
    pub expect: Value,
}

#[derive(Debug, Clone, Default, Deserialize)]
pub struct RequestSpec {
    #[serde(default)]
    pub op: Option<String>,
    #[serde(default = "default_idempotent")]
    pub idempotent: bool,
    #[serde(default)]
    pub args_hash: Option<String>,
}

fn default_idempotent() -> bool {
    true
}

impl CallStep {
    /// The `Request` envelope this call submits (op defaults to the target).
    #[must_use]
    pub fn request(&self) -> Request {
        Request {
            v: ENVELOPE_VERSION,
            target: self.target.clone(),
            op: self
                .request
                .op
                .clone()
                .unwrap_or_else(|| self.target.clone()),
            idempotent: self.request.idempotent,
            args_hash: self.request.args_hash.clone(),
        }
    }
}

/// A scripted effect: attempt N returns the Nth scripted `AttemptResult`.
/// Consuming past the script is a harness bug and panics with context.
#[derive(Debug)]
pub struct ScriptedEffect<'a> {
    label: String,
    script: &'a [AttemptResult],
    consumed: usize,
}

impl<'a> ScriptedEffect<'a> {
    #[must_use]
    pub fn new(label: impl Into<String>, script: &'a [AttemptResult]) -> Self {
        Self {
            label: label.into(),
            script,
            consumed: 0,
        }
    }

    /// The result for this attempt.
    ///
    /// # Panics
    /// When the core makes more attempts than the scenario scripted.
    pub fn next(&mut self, attempt: u32) -> AttemptResult {
        assert!(
            self.consumed < self.script.len(),
            "{}: effect script exhausted (attempt {attempt}, scripted {})",
            self.label,
            self.script.len()
        );
        let result = self.script[self.consumed].clone();
        self.consumed += 1;
        result
    }

    /// A mismatch message unless every scripted attempt was consumed.
    #[must_use]
    pub fn leftover(&self) -> Option<String> {
        (self.consumed != self.script.len()).then(|| {
            format!(
                "{}: effect script not fully consumed ({}/{} attempts used)",
                self.label,
                self.consumed,
                self.script.len()
            )
        })
    }
}

/// Subset match: objects require listed keys to match recursively; arrays
/// must match exactly; scalars must be equal. Appends mismatch descriptions.
pub fn subset_mismatches(actual: &Value, expected: &Value, path: &str, out: &mut Vec<String>) {
    match expected {
        Value::Object(exp) => match actual {
            Value::Object(act) => {
                for (key, value) in exp {
                    match act.get(key) {
                        None => out.push(format!("{path}.{key}: missing (expected {value})")),
                        Some(a) => subset_mismatches(a, value, &format!("{path}.{key}"), out),
                    }
                }
            }
            other => out.push(format!("{path}: expected object, got {other}")),
        },
        Value::Array(exp) => match actual {
            Value::Array(act) if act.len() == exp.len() => {
                for (i, (a, e)) in act.iter().zip(exp).enumerate() {
                    subset_mismatches(a, e, &format!("{path}[{i}]"), out);
                }
            }
            other => out.push(format!("{path}: expected {expected}, got {other}")),
        },
        scalar => {
            if actual != scalar {
                out.push(format!("{path}: expected {scalar}, got {actual}"));
            }
        }
    }
}

/// Loads every `*.json` scenario in the directory, sorted by file name.
///
/// # Panics
/// On unreadable directories/files or scenario parse errors — a broken
/// corpus should fail the harness loudly, not skip silently.
#[must_use]
pub fn load_dir(dir: &Path) -> Vec<(PathBuf, Scenario)> {
    let mut files: Vec<PathBuf> = std::fs::read_dir(dir)
        .unwrap_or_else(|e| panic!("cannot read {}: {e}", dir.display()))
        .map(|entry| entry.expect("readable dir entry").path())
        .filter(|p| p.extension().is_some_and(|ext| ext == "json"))
        .collect();
    files.sort();
    assert!(!files.is_empty(), "no scenarios found in {}", dir.display());
    files
        .into_iter()
        .map(|path| {
            let text = std::fs::read_to_string(&path)
                .unwrap_or_else(|e| panic!("cannot read {}: {e}", path.display()));
            let scenario: Scenario = serde_json::from_str(&text)
                .unwrap_or_else(|e| panic!("bad scenario {}: {e}", path.display()));
            (path, scenario)
        })
        .collect()
}

/// The canonical scenarios directory, resolved from a crate manifest dir.
#[must_use]
pub fn scenarios_dir(manifest_dir: &str) -> PathBuf {
    PathBuf::from(manifest_dir).join("../../conformance/scenarios")
}
