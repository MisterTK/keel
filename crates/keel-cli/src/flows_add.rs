//! `keel flows add <entrypoint>` — one-command durability designation
//! (dx-spec §1 Level 2: "Durability, still config-only").
//!
//! Appends the entrypoint to `[flows] entrypoints` in `keel.toml` through the
//! policy-diff engine ([`crate::diff`]), so the edit is *surgical*: user
//! comments and formatting outside the touched key survive byte-for-byte, the
//! `[flows]` table is created when absent, and the change is also emitted as
//! an applyable unified diff + structured hunks (dx-spec §5, diffs as the
//! lingua franca). Re-running with the same entrypoint is an exact no-op.
//!
//! The one formatting caveat: the `entrypoints` array itself is re-rendered as
//! a single-line array when appended to (the diff engine sets the whole
//! value); everything outside that one line is untouched.
//!
//! Ergonomics: the bare forms `keel flows suggest` prints are accepted —
//! `pipeline.ingest:main` infers `py:`, `jobs/nightly.ts#run` infers `ts:` —
//! and explicit `py:` / `ts:` / `rs:` refs pass through. The final ref is
//! validated against the frozen `entrypointRef` grammar
//! (`contracts/policy.schema.json`: `^(py|ts|rs):[^\s]+$`) before anything is
//! written.

use std::path::Path;

use serde::Serialize;
use toml_edit::Value;

use crate::diff::{ChangeHunk, PolicyOp, PolicyPath, propose};
use crate::render::to_json;
use crate::{EXIT_USAGE, Rendered, evidence};

/// The machine twin of `keel flows add`.
#[derive(Debug, Serialize)]
struct AddReport {
    /// True when the entrypoint was appended (false on an idempotent re-run).
    added: bool,
    /// True when the entrypoint was already designated.
    already_designated: bool,
    /// Structured `{path, before, after}` hunks (empty on a no-op).
    changes: Vec<ChangeHunk>,
    /// The normalized full ref that was (or already is) designated.
    entrypoint: String,
    /// Unified diff `a/keel.toml` → `b/keel.toml` (empty on a no-op).
    patch: String,
    /// True when `keel.toml` was written (false under `--diff` or a no-op).
    written: bool,
}

/// `keel flows add <entrypoint> [--diff]` for `project`.
pub fn run(project: &Path, entrypoint: &str, diff_only: bool) -> Rendered {
    let full = match normalize_entrypoint(entrypoint) {
        Ok(f) => f,
        Err(e) => return usage_error(&e),
    };
    let toml_path = evidence::keel_toml(project);
    let current = if toml_path.exists() {
        match std::fs::read_to_string(&toml_path) {
            Ok(t) => Some(t),
            Err(e) => return usage_error(&format!("could not read {}: {e}", toml_path.display())),
        }
    } else {
        None
    };
    let existing = match existing_entrypoints(current.as_deref()) {
        Ok(list) => list,
        Err(e) => return usage_error(&e),
    };

    if existing.iter().any(|e| e == &full) {
        let human = format!(
            "keel \u{25b8} {full} is already designated in [flows] entrypoints \u{2014} nothing to do."
        );
        return Rendered::ok(
            human,
            to_json(&AddReport {
                added: false,
                already_designated: true,
                changes: Vec::new(),
                entrypoint: full,
                patch: String::new(),
                written: false,
            }),
        );
    }

    let mut array = toml_edit::Array::new();
    for e in &existing {
        array.push(e.as_str());
    }
    array.push(full.as_str());
    let ops = [PolicyOp::Set {
        path: PolicyPath::new(["flows", "entrypoints"]),
        value: Value::Array(array),
    }];
    let proposal = match propose(current.as_deref(), &ops) {
        Ok(p) => p,
        Err(e) => return usage_error(&e.to_string()),
    };

    let written = !diff_only;
    if written && let Err(e) = std::fs::write(&toml_path, &proposal.new_text) {
        return usage_error(&format!("could not write {}: {e}", toml_path.display()));
    }

    let human = if diff_only {
        format!(
            "keel \u{25b8} would designate {full} as a durable flow (keel.toml not written).\n\
             \napply with `git apply` (or `patch -p1`):\n\n{}",
            proposal.patch
        )
    } else {
        format!(
            "keel \u{25b8} designated {full} as a durable flow in keel.toml.\n  \
             Run it with `keel run <script>`; kill it mid-run and the next `keel run` resumes.\n  \
             Inspect with `keel flows` / `keel trace <flow>`."
        )
    };
    Rendered::ok(
        human,
        to_json(&AddReport {
            added: true,
            already_designated: false,
            changes: proposal.changes,
            entrypoint: full,
            patch: proposal.patch,
            written,
        }),
    )
}

