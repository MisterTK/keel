//! `keel explain <code>` — the frozen error taxonomy, for humans and agents.
//!
//! The copy is `contracts/error-codes.json`, embedded with `include_str!` and
//! parsed once into typed entries. A coding agent that sees `KEEL-E014` gets the
//! exact remedy here without a web search (dx-spec §5); an unknown code exits
//! [`EXIT_USAGE`](crate::EXIT_USAGE) and lists every code it *does* know.

use std::collections::BTreeMap;
use std::sync::OnceLock;

use serde::{Deserialize, Serialize};

use crate::render::to_json;
use crate::{EXIT_USAGE, Rendered};

/// The frozen taxonomy, embedded so the binary and the contract never drift.
const ERROR_CODES_JSON: &str = include_str!("../../../contracts/error-codes.json");

/// The base for the per-code docs URL stub (dx-spec §5, "docs built for
/// retrieval").
const DOCS_BASE: &str = "https://keel.dev/errors";

/// One taxonomy entry — the four verbatim strings `keel explain` prints. Field
/// order matches `contracts/error-codes.json` for readability; the JSON twin
/// re-sorts keys via [`to_json`](crate::render::to_json).
#[derive(Debug, Clone, Deserialize)]
struct ErrorEntry {
    name: String,
    what: String,
    why: String,
    next: String,
}

/// The whole `error-codes.json` document, typed.
#[derive(Debug, Deserialize)]
struct ErrorCodes {
    codes: BTreeMap<String, ErrorEntry>,
}

/// The machine twin of an explained code (sorted-key JSON via [`to_json`]).
#[derive(Debug, Serialize)]
struct ExplainReport<'a> {
    code: &'a str,
    docs: String,
    name: &'a str,
    next: &'a str,
    /// An honest "(planned)" qualifier when the frozen `next` guidance points at
    /// a CLI affordance that does not exist yet (omitted otherwise).
    #[serde(skip_serializing_if = "Option::is_none")]
    planned: Option<&'static str>,
    what: &'a str,
    why: &'a str,
}

/// The frozen taxonomy references a few CLI affordances that v0.1 does not
/// implement yet. The taxonomy file is frozen, so rather than edit its `next`
/// copy we append an honest qualifier at render time (finding: `next` points at
/// nonexistent affordances — `keel replay`, the lease holder in `keel flows`,
/// Tier 1 trace ids).
fn planned_note(code: &str) -> Option<&'static str> {
    match code {
        "KEEL-E032" => Some(
            "`keel replay <flow>` is not implemented in v0.1 (planned). To force a fresh run of a \
             dead flow now, remove its rows from .keel/journal.db.",
        ),
        "KEEL-E030" => Some(
            "`keel flows` does not display the lease holder in v0.1 (planned); it lists flow id, \
             entrypoint, status, steps, and age.",
        ),
        "KEEL-E010" => Some(
            "`keel trace <id>` resolves durable-flow ids; Tier 1 trace ids (t-NNNNNN) are not \
             persisted in v0.1, so they cannot be looked up (planned).",
        ),
        _ => None,
    }
}

/// The machine twin when the code is unknown.
#[derive(Debug, Serialize)]
struct UnknownReport<'a> {
    error: &'static str,
    known: Vec<&'a str>,
    requested: &'a str,
}

/// Parse the embedded taxonomy once. The contract is validated in CI, so a parse
/// failure here is a build-time contract break, not a runtime condition.
fn taxonomy() -> &'static ErrorCodes {
    static TAXONOMY: OnceLock<ErrorCodes> = OnceLock::new();
    TAXONOMY.get_or_init(|| {
        serde_json::from_str(ERROR_CODES_JSON).expect("contracts/error-codes.json parses")
    })
}

/// Every known code, sorted (the `BTreeMap` already orders them).
fn known_codes() -> Vec<&'static str> {
    taxonomy().codes.keys().map(String::as_str).collect()
}

