//! `keel flows suggest` — replay-safety analysis over candidate flow
//! entrypoints (dx-spec §1 Level 2: "Durability, still config-only").
//!
//! Combines two evidence sources exactly like `keel init` does (dx-spec §2):
//! the static scan's per-function attribution ([`scan::FunctionFacts`] —
//! intercepted-effect call sites, idempotent-unsafe effects, time/random
//! reads Tier 2 would virtualize, and constructs that defeat replay outright)
//! and `.keel/discovery.db`'s observed call counts, joined on the targets a
//! function's effects reference. A function is a **candidate** when it
//! performs at least one intercepted effect — a pure helper has nothing to
//! make durable.
//!
//! **Verdict.** `replay-safe: NO` only when the scan found a construct that
//! defeats replay outright (`unsafe_reasons`: threads, subprocesses, raw
//! sockets, `child_process`/`worker_threads`). Idempotent-unsafe effects and
//! time/random reads do **not** flip the verdict — Tier 2 injects an
//! idempotency key into retried effects automatically (CCR-2,
//! `contracts/adapter-pack.md`) and virtualizes time/random reads on replay;
//! they are surfaced as context for the reviewer, not blockers.
//!
//! **Ranking.** Candidates are ordered by observed traffic first (the
//! functions actually being exercised are the best flow candidates), then by
//! effect count, then by `(file, line)` as the final deterministic tie-break
//! (dx-spec §5) — never by insertion order alone, so a re-run against
//! unchanged evidence reproduces byte-identical `--json` output.
//!
//! **Honesty about coverage.** The JS/TS pass attributes by a brace-depth
//! line heuristic, not a parse (`scan::js` module docs) — it cannot see class
//! methods, object-literal methods, or a function whose opening `{` sits on
//! its own line. Python attribution is exact (real `ast` containment). Both
//! `python_available` and a fixed note about the JS/TS limitation ride along
//! in the report so `--json` output never overclaims precision it does not
//! have.

use std::collections::BTreeMap;
use std::fmt::Write as _;
use std::path::Path;

use keel_journal::TargetStats;
use serde::Serialize;

use crate::flows::soft_error;
use crate::flows_add::existing_entrypoints;
use crate::init::{has_python_files, plural};
use crate::render::to_json;
use crate::scan::{self, FunctionFacts, ScanResult};
use crate::{Rendered, evidence};

/// A fixed, honest note about the JS/TS scanner's attribution limits, carried
/// in the report itself so a `--json` consumer learns about it without
/// reading Rust source.
const JS_ATTRIBUTION_NOTE: &str = "JS/TS attribution is a line-oriented heuristic: it cannot see \
     class methods, object-literal methods, or a function whose opening `{` sits on its own \
     line. Python attribution is exact (real `ast` containment).";

/// One candidate flow entrypoint — the machine twin of one `keel flows
/// suggest` line.
#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
pub struct Candidate {
    /// True when `entrypoint` is already in `[flows] entrypoints`.
    pub already_designated: bool,
    /// Calls observed (summed across the function's referenced targets) in
    /// `.keel/discovery.db`. `0` when no traffic was recorded, which may mean
    /// no targets were statically identified either.
    pub calls_observed: i64,
    /// The bare display form `keel flows add` also accepts back
    /// (`pipeline.ingest:main`, `jobs/nightly.ts#run`) — the `py:`/`ts:`
    /// namespace stripped.
    pub display: String,
    /// Intercepted-effect call sites.
    pub effects: u32,
    /// The full `py:`/`ts:` entrypoint ref — pass this (or `display`) to
    /// `keel flows add`.
    pub entrypoint: String,
    /// Project-relative defining file.
    pub file: String,
    /// Effect calls that are POST/PATCH-shaped with no idempotency evidence.
    pub idempotent_unsafe: u32,
    /// 1-based line of the function definition.
    pub line: u32,
    /// Randomness reads that Tier 2 would virtualize on replay.
    pub random_reads: u32,
    /// `false` only when `unsafe_reasons` is non-empty.
    pub replay_safe: bool,
    /// Wall-clock reads that Tier 2 would virtualize on replay.
    pub time_reads: u32,
    /// Why `replay_safe` is `false` (empty when it is `true`); sorted,
    /// deterministic (inherited from the scan).
    pub unsafe_reasons: Vec<String>,
}

/// The `keel flows suggest` report.
#[derive(Debug, Serialize)]
struct SuggestReport {
    candidates: Vec<Candidate>,
    count: usize,
    js_attribution_note: &'static str,
    python_available: bool,
    replay_safe_count: usize,
}