/// Normalize a user-supplied ref to a full `py:`/`ts:`/`rs:` entrypoint, or
/// explain precisely why it cannot be one. Accepts the bare display forms
/// `keel flows suggest` prints and infers their language namespace.
fn normalize_entrypoint(raw: &str) -> Result<String, String> {
    let raw = raw.trim();
    if raw.is_empty() {
        return Err(no_parse(raw, "it is empty"));
    }
    if raw.chars().any(char::is_whitespace) {
        return Err(no_parse(raw, "it contains whitespace"));
    }
    for prefix in ["py:", "rs:", "ts:"] {
        if let Some(rest) = raw.strip_prefix(prefix) {
            if rest.is_empty() {
                return Err(no_parse(raw, "nothing follows the language prefix"));
            }
            return Ok(raw.to_owned());
        }
    }
    if is_python_shape(raw) {
        return Ok(format!("py:{raw}"));
    }
    if is_ts_shape(raw) {
        return Ok(format!("ts:{raw}"));
    }
    Err(no_parse(
        raw,
        "it is neither `module.path:function` (Python) nor `path/file.ts#function` (JS/TS)",
    ))
}

/// The KEEL-E001 message for an unusable ref: what / why / what-next.
fn no_parse(raw: &str, why: &str) -> String {
    format!(
        "{raw:?} is not a flow entrypoint ref: {why}. \
         Use `py:module.path:function`, `ts:path/file.ts#function`, or a bare form \
         from `keel flows suggest` (e.g. `pipeline.ingest:main`)."
    )
}

/// `module(.module)*:function` where every part is a Python identifier.
fn is_python_shape(s: &str) -> bool {
    let Some((module, function)) = s.rsplit_once(':') else {
        return false;
    };
    !module.is_empty()
        && module.split('.').all(is_py_ident)
        && is_py_ident(function)
        && !s.contains('#')
        && !s.contains('/')
}

fn is_py_ident(s: &str) -> bool {
    let mut chars = s.chars();
    chars
        .next()
        .is_some_and(|c| c.is_ascii_alphabetic() || c == '_')
        && chars.all(|c| c.is_ascii_alphanumeric() || c == '_')
}

/// `path/to/file.<js-ext>#function` — the JS/TS entrypoint shape.
fn is_ts_shape(s: &str) -> bool {
    let Some((file, function)) = s.rsplit_once('#') else {
        return false;
    };
    let has_js_ext = [".js", ".mjs", ".cjs", ".ts", ".mts", ".cts", ".jsx", ".tsx"]
        .iter()
        .any(|ext| file.ends_with(ext));
    has_js_ext && !function.is_empty()
}