/// Explain `code`. A known code renders what/why/next + a docs URL and exits
/// [`EXIT_OK`](crate::EXIT_OK); an unknown one lists the known codes on stderr
/// and exits [`EXIT_USAGE`](crate::EXIT_USAGE).
pub fn run(code: &str) -> Rendered {
    let code = code.trim();
    let Some(entry) = taxonomy().codes.get(code) else {
        return unknown(code);
    };
    let docs = format!("{DOCS_BASE}/{code}");
    let planned = planned_note(code);
    let report = ExplainReport {
        code,
        docs: docs.clone(),
        name: &entry.name,
        next: &entry.next,
        planned,
        what: &entry.what,
        why: &entry.why,
    };
    let note = planned.map_or(String::new(), |p| format!("\n\nNote:  {p}"));
    let human = format!(
        "{code}  {name}\n\nWhat:  {what}\nWhy:   {why}\nNext:  {next}{note}\n\nDocs:  {docs}",
        name = entry.name,
        what = entry.what,
        why = entry.why,
        next = entry.next,
    );
    Rendered::ok(human, to_json(&report))
}

/// The unknown-code path: exit 2, list every known code.
fn unknown(code: &str) -> Rendered {
    let known = known_codes();
    let human = format!(
        "keel \u{25b8} {code}: unknown error code.\n\nKnown codes:\n{list}",
        list = known
            .iter()
            .map(|c| format!("  {c}"))
            .collect::<Vec<_>>()
            .join("\n"),
    );
    let report = UnknownReport {
        error: "unknown-code",
        known,
        requested: code,
    };
    Rendered {
        human,
        json: to_json(&report),
        exit: EXIT_USAGE,
        to_stderr: true,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn taxonomy_parses_and_has_the_frozen_codes() {
        let t = taxonomy();
        assert!(t.codes.contains_key("KEEL-E001"));
        assert!(t.codes.contains_key("KEEL-E014"));
        assert!(t.codes.contains_key("KEEL-E040"));
    }

    #[test]
    fn explain_e014_carries_the_contract_copy_verbatim() {
        let r = run("KEEL-E014");
        assert_eq!(r.exit, crate::EXIT_OK);
        // The four strings must appear verbatim (acceptance: matches
        // error-codes.json copy exactly).
        assert!(r.human.contains("non-idempotent-not-retried"));
        assert!(r.human.contains(
            "The call failed with a retryable error, but Keel did not retry it \
             because repeating it is not provably safe."
        ));
        assert!(r.human.contains(
            "Level 0 hard rule: non-idempotent calls (e.g. POST without an \
             idempotency key) are observed, never retried."
        ));
        assert!(r.human.contains(
            "If the API supports idempotency keys, configure \
             `idempotency = { header = \"...\" }` for the target; then retries \
             become safe."
        ));
        assert_eq!(r.json["docs"], "https://keel.dev/errors/KEEL-E014");
        assert_eq!(r.json["name"], "non-idempotent-not-retried");
    }

    #[test]
    fn explain_trims_surrounding_whitespace() {
        assert_eq!(run("  KEEL-E011  ").exit, crate::EXIT_OK);
    }

    #[test]
    fn planned_affordances_are_qualified_but_the_frozen_copy_stays_verbatim() {
        // The frozen `next` still points at `keel replay`, verbatim…
        let r = run("KEEL-E032");
        assert_eq!(r.exit, crate::EXIT_OK);
        assert!(
            r.human.contains("keel replay <flow>"),
            "frozen copy verbatim"
        );
        // …but an honest planned qualifier is appended (human + JSON).
        assert!(r.human.contains("not implemented in v0.1 (planned)"));
        assert!(r.json["planned"].as_str().unwrap().contains("keel replay"));
    }

    #[test]
    fn a_code_without_a_planned_gap_has_no_note() {
        let r = run("KEEL-E014");
        assert!(!r.human.contains("Note:"));
        assert!(r.json.get("planned").is_none() || r.json["planned"].is_null());
    }

    #[test]
    fn unknown_code_exits_usage_and_lists_known() {
        let r = run("KEEL-E999");
        assert_eq!(r.exit, EXIT_USAGE);
        assert!(r.to_stderr);
        assert_eq!(r.json["error"], "unknown-code");
        let known = r.json["known"].as_array().unwrap();
        assert!(known.iter().any(|c| c == "KEEL-E001"));
        assert!(r.human.contains("Known codes:"));
    }
}