/// `keel flows suggest` for `project`.
pub fn run(project: &Path) -> Rendered {
    let scan = scan::scan(project);
    let discovery = match evidence::read_discovery(project) {
        Ok(d) => d,
        Err(e) => return soft_error(&e),
    };
    let existing = read_existing(project);
    let report = build_report(&scan, &discovery, &existing);
    let human = human(&report, project);
    Rendered::ok(human, to_json(&report))
}

/// The `[flows] entrypoints` already designated, or empty when `keel.toml` is
/// absent, unreadable, or invalid — `suggest` is a report, not a validator; a
/// broken policy just means it cannot say which candidates are already flows
/// (`keel doctor` carries policy validity separately).
fn read_existing(project: &Path) -> Vec<String> {
    let text = std::fs::read_to_string(evidence::keel_toml(project)).ok();
    existing_entrypoints(text.as_deref()).unwrap_or_default()
}

/// Build the report from a scan, observed traffic, and the designated
/// entrypoints. Pure — unit-tested without touching the filesystem.
fn build_report(
    scan: &ScanResult,
    discovery: &[TargetStats],
    existing: &[String],
) -> SuggestReport {
    let calls_by_target: BTreeMap<&str, i64> = discovery
        .iter()
        .map(|s| (s.target.as_str(), s.calls))
        .collect();
    let mut candidates: Vec<Candidate> = scan
        .functions
        .iter()
        .filter(|f| f.effects > 0)
        .map(|f| to_candidate(f, &calls_by_target, existing))
        .collect();
    // Rank by observed traffic, then effect count, then the scan's own
    // deterministic (file, line) order — never insertion order alone.
    candidates.sort_by(|a, b| {
        b.calls_observed
            .cmp(&a.calls_observed)
            .then_with(|| b.effects.cmp(&a.effects))
            .then_with(|| a.file.cmp(&b.file))
            .then_with(|| a.line.cmp(&b.line))
    });
    let replay_safe_count = candidates.iter().filter(|c| c.replay_safe).count();
    SuggestReport {
        count: candidates.len(),
        replay_safe_count,
        candidates,
        js_attribution_note: JS_ATTRIBUTION_NOTE,
        python_available: scan.python_available,
    }
}

fn to_candidate(
    f: &FunctionFacts,
    calls_by_target: &BTreeMap<&str, i64>,
    existing: &[String],
) -> Candidate {
    let calls_observed = f
        .targets
        .iter()
        .filter_map(|t| calls_by_target.get(t.as_str()))
        .sum();
    Candidate {
        already_designated: existing.iter().any(|e| e == &f.entrypoint),
        calls_observed,
        display: bare_display(&f.entrypoint),
        effects: f.effects,
        entrypoint: f.entrypoint.clone(),
        file: f.file.clone(),
        idempotent_unsafe: f.idempotent_unsafe,
        line: f.line,
        random_reads: f.random_reads,
        replay_safe: f.unsafe_reasons.is_empty(),
        time_reads: f.time_reads,
        unsafe_reasons: f.unsafe_reasons.clone(),
    }
}

/// Strip the `py:`/`ts:` namespace for display — the same bare form `keel
/// flows add` infers back (`crate::flows_add::normalize_entrypoint`).
fn bare_display(entrypoint: &str) -> String {
    entrypoint
        .strip_prefix("py:")
        .or_else(|| entrypoint.strip_prefix("ts:"))
        .unwrap_or(entrypoint)
        .to_owned()
}

/// The human report, derived entirely from [`SuggestReport`] (plus a
/// filesystem probe for the python3-not-found note, exactly like `init`'s).
fn human(report: &SuggestReport, project: &Path) -> String {
    let python_note = (!report.python_available && has_python_files(project))
        .then_some("\nkeel \u{25b8} note: python3 was not found; Python files were not scanned.\n");
    if report.candidates.is_empty() {
        return format!(
            "keel \u{25b8} no candidate flow entrypoints found (no function calls an intercepted effect).{}",
            python_note.unwrap_or_default()
        );
    }
    let mut lines = vec![format!(
        "keel \u{25b8} {} candidate flow entrypoint{}, {} replay-safe:\n",
        report.count,
        plural(report.count),
        report.replay_safe_count,
    )];
    for c in &report.candidates {
        lines.push(format!("  {}\n", candidate_line(c)));
    }
    if let Some(note) = python_note {
        lines.push(note.to_owned());
    }
    lines.push(format!(
        "\nkeel \u{25b8} note: {}\n",
        report.js_attribution_note
    ));
    lines.push("\nkeel \u{25b8} designate one with `keel flows add <entrypoint>`.\n".to_owned());
    lines.concat()
}