/// The `[flows] entrypoints` strings already in the policy. `None` current
/// text means no file (empty list). A present-but-malformed `flows` table is a
/// hard error — silently overwriting a user's non-array value would destroy
/// their content.
///
/// Shared with `keel flows suggest`, which uses it (leniently — see
/// [`crate::flows_suggest`]) to mark candidates already designated as flows.
pub(crate) fn existing_entrypoints(current: Option<&str>) -> Result<Vec<String>, String> {
    let Some(text) = current else {
        return Ok(Vec::new());
    };
    let value: toml::Value = text
        .parse()
        .map_err(|e: toml::de::Error| format!("keel.toml is not valid TOML: {e}"))?;
    let Some(flows) = value.get("flows") else {
        return Ok(Vec::new());
    };
    let Some(flows) = flows.as_table() else {
        return Err(
            "keel.toml's `flows` key is not a table; fix it before adding entrypoints \
                    (expected `[flows]` with `entrypoints = [\"py:…\"]`)."
                .to_owned(),
        );
    };
    let Some(entrypoints) = flows.get("entrypoints") else {
        return Ok(Vec::new());
    };
    let Some(list) = entrypoints.as_array() else {
        return Err(
            "keel.toml's `flows.entrypoints` is not an array; fix it before adding entrypoints \
             (expected `entrypoints = [\"py:…\"]`)."
                .to_owned(),
        );
    };
    list.iter()
        .map(|v| {
            v.as_str().map(str::to_owned).ok_or_else(|| {
                "keel.toml's `flows.entrypoints` contains a non-string entry; fix it before \
                 adding entrypoints."
                    .to_owned()
            })
        })
        .collect()
}