/// One candidate's line, in the spec's shape (dx-spec §1 Level 2):
/// `pipeline.ingest:main      12 effects, 3 idempotent-unsafe, est. replay-safe: YES`.
fn candidate_line(c: &Candidate) -> String {
    let mut line = format!(
        "{:<28}{} effect{}",
        c.display,
        c.effects,
        plural(c.effects as usize)
    );
    if c.idempotent_unsafe > 0 {
        let _ = write!(line, ", {} idempotent-unsafe", c.idempotent_unsafe);
    }
    let _ = write!(
        line,
        ", est. replay-safe: {}",
        if c.replay_safe { "YES" } else { "NO" }
    );
    if c.replay_safe {
        let mut virtualized = Vec::new();
        if c.time_reads > 0 {
            virtualized.push(format!(
                "{} time read{}",
                c.time_reads,
                plural(c.time_reads as usize)
            ));
        }
        if c.random_reads > 0 {
            virtualized.push(format!(
                "{} random read{}",
                c.random_reads,
                plural(c.random_reads as usize)
            ));
        }
        if !virtualized.is_empty() {
            let _ = write!(line, " ({} will be virtualized)", virtualized.join(", "));
        }
    } else {
        let _ = write!(line, " \u{2014} {}", c.unsafe_reasons.join("; "));
    }
    if c.calls_observed > 0 {
        let _ = write!(
            line,
            " [observed {} call{}]",
            c.calls_observed,
            plural(usize::try_from(c.calls_observed).unwrap_or(usize::MAX))
        );
    }
    if c.already_designated {
        line.push_str(" [already a flow]");
    }
    line
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::BTreeSet;

    fn facts(entrypoint: &str, file: &str, line: u32) -> FunctionFacts {
        FunctionFacts {
            entrypoint: entrypoint.to_owned(),
            file: file.to_owned(),
            line,
            ..FunctionFacts::default()
        }
    }

    fn scan_with(functions: Vec<FunctionFacts>) -> ScanResult {
        ScanResult {
            python_available: true,
            functions,
            ..ScanResult::default()
        }
    }

    #[test]
    fn pure_functions_are_not_candidates() {
        let scan = scan_with(vec![facts("py:app:helper", "app.py", 1)]);
        let report = build_report(&scan, &[], &[]);
        assert_eq!(report.count, 0);
        assert!(report.candidates.is_empty());
    }

    #[test]
    fn a_function_with_effects_is_a_candidate_and_defaults_replay_safe() {
        let mut f = facts("py:pipeline.ingest:main", "pipeline/ingest.py", 10);
        f.effects = 12;
        f.idempotent_unsafe = 3;
        let scan = scan_with(vec![f]);
        let report = build_report(&scan, &[], &[]);
        assert_eq!(report.count, 1);
        assert_eq!(report.replay_safe_count, 1);
        let c = &report.candidates[0];
        assert_eq!(c.display, "pipeline.ingest:main");
        assert_eq!(c.entrypoint, "py:pipeline.ingest:main");
        assert!(c.replay_safe);
        assert!(!c.already_designated);
    }

    #[test]
    fn unsafe_reasons_flip_the_verdict_but_idempotent_unsafe_alone_does_not() {
        let mut safe = facts("py:app:a", "app.py", 1);
        safe.effects = 1;
        safe.idempotent_unsafe = 5;
        let mut unsafe_fn = facts("py:app:b", "app.py", 20);
        unsafe_fn.effects = 1;
        unsafe_fn.unsafe_reasons = vec!["subprocess use at app.py:22".to_owned()];
        let scan = scan_with(vec![safe, unsafe_fn]);
        let report = build_report(&scan, &[], &[]);
        assert_eq!(report.replay_safe_count, 1);
        let a = report
            .candidates
            .iter()
            .find(|c| c.entrypoint.ends_with(":a"))
            .unwrap();
        assert!(
            a.replay_safe,
            "idempotent-unsafe alone does not block replay-safety"
        );
        let b = report
            .candidates
            .iter()
            .find(|c| c.entrypoint.ends_with(":b"))
            .unwrap();
        assert!(!b.replay_safe);
        assert_eq!(
            b.unsafe_reasons,
            vec!["subprocess use at app.py:22".to_owned()]
        );
    }

    #[test]
    fn already_designated_entrypoints_are_flagged() {
        let mut f = facts("py:pipeline.ingest:main", "pipeline/ingest.py", 10);
        f.effects = 1;
        let scan = scan_with(vec![f]);
        let report = build_report(&scan, &[], &["py:pipeline.ingest:main".to_owned()]);
        assert!(report.candidates[0].already_designated);
    }

    #[test]
    fn discovery_calls_join_on_targets_and_drive_ranking() {
        let mut quiet = facts("py:app:quiet", "app.py", 1);
        quiet.effects = 10;
        quiet.targets = BTreeSet::from(["api.quiet.example".to_owned()]);
        let mut busy = facts("py:app:busy", "app.py", 30);
        busy.effects = 2;
        busy.targets = BTreeSet::from(["api.busy.example".to_owned()]);
        let scan = scan_with(vec![quiet.clone(), busy.clone()]);
        let discovery = vec![
            TargetStats {
                target: "api.busy.example".to_owned(),
                calls: 500,
                ..zero_stats()
            },
            TargetStats {
                target: "api.quiet.example".to_owned(),
                calls: 1,
                ..zero_stats()
            },
        ];
        let report = build_report(&scan, &discovery, &[]);
        // Observed traffic outranks a higher static effect count.
        assert_eq!(report.candidates[0].display, "app:busy");
        assert_eq!(report.candidates[0].calls_observed, 500);
        assert_eq!(report.candidates[1].display, "app:quiet");
        assert_eq!(report.candidates[1].calls_observed, 1);
    }

    #[test]
    fn ordering_is_deterministic_by_file_and_line_when_traffic_and_effects_tie() {
        let mut b = facts("py:z:second", "z.py", 5);
        b.effects = 1;
        let mut a = facts("py:a:first", "a.py", 1);
        a.effects = 1;
        let scan = scan_with(vec![b, a]);
        let report = build_report(&scan, &[], &[]);
        assert_eq!(report.candidates[0].file, "a.py");
        assert_eq!(report.candidates[1].file, "z.py");
    }

    #[test]
    fn human_line_matches_the_spec_shape() {
        let mut f = facts("py:pipeline.ingest:main", "pipeline/ingest.py", 10);
        f.effects = 12;
        f.idempotent_unsafe = 3;
        let c = to_candidate(&f, &BTreeMap::new(), &[]);
        let line = candidate_line(&c);
        assert!(line.contains("pipeline.ingest:main"));
        assert!(line.contains("12 effects"));
        assert!(line.contains("3 idempotent-unsafe"));
        assert!(line.contains("est. replay-safe: YES"));
    }

    #[test]
    fn human_line_notes_virtualized_reads_when_replay_safe() {
        let mut f = facts("py:jobs.nightly:run", "jobs/nightly.py", 4);
        f.effects = 31;
        f.time_reads = 2;
        let c = to_candidate(&f, &BTreeMap::new(), &[]);
        let line = candidate_line(&c);
        assert!(line.contains("31 effects"));
        assert!(!line.contains("idempotent-unsafe"));
        assert!(line.contains("est. replay-safe: YES"));
        assert!(line.contains("2 time reads will be virtualized"));
    }

    #[test]
    fn human_line_singular_read_is_not_pluralized() {
        let mut f = facts("py:app:one_read", "app.py", 1);
        f.effects = 1;
        f.time_reads = 1;
        let c = to_candidate(&f, &BTreeMap::new(), &[]);
        assert!(candidate_line(&c).contains("1 time read will be virtualized"));
    }

    #[test]
    fn human_output_lists_no_candidates_when_scan_finds_none() {
        let report = build_report(&scan_with(vec![]), &[], &[]);
        let out = human(&report, Path::new("."));
        assert!(out.contains("no candidate flow entrypoints found"));
    }

    #[test]
    fn json_report_carries_the_js_attribution_honesty_note() {
        let report = build_report(&scan_with(vec![]), &[], &[]);
        let json = to_json(&report);
        assert!(
            json["js_attribution_note"]
                .as_str()
                .unwrap()
                .contains("line-oriented heuristic")
        );
    }

    fn zero_stats() -> TargetStats {
        TargetStats {
            target: String::new(),
            calls: 0,
            attempts: 0,
            retries: 0,
            successes: 0,
            failures: 0,
            cache_hits: 0,
            throttled: 0,
            breaker_opens: 0,
            total_latency_ms: 0,
            max_latency_ms: 0,
            first_seen_ms: 0,
            last_seen_ms: 0,
            last_error_class: None,
            last_error_status: None,
        }
    }
}