/// A policy/usage failure (KEEL-E001, exit 2, stderr) — mirrors `keel init`.
fn usage_error(message: &str) -> Rendered {
    #[derive(Serialize)]
    struct ErrReport<'a> {
        code: &'static str,
        error: &'a str,
    }
    Rendered {
        human: format!("keel \u{25b8} KEEL-E001: {message}"),
        json: to_json(&ErrReport {
            code: "KEEL-E001",
            error: message,
        }),
        exit: EXIT_USAGE,
        to_stderr: true,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn project() -> tempfile::TempDir {
        tempfile::TempDir::new().unwrap()
    }

    fn read_toml(dir: &tempfile::TempDir) -> String {
        std::fs::read_to_string(dir.path().join("keel.toml")).unwrap()
    }

    // ---- normalization ----

    #[test]
    fn bare_python_shape_infers_py() {
        assert_eq!(
            normalize_entrypoint("pipeline.ingest:main").unwrap(),
            "py:pipeline.ingest:main"
        );
        assert_eq!(normalize_entrypoint("app:run").unwrap(), "py:app:run");
    }

    #[test]
    fn bare_ts_shape_infers_ts() {
        assert_eq!(
            normalize_entrypoint("jobs/nightly.ts#run").unwrap(),
            "ts:jobs/nightly.ts#run"
        );
        assert_eq!(
            normalize_entrypoint("app.mjs#fetchData").unwrap(),
            "ts:app.mjs#fetchData"
        );
    }

    #[test]
    fn explicit_prefixes_pass_through() {
        for full in ["py:pipeline.ingest:main", "ts:jobs/n.ts#run", "rs:crate::x"] {
            assert_eq!(normalize_entrypoint(full).unwrap(), full);
        }
    }

    #[test]
    fn garbage_refs_are_rejected_with_guidance() {
        for bad in ["", "  ", "just-a-name", "py:", "has space:fn", "file.txt#x"] {
            let err = normalize_entrypoint(bad).unwrap_err();
            assert!(err.contains("keel flows suggest"), "{err}");
        }
    }

    // ---- the write path ----

    #[test]
    fn creates_keel_toml_and_flows_table_when_absent() {
        let dir = project();
        let r = run(dir.path(), "pipeline.ingest:main", false);
        assert_eq!(r.exit, crate::EXIT_OK);
        assert_eq!(r.json["added"], true);
        assert_eq!(r.json["written"], true);
        assert_eq!(r.json["entrypoint"], "py:pipeline.ingest:main");
        let text = read_toml(&dir);
        assert_eq!(
            text,
            "[flows]\nentrypoints = [\"py:pipeline.ingest:main\"]\n"
        );
        // The creation patch is /dev/null-headed and applyable.
        let patch = r.json["patch"].as_str().unwrap();
        assert!(
            patch.starts_with("--- /dev/null\n+++ b/keel.toml\n"),
            "{patch}"
        );
        assert_eq!(crate::diff::apply_unified("", patch).unwrap(), text);
    }

    #[test]
    fn appends_to_existing_policy_preserving_user_bytes() {
        let dir = project();
        let existing = "\
# my tuning \u{2014} keep me

[target.\"api.example.com\"]        # seen in: app.py:4
timeout = \"9s\"                      # tuned by us
";
        std::fs::write(dir.path().join("keel.toml"), existing).unwrap();
        let r = run(dir.path(), "py:pipeline.ingest:main", false);
        assert_eq!(r.exit, crate::EXIT_OK);
        let text = read_toml(&dir);
        assert!(
            text.starts_with(existing),
            "untouched bytes survive: {text}"
        );
        assert!(text.contains("[flows]\nentrypoints = [\"py:pipeline.ingest:main\"]\n"));
        let patch = r.json["patch"].as_str().unwrap();
        assert_eq!(crate::diff::apply_unified(existing, patch).unwrap(), text);
        // Structured hunk names the flows table.
        assert_eq!(r.json["changes"][0]["path"], "flows.entrypoints");
    }

    #[test]
    fn appends_to_an_existing_entrypoints_array_in_order() {
        let dir = project();
        std::fs::write(
            dir.path().join("keel.toml"),
            "[flows]\nentrypoints = [\"py:a.b:c\"]\n",
        )
        .unwrap();
        let r = run(dir.path(), "jobs/nightly.ts#run", false);
        assert_eq!(r.exit, crate::EXIT_OK);
        assert_eq!(
            read_toml(&dir),
            "[flows]\nentrypoints = [\"py:a.b:c\", \"ts:jobs/nightly.ts#run\"]\n"
        );
    }

    #[test]
    fn rerun_is_an_exact_noop() {
        let dir = project();
        let r1 = run(dir.path(), "pipeline.ingest:main", false);
        assert_eq!(r1.json["added"], true);
        let before = read_toml(&dir);
        // Same ref, bare or prefixed: both are already designated.
        for spelling in ["pipeline.ingest:main", "py:pipeline.ingest:main"] {
            let r = run(dir.path(), spelling, false);
            assert_eq!(r.exit, crate::EXIT_OK);
            assert_eq!(r.json["added"], false);
            assert_eq!(r.json["already_designated"], true);
            assert_eq!(r.json["written"], false);
            assert_eq!(r.json["patch"], "");
        }
        assert_eq!(read_toml(&dir), before, "file byte-identical after re-runs");
    }

    #[test]
    fn diff_flag_previews_without_writing() {
        let dir = project();
        let r = run(dir.path(), "pipeline.ingest:main", true);
        assert_eq!(r.exit, crate::EXIT_OK);
        assert_eq!(r.json["written"], false);
        assert!(
            !dir.path().join("keel.toml").exists(),
            "--diff never writes"
        );
        assert!(r.human.contains("apply with `git apply`"));
        assert!(!r.json["patch"].as_str().unwrap().is_empty());
    }

    // ---- error paths ----

    #[test]
    fn invalid_toml_is_a_usage_error() {
        let dir = project();
        std::fs::write(dir.path().join("keel.toml"), "not [valid\n").unwrap();
        let r = run(dir.path(), "pipeline.ingest:main", false);
        assert_eq!(r.exit, EXIT_USAGE);
        assert!(r.to_stderr);
        assert!(r.human.contains("KEEL-E001"));
        assert!(r.human.contains("not valid TOML"));
    }

    #[test]
    fn non_array_entrypoints_is_refused_not_overwritten() {
        let dir = project();
        let original = "[flows]\nentrypoints = \"py:a:b\"\n";
        std::fs::write(dir.path().join("keel.toml"), original).unwrap();
        let r = run(dir.path(), "pipeline.ingest:main", false);
        assert_eq!(r.exit, EXIT_USAGE);
        assert!(r.human.contains("not an array"));
        assert_eq!(read_toml(&dir), original, "nothing was destroyed");
    }

    #[test]
    fn invalid_ref_is_a_usage_error_before_any_io() {
        let dir = project();
        let r = run(dir.path(), "not a ref", false);
        assert_eq!(r.exit, EXIT_USAGE);
        assert_eq!(r.json["code"], "KEEL-E001");
        assert!(!dir.path().join("keel.toml").exists());
    }
}
