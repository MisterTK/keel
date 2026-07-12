//! `keel init` — evidence-merged policy generation (dx-spec §1, Level 1).
//!
//! Walks the project (static scan, §[`scan`](crate::scan)), merges in observed
//! traffic from `.keel/discovery.db`, and writes a `keel.toml` that "reads like
//! a senior SRE reviewed your codebase": every target cites `file:line`
//! evidence, and observed targets carry their real call counts. The generated
//! file *is* the documentation — deleting any entry just falls back to the same
//! built-in defaults.
//!
//! Determinism (dx-spec §5): no date in the header unless `--stamp`, targets and
//! sightings sorted, byte-identical output for identical inputs. `--diff`
//! previews changes against an existing file without writing.

use std::collections::{BTreeMap, BTreeSet};
use std::path::Path;

use keel_journal::TargetStats;
use serde::Serialize;

use crate::render::to_json;
use crate::scan::{ScanResult, TargetClass, TargetEvidence};
use crate::{EXIT_USAGE, Rendered, evidence, scan};

/// Column at which trailing `#` comments begin, when the line is shorter.
const COMMENT_COL: usize = 37;

/// Options parsed from the `keel init` flags.
#[derive(Debug, Clone, Copy, Default)]
pub struct InitOptions {
    /// Preview changes against an existing `keel.toml` instead of writing.
    pub diff: bool,
    /// Stamp today's date into the header (off by default for determinism).
    pub stamp: bool,
    /// Drop the Keel section into `AGENTS.md` (dx-spec §5) instead of generating
    /// a policy — so every future coding-agent session inherits Keel context.
    pub agents: bool,
}

/// Marker fencing the Keel-managed region in `AGENTS.md`, so a re-run updates the
/// section in place (idempotent) instead of appending a duplicate.
const AGENTS_BEGIN: &str = "<!-- keel:begin -->";
const AGENTS_END: &str = "<!-- keel:end -->";

/// The concise, agent-facing Keel section (dx-spec §5). Deterministic: no dates
/// or versions, so an agent can diff it. Bytes are golden-tested.
const AGENTS_SNIPPET: &str = "\
## Keel (resilience & durable execution)

This project uses **Keel** for production-grade resilience (retries, timeouts,
circuit breakers, rate limits) and opt-in durable flows — applied at intercepted
call boundaries with **zero code changes**. Policy lives in one file: `keel.toml`.

Before changing any resilience behavior:
- Run `keel doctor --json` to see what is wrapped, what is not, and why.
- Propose policy edits as a diff: `keel init --diff` shows adds/removes from evidence.
- Every command has a `--json` twin with deterministic, sorted output — diff it to detect change.

Useful commands (all support `--json`):
- `keel status` — coverage, retries saved, breaker events, resumable flows.
- `keel explain <KEEL-E0NN>` — the exact what/why/next for an error code.
- `keel flows` / `keel trace <flow>` — durable (Tier 2) flow state and step ledger.

Do not hand-write retry loops or backoff around calls Keel already wraps; edit
`keel.toml` instead. Uninstalling Keel removes the behavior and nothing else —
the code runs identically without it.";

/// The full fenced block written into `AGENTS.md` (begin marker, snippet, end
/// marker, trailing newline). Public so the golden test can pin its bytes.
#[must_use]
pub fn agents_block() -> String {
    format!("{AGENTS_BEGIN}\n{AGENTS_SNIPPET}\n{AGENTS_END}\n")
}

/// The machine twin of `--agents`.
#[derive(Debug, Serialize)]
struct AgentsReport {
    already_current: bool,
    path: String,
    updated: bool,
    wrote: bool,
}

/// `keel init --agents`: create/update the Keel section in `AGENTS.md`. Idempotent
/// — a marker-fenced region is replaced in place on re-run, so it never appends a
/// duplicate and reflects the current snippet exactly.
fn run_agents(project: &Path) -> Rendered {
    let path = project.join("AGENTS.md");
    let existing = match std::fs::read_to_string(&path) {
        Ok(text) => text,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => String::new(),
        Err(e) => return config_error(&format!("could not read {}: {e}", path.display())),
    };
    let block = agents_block();
    let (new_content, replaced, wrote) = splice_agents_block(&existing, &block);
    let already_current = !wrote;
    // `updated` = we replaced an existing region AND its bytes changed; a fresh
    // create is `wrote` but not `updated`, and a no-op re-run is neither.
    let updated = wrote && replaced;
    if wrote && let Err(e) = std::fs::write(&path, &new_content) {
        return config_error(&format!("could not write {}: {e}", path.display()));
    }
    let verb = if already_current {
        "already current"
    } else if updated {
        "updated the Keel section in"
    } else {
        "wrote the Keel section to"
    };
    let human = format!("keel \u{25b8} {verb} {}", path.display());
    let report = AgentsReport {
        already_current,
        path: path.display().to_string(),
        updated,
        wrote,
    };
    Rendered::ok(human, to_json(&report))
}

/// Compute the new `AGENTS.md` content given the existing text and the desired
/// block. Returns `(content, replaced_existing, needs_write)`. Pure — unit
/// tested. Replaces a marker-fenced region in place; else appends (or creates).
fn splice_agents_block(existing: &str, block: &str) -> (String, bool, bool) {
    if let (Some(start), Some(end_idx)) = (existing.find(AGENTS_BEGIN), existing.find(AGENTS_END)) {
        let end = end_idx + AGENTS_END.len();
        // Consume a single trailing newline after the end marker so re-splicing
        // is stable (the block already ends in one).
        let tail_start = existing[end..].strip_prefix('\n').map_or(end, |_| end + 1);
        let mut out = String::with_capacity(existing.len());
        out.push_str(&existing[..start]);
        out.push_str(block);
        out.push_str(&existing[tail_start..]);
        let needs_write = out != existing;
        return (out, true, needs_write);
    }
    if existing.is_empty() {
        return (block.to_owned(), false, true);
    }
    let mut out = existing.to_owned();
    if !out.ends_with('\n') {
        out.push('\n');
    }
    out.push('\n');
    out.push_str(block);
    (out, false, true)
}

/// The machine twin of a write.
#[derive(Debug, Serialize)]
struct WroteReport {
    gitignore_updated: bool,
    observed_runs: u32,
    static_scans: usize,
    targets: Vec<String>,
    wrote: String,
}

/// The machine twin of `--diff`.
#[derive(Debug, Serialize)]
struct DiffReport {
    added: Vec<String>,
    removed: Vec<String>,
    unchanged: Vec<String>,
}

/// Run `keel init` for `project`.
pub fn run(project: &Path, opts: InitOptions) -> Rendered {
    if opts.agents {
        return run_agents(project);
    }
    let scan = scan::scan(project);
    let discovery = match evidence::read_discovery(project) {
        Ok(d) => d,
        Err(e) => return config_error(&e),
    };

    let content = render_keel_toml(&scan, &discovery, opts.stamp.then(today_utc).as_deref());
    let targets = merged_targets(&scan, &discovery);

    let toml_path = evidence::keel_toml(project);
    if opts.diff {
        return diff(&toml_path, &targets);
    }
    if toml_path.exists() {
        return config_error(&format!(
            "{} already exists. Run `keel init --diff` to preview changes, or edit it directly.",
            toml_path.display()
        ));
    }

    if let Err(e) = std::fs::write(&toml_path, &content) {
        return config_error(&format!("could not write {}: {e}", toml_path.display()));
    }
    let gitignore_updated = update_gitignore(project).unwrap_or(false);

    let mut warnings = String::new();
    if !scan.python_available && has_python_files(project) {
        warnings.push_str(
            "\nkeel \u{25b8} note: python3 was not found; Python files were not scanned.\n",
        );
    }

    let observed_runs = u32::from(!discovery.is_empty());
    let human = format!(
        "keel \u{25b8} wrote {} ({} target{}) from {} static scan{} + {} observed run{}.{}",
        toml_path.display(),
        targets.len(),
        plural(targets.len()),
        scan.files_scanned,
        plural(scan.files_scanned),
        observed_runs,
        plural(observed_runs as usize),
        if warnings.is_empty() {
            String::new()
        } else {
            warnings
        }
    );
    let report = WroteReport {
        gitignore_updated,
        observed_runs,
        static_scans: scan.files_scanned,
        targets,
        wrote: toml_path.display().to_string(),
    };
    Rendered::ok(human, to_json(&report))
}

/// The set of targets the generated file will contain: static findings unioned
/// with discovery-only targets (runtime caught what the scan missed).
fn merged_targets(scan: &ScanResult, discovery: &[TargetStats]) -> Vec<String> {
    let mut set: BTreeSet<String> = scan.targets.keys().cloned().collect();
    for stats in discovery {
        set.insert(stats.target.clone());
    }
    set.into_iter().collect()
}

/// Render the full `keel.toml` text. Pure and deterministic — the snapshot
/// tests pin its bytes.
pub fn render_keel_toml(
    scan: &ScanResult,
    discovery: &[TargetStats],
    stamp: Option<&str>,
) -> String {
    let by_target: BTreeMap<&str, &TargetStats> =
        discovery.iter().map(|s| (s.target.as_str(), s)).collect();
    let observed_runs = u32::from(!discovery.is_empty());

    let mut out = String::new();
    let date = stamp.map_or_else(String::new, |d| format!(" ({d})"));
    let header = format!(
        "# Generated by keel init from {} static scan{} + {} observed run{}{}\n",
        scan.files_scanned,
        plural(scan.files_scanned),
        observed_runs,
        plural(observed_runs as usize),
        date,
    );
    out.push_str(&header);
    out.push_str(
        "# Every entry below was found in YOUR code. Delete anything; defaults still apply.\n",
    );

    for target in merged_targets(scan, discovery) {
        out.push('\n');
        let evidence = scan.targets.get(&target);
        let stats = by_target.get(target.as_str()).copied();
        write_block(&mut out, &target, evidence, stats);
    }
    out
}

/// Write one `[target."…"]` block: header + evidence comment(s) + policy body.
fn write_block(
    out: &mut String,
    target: &str,
    evidence: Option<&TargetEvidence>,
    stats: Option<&TargetStats>,
) {
    let header = format!("[target.\"{target}\"]");
    let seen_comment = evidence.map(|e| {
        let labels = e
            .sightings
            .iter()
            .map(scan::Sighting::label)
            .collect::<Vec<_>>()
            .join(", ");
        format!("# seen in: {labels}")
    });
    let comment =
        seen_comment.unwrap_or_else(|| "# seen only at runtime (.keel/discovery.db)".to_owned());
    out.push_str(&pad_comment(&header, &comment));
    out.push('\n');

    if let Some(s) = stats {
        let observed = format!("# {}\n", observed_comment(s));
        out.push_str(&observed);
    }

    let class = evidence.map_or_else(
        || {
            if target.starts_with("llm:") {
                TargetClass::Llm
            } else {
                TargetClass::Host
            }
        },
        |e| e.class,
    );
    match (class, stats) {
        // dx-spec §1 flagship: an observed `llm:*` target earns an *active* rate
        // limit tuned from its own evidence, inserted between breaker and cache.
        (TargetClass::Llm, Some(s)) => {
            out.push_str(LLM_BODY_HEAD);
            out.push_str(&observed_rate_line(s));
            out.push('\n');
            out.push_str(LLM_CACHE_LINE);
        }
        // Host targets stay comments-only even with observed traffic: imposing an
        // active throttle on general outbound HTTP without an explicit opt-in
        // would be a Level-0 surprise (dx-spec §1 hard rules). An evidence-tuned
        // host rate is deliberately out of scope for v0.1.
        _ => out.push_str(&policy_body(class)),
    }
}

/// Outbound-host policy body. Mirrors the frozen smart-defaults pack
/// (`contracts/defaults.toml` outbound); a test asserts they stay in sync.
const HOST_BODY: &str = concat!(
    "timeout = \"30s\"\n",
    "retry   = { attempts = 3, schedule = \"exp(200ms, x2, max 30s, jitter)\", on = [\"conn\", \"timeout\", \"429\", \"5xx\"] }\n",
    "breaker = { failures = 5, cooldown = \"15s\" }\n",
);

/// The LLM body up to and including the breaker line — everything that precedes
/// the *optional* evidence-derived `rate` line. Mirrors `contracts/defaults.toml`
/// llm pack.
const LLM_BODY_HEAD: &str = concat!(
    "timeout = \"120s\"\n",
    "retry   = { attempts = 6, schedule = \"exp(500ms, x2, max 60s, jitter)\", on = [\"conn\", \"timeout\", \"429\", \"5xx\"] }\n",
    "breaker = { failures = 5, cooldown = \"30s\" }\n",
);

/// The LLM dev-cache line — always the last line of an `llm:*` block.
const LLM_CACHE_LINE: &str =
    "cache   = { mode = \"dev\" }          # dev-loop cache; disabled when KEEL_ENV=prod\n";

/// Floor for an observed `llm:*` target's active rate, in calls/min. Below this
/// the derived headroom is noise (LLM traffic is bursty), so we never emit an
/// active limit under 60/min — also the value used when the observation window
/// is a single instant (no mean to derive).
const LLM_RATE_FLOOR_PER_MIN: u64 = 60;

/// Headroom multiplier over the observed MEAN rate. The discovery store keeps
/// only `calls` + `first_seen_ms`/`last_seen_ms` — it measures a mean, never a
/// per-minute *peak* — so we scale the mean up generously to leave room for the
/// peaks we did not measure. NEVER describe the result as a peak.
const LLM_RATE_HEADROOM: u64 = 3;

/// The policy body for a class *without* any evidence-derived keys. Values
/// mirror the frozen smart-defaults pack (`contracts/defaults.toml`); a test
/// asserts they stay in sync. Writing them out (rather than relying on the
/// invisible defaults) makes the file self-documenting — the DX promise that
/// "the generated file is the docs".
fn policy_body(class: TargetClass) -> String {
    match class {
        TargetClass::Host => HOST_BODY.to_owned(),
        TargetClass::Llm => format!("{LLM_BODY_HEAD}{LLM_CACHE_LINE}"),
    }
}

/// The observed MEAN calls/minute as an integer floor, or `0` when the window is
/// a single instant (`first_seen_ms == last_seen_ms`). Pure integer math keeps
/// the output byte-deterministic. Basis for both the derived rate and its
/// comment.
fn mean_per_min_floor(s: &TargetStats) -> u64 {
    let span_ms = u64::try_from((s.last_seen_ms - s.first_seen_ms).max(0)).unwrap_or(u64::MAX);
    if span_ms == 0 {
        return 0;
    }
    let calls = u64::try_from(s.calls.max(0)).unwrap_or(u64::MAX);
    calls.saturating_mul(60_000) / span_ms
}

/// Derive an active per-minute rate limit for an observed `llm:*` target:
/// `mean × LLM_RATE_HEADROOM`, [rounded up to a clean value](round_up_clean),
/// clamped to a floor of [`LLM_RATE_FLOOR_PER_MIN`]. A single-instant window has
/// no derivable mean, so it falls back to the floor. Deterministic integer math.
fn llm_rate_per_min(s: &TargetStats) -> u64 {
    let mean = mean_per_min_floor(s);
    if mean == 0 {
        return LLM_RATE_FLOOR_PER_MIN;
    }
    round_up_clean(mean.saturating_mul(LLM_RATE_HEADROOM)).max(LLM_RATE_FLOOR_PER_MIN)
}

/// Round `n` UP to the next "clean" value in the 1-2-5 decade series
/// (…10, 20, 50, 100, 200, 500, 1000…) — the standard nice-number ceiling.
/// `round_up_clean(0) == 0`.
fn round_up_clean(n: u64) -> u64 {
    if n == 0 {
        return 0;
    }
    let mut unit = 1_u64;
    loop {
        for m in [1_u64, 2, 5] {
            let candidate = m.saturating_mul(unit);
            if candidate >= n {
                return candidate;
            }
        }
        match unit.checked_mul(10) {
            Some(next) => unit = next,
            None => return u64::MAX,
        }
    }
}

/// The active `rate` line for an observed `llm:*` target, comment-aligned like
/// the rest of the block. Honest about what we measured: it cites the mean,
/// never a peak.
fn observed_rate_line(s: &TargetStats) -> String {
    let mean = mean_per_min_floor(s);
    let comment = if mean == 0 {
        "# floor: single observation window, no mean to derive".to_owned()
    } else {
        format!("# headroom over your observed mean of ~{mean}/min")
    };
    pad_comment(
        &format!("rate    = \"{}/min\"", llm_rate_per_min(s)),
        &comment,
    )
}

/// The observed-traffic comment for a target with discovery evidence.
fn observed_comment(s: &TargetStats) -> String {
    format!(
        "observed: {} call{}, {} retr{}, ~{:.1}/min mean (.keel/discovery.db)",
        s.calls,
        plural(usize::try_from(s.calls).unwrap_or(usize::MAX)),
        s.retries,
        if s.retries == 1 { "y" } else { "ies" },
        per_minute(s),
    )
}

/// Mean calls/minute over the observed window; falls back to the raw call count
/// when the window has zero span (a single observation).
fn per_minute(s: &TargetStats) -> f64 {
    #[expect(
        clippy::cast_precision_loss,
        reason = "call counts and ms spans are small; f64 is exact enough for a comment"
    )]
    let (calls, span_ms) = (
        s.calls as f64,
        (s.last_seen_ms - s.first_seen_ms).max(0) as f64,
    );
    if span_ms <= 0.0 {
        calls
    } else {
        calls * 60_000.0 / span_ms
    }
}

/// Pad `line` so a trailing `#` comment starts at [`COMMENT_COL`] (or one space
/// past a longer line), keeping comment columns aligned and deterministic.
fn pad_comment(line: &str, comment: &str) -> String {
    let width = if line.len() < COMMENT_COL {
        COMMENT_COL
    } else {
        line.len() + 1
    };
    format!("{line:<width$}{comment}")
}

/// `--diff`: compare generated targets against those already in `keel.toml`.
fn diff(toml_path: &Path, generated: &[String]) -> Rendered {
    let existing = match read_existing_targets(toml_path) {
        Ok(t) => t,
        Err(e) => return config_error(&e),
    };
    let generated_set: BTreeSet<&str> = generated.iter().map(String::as_str).collect();
    let added: Vec<String> = generated_set
        .iter()
        .filter(|t| !existing.contains(**t))
        .map(|t| (*t).to_owned())
        .collect();
    let removed: Vec<String> = existing
        .iter()
        .filter(|t| !generated_set.contains(t.as_str()))
        .cloned()
        .collect();
    let unchanged: Vec<String> = generated_set
        .iter()
        .filter(|t| existing.contains(**t))
        .map(|t| (*t).to_owned())
        .collect();

    let mut human = String::from("keel \u{25b8} keel init --diff\n");
    if added.is_empty() && removed.is_empty() {
        human.push_str("  no changes: every discovered target is already in keel.toml.\n");
    } else {
        for t in &added {
            let line = format!("  + [target.\"{t}\"]   (found in code, not in keel.toml)\n");
            human.push_str(&line);
        }
        for t in &removed {
            let line = format!("  - [target.\"{t}\"]   (in keel.toml, no longer found in code)\n");
            human.push_str(&line);
        }
    }
    let report = DiffReport {
        added,
        removed,
        unchanged,
    };
    Rendered::ok(human, to_json(&report))
}

/// The set of `[target."…"]` keys already declared in an existing `keel.toml`.
/// A missing file is an empty set (everything is "added").
fn read_existing_targets(toml_path: &Path) -> Result<BTreeSet<String>, String> {
    if !toml_path.exists() {
        return Ok(BTreeSet::new());
    }
    let text = std::fs::read_to_string(toml_path)
        .map_err(|e| format!("could not read {}: {e}", toml_path.display()))?;
    let value: toml::Value = text
        .parse()
        .map_err(|e| format!("{} is not valid TOML: {e}", toml_path.display()))?;
    let mut set = BTreeSet::new();
    if let Some(table) = value.get("target").and_then(toml::Value::as_table) {
        for key in table.keys() {
            set.insert(key.clone());
        }
    }
    Ok(set)
}

/// Append `.keel/` to `.gitignore` (creating it if absent) when not already
/// ignored. Returns whether the file was changed.
fn update_gitignore(project: &Path) -> std::io::Result<bool> {
    let path = project.join(".gitignore");
    if !path.exists() {
        std::fs::write(&path, ".keel/\n")?;
        return Ok(true);
    }
    let text = std::fs::read_to_string(&path)?;
    let ignored = text
        .lines()
        .map(str::trim)
        .any(|l| l == ".keel" || l == ".keel/");
    if ignored {
        return Ok(false);
    }
    let mut updated = text;
    if !updated.ends_with('\n') && !updated.is_empty() {
        updated.push('\n');
    }
    updated.push_str(".keel/\n");
    std::fs::write(&path, updated)?;
    Ok(true)
}

fn has_python_files(project: &Path) -> bool {
    fn walk(dir: &Path) -> bool {
        let Ok(entries) = std::fs::read_dir(dir) else {
            return false;
        };
        for entry in entries.flatten() {
            let path = entry.path();
            let name = entry.file_name();
            let name = name.to_string_lossy();
            if path.is_dir() {
                if scan::SKIP_DIRS.contains(&name.as_ref()) || name.starts_with('.') {
                    continue;
                }
                if walk(&path) {
                    return true;
                }
            } else if path.extension().and_then(|e| e.to_str()) == Some("py") {
                return true;
            }
        }
        false
    }
    walk(project)
}

/// A config/usage failure (exit 2), rendered for both audiences.
fn config_error(message: &str) -> Rendered {
    #[derive(Serialize)]
    struct ErrReport<'a> {
        code: &'static str,
        error: &'a str,
    }
    let human = format!("keel \u{25b8} KEEL-E001: {message}");
    Rendered {
        human,
        json: to_json(&ErrReport {
            code: "KEEL-E001",
            error: message,
        }),
        exit: EXIT_USAGE,
        to_stderr: true,
    }
    .with_exit(EXIT_USAGE)
}

fn plural(n: usize) -> &'static str {
    if n == 1 { "" } else { "s" }
}

/// Today's date as `YYYY-MM-DD` (UTC). Only reached under `--stamp`, so the
/// determinism guarantee (no wall clock by default) holds. Civil-date math is
/// Hinnant's algorithm — no dependency, no locale.
fn today_utc() -> String {
    let secs = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |d| d.as_secs());
    let days = i64::try_from(secs / 86_400).unwrap_or(0);
    let (y, m, d) = civil_from_days(days);
    format!("{y:04}-{m:02}-{d:02}")
}

/// Convert days-since-epoch to `(year, month, day)` (proleptic Gregorian).
fn civil_from_days(z: i64) -> (i64, u32, u32) {
    let z = z + 719_468;
    let era = if z >= 0 { z } else { z - 146_096 } / 146_097;
    let doe = z - era * 146_097;
    let yoe = (doe - doe / 1_460 + doe / 36_524 - doe / 146_096) / 365;
    let y = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let m = if mp < 10 { mp + 3 } else { mp - 9 };
    let y = if m <= 2 { y + 1 } else { y };
    #[expect(
        clippy::cast_sign_loss,
        clippy::cast_possible_truncation,
        reason = "m,d in 1..=31"
    )]
    (y, m as u32, d as u32)
}

#[cfg(test)]
mod tests {
    use std::fs;

    use keel_journal::ErrorClass;
    use tempfile::TempDir;

    use super::*;

    /// A `TargetStats` for `llm:openai` with the given call count and observation
    /// window; every other counter is inert (irrelevant to rate derivation).
    fn llm_stats(calls: i64, first_seen_ms: i64, last_seen_ms: i64) -> TargetStats {
        TargetStats {
            target: "llm:openai".to_owned(),
            calls,
            attempts: calls,
            retries: 0,
            successes: calls,
            failures: 0,
            cache_hits: 0,
            throttled: 0,
            breaker_opens: 0,
            total_latency_ms: 0,
            max_latency_ms: 0,
            first_seen_ms,
            last_seen_ms,
            last_error_class: None,
            last_error_status: None,
        }
    }

    fn host_scan() -> ScanResult {
        let mut s = ScanResult {
            files_scanned: 2,
            python_available: true,
            ..ScanResult::default()
        };
        // Reuse the private add via a fresh evidence set.
        s.targets.insert(
            "api.example.com".to_owned(),
            TargetEvidence {
                class: TargetClass::Host,
                sightings: [scan::Sighting {
                    file: "app.py".into(),
                    line: 4,
                }]
                .into_iter()
                .collect(),
            },
        );
        s.targets.insert(
            "llm:openai".to_owned(),
            TargetEvidence {
                class: TargetClass::Llm,
                sightings: [scan::Sighting {
                    file: "app.py".into(),
                    line: 2,
                }]
                .into_iter()
                .collect(),
            },
        );
        s
    }

    #[test]
    fn header_counts_and_no_date_by_default() {
        let out = render_keel_toml(&host_scan(), &[], None);
        assert!(
            out.starts_with("# Generated by keel init from 2 static scans + 0 observed runs\n")
        );
        assert!(!out.contains("202"), "no date without --stamp");
    }

    #[test]
    fn stamp_adds_a_date() {
        let out = render_keel_toml(&host_scan(), &[], Some("2026-07-12"));
        assert!(out.lines().next().unwrap().ends_with(" (2026-07-12)"));
    }

    #[test]
    fn host_and_llm_blocks_cite_evidence() {
        let out = render_keel_toml(&host_scan(), &[], None);
        assert!(out.contains("[target.\"api.example.com\"]"));
        assert!(out.contains("# seen in: app.py:4"));
        assert!(out.contains("[target.\"llm:openai\"]"));
        assert!(out.contains("# seen in: app.py:2"));
        assert!(out.contains("cache   = { mode = \"dev\" }"));
    }

    #[test]
    fn discovery_only_target_is_surfaced_with_observed_comment() {
        let scan = ScanResult {
            files_scanned: 1,
            python_available: true,
            ..ScanResult::default()
        };
        let stats = TargetStats {
            target: "api.dynamic.com".to_owned(),
            calls: 120,
            attempts: 132,
            retries: 12,
            successes: 120,
            failures: 0,
            cache_hits: 0,
            throttled: 0,
            breaker_opens: 0,
            total_latency_ms: 12_000,
            max_latency_ms: 300,
            first_seen_ms: 0,
            last_seen_ms: 120_000, // 2 minutes → 60/min
            last_error_class: None,
            last_error_status: None,
        };
        let out = render_keel_toml(&scan, std::slice::from_ref(&stats), None);
        assert!(out.contains("[target.\"api.dynamic.com\"]"));
        assert!(out.contains("# seen only at runtime (.keel/discovery.db)"));
        assert!(out.contains("# observed: 120 calls, 12 retries, ~60.0/min mean"));
        // header now reports one observed run
        assert!(out.contains("+ 1 observed run\n"));
    }

    #[test]
    fn error_class_import_is_available() {
        // Guards the keel_journal re-export used by status/doctor tests too.
        let _ = ErrorClass::Http;
    }

    #[test]
    fn default_body_matches_the_frozen_pack() {
        // The hardcoded policy bodies must equal contracts/defaults.toml.
        let defaults: toml::Value = include_str!("../contract/defaults.toml")
            .parse()
            .expect("defaults.toml parses");
        let outbound = &defaults["defaults"]["outbound"];
        assert_eq!(outbound["timeout"].as_str(), Some("30s"));
        assert_eq!(outbound["retry"]["attempts"].as_integer(), Some(3));
        assert_eq!(outbound["breaker"]["cooldown"].as_str(), Some("15s"));
        let llm = &defaults["defaults"]["llm"];
        assert_eq!(llm["timeout"].as_str(), Some("120s"));
        assert_eq!(llm["retry"]["attempts"].as_integer(), Some(6));
        assert_eq!(llm["breaker"]["cooldown"].as_str(), Some("30s"));
        assert_eq!(llm["cache"]["mode"].as_str(), Some("dev"));
        // and the bodies we emit reflect those values
        assert!(policy_body(TargetClass::Host).contains("attempts = 3"));
        assert!(policy_body(TargetClass::Host).contains("cooldown = \"15s\""));
        assert!(policy_body(TargetClass::Llm).contains("attempts = 6"));
        assert!(policy_body(TargetClass::Llm).contains("cooldown = \"30s\""));
        assert!(policy_body(TargetClass::Llm).contains("mode = \"dev\""));
    }

    #[test]
    fn civil_date_epoch_is_1970_01_01() {
        assert_eq!(civil_from_days(0), (1970, 1, 1));
        assert_eq!(civil_from_days(19_997), (2024, 10, 1));
    }

    // ---- item 3: evidence-tuned llm rate derivation ----

    #[test]
    fn round_up_clean_walks_the_1_2_5_series() {
        assert_eq!(round_up_clean(0), 0);
        assert_eq!(round_up_clean(1), 1);
        assert_eq!(round_up_clean(3), 5);
        assert_eq!(round_up_clean(6), 10);
        assert_eq!(round_up_clean(11), 20);
        assert_eq!(round_up_clean(50), 50);
        assert_eq!(round_up_clean(60), 100);
        assert_eq!(round_up_clean(123), 200);
        assert_eq!(round_up_clean(300), 500);
        assert_eq!(round_up_clean(501), 1_000);
    }

    #[test]
    fn llm_rate_is_mean_times_three_rounded_up_to_a_clean_value() {
        // 200 calls over a 2-min window → mean 100/min → ×3 = 300 → clean 500.
        let s = llm_stats(200, 0, 120_000);
        assert_eq!(mean_per_min_floor(&s), 100);
        assert_eq!(llm_rate_per_min(&s), 500);
    }

    #[test]
    fn llm_rate_floors_at_60_for_sparse_traffic() {
        // 5 calls over 1 min → mean 5/min → ×3 = 15 → clean 20 → floored to 60.
        let s = llm_stats(5, 0, 60_000);
        assert_eq!(mean_per_min_floor(&s), 5);
        assert_eq!(llm_rate_per_min(&s), LLM_RATE_FLOOR_PER_MIN);
    }

    #[test]
    fn llm_rate_zero_span_window_falls_back_to_floor() {
        // Single-instant window (first_seen == last_seen): no mean derivable.
        let s = llm_stats(500, 1_000, 1_000);
        assert_eq!(mean_per_min_floor(&s), 0);
        assert_eq!(llm_rate_per_min(&s), LLM_RATE_FLOOR_PER_MIN);
    }

    #[test]
    fn observed_llm_target_gets_an_active_rate_line() {
        let scan = ScanResult {
            files_scanned: 1,
            python_available: true,
            ..ScanResult::default()
        };
        let stats = llm_stats(200, 0, 120_000);
        let out = render_keel_toml(&scan, std::slice::from_ref(&stats), None);

        assert!(out.contains("[target.\"llm:openai\"]"));
        assert!(out.contains("rate    = \"500/min\""));
        assert!(out.contains("# headroom over your observed mean of ~100/min"));
        // We measure a mean, never a peak — the word must never appear.
        assert!(!out.contains("peak"));
        // The rate line sits between breaker and cache.
        let rate_at = out.find("rate    =").expect("rate line present");
        let cache_at = out.find("cache   =").expect("cache line present");
        assert!(rate_at < cache_at, "rate must precede cache");
    }

    #[test]
    fn zero_span_llm_target_emits_floor_with_honest_comment() {
        let scan = ScanResult {
            files_scanned: 1,
            python_available: true,
            ..ScanResult::default()
        };
        let stats = llm_stats(9, 5_000, 5_000);
        let out = render_keel_toml(&scan, std::slice::from_ref(&stats), None);
        assert!(out.contains("rate    = \"60/min\""));
        assert!(out.contains("# floor: single observation window, no mean to derive"));
        assert!(!out.contains("peak"));
    }

    #[test]
    fn observed_host_target_stays_comments_only() {
        // Host targets never get an active rate, even with observed traffic.
        let scan = ScanResult {
            files_scanned: 1,
            python_available: true,
            ..ScanResult::default()
        };
        let stats = TargetStats {
            target: "api.host.example".to_owned(),
            ..llm_stats(200, 0, 120_000)
        };
        let out = render_keel_toml(&scan, std::slice::from_ref(&stats), None);
        assert!(out.contains("[target.\"api.host.example\"]"));
        assert!(
            !out.contains("rate    ="),
            "host targets must not emit an active rate line"
        );
    }

    // ---- item 2: keel init write path ----

    #[test]
    fn refuses_when_keel_toml_already_exists() {
        let dir = TempDir::new().unwrap();
        fs::write(dir.path().join("keel.toml"), "# hand-written\n").unwrap();

        let r = run(dir.path(), InitOptions::default());

        assert_eq!(r.exit, EXIT_USAGE);
        assert!(r.to_stderr);
        assert!(r.human.contains("already exists"));
        // The existing file is left untouched.
        assert_eq!(
            fs::read_to_string(dir.path().join("keel.toml")).unwrap(),
            "# hand-written\n"
        );
    }

    #[test]
    fn diff_reports_added_and_removed_targets_precisely() {
        let dir = TempDir::new().unwrap();
        // JS scan (pure Rust, no python3) will find `api.example.com`.
        fs::write(
            dir.path().join("app.mjs"),
            "const r = await fetch(\"https://api.example.com/v1/x\");\n",
        )
        .unwrap();
        // An existing keel.toml declares a target the scan will NOT find.
        fs::write(
            dir.path().join("keel.toml"),
            "[target.\"api.gone.example\"]\ntimeout = \"30s\"\n",
        )
        .unwrap();

        let r = run(
            dir.path(),
            InitOptions {
                diff: true,
                stamp: false,
                agents: false,
            },
        );

        assert_eq!(r.exit, crate::EXIT_OK);
        assert_eq!(
            r.json["added"].as_array().unwrap(),
            &vec![serde_json::json!("api.example.com")]
        );
        assert_eq!(
            r.json["removed"].as_array().unwrap(),
            &vec![serde_json::json!("api.gone.example")]
        );
        assert!(r.json["unchanged"].as_array().unwrap().is_empty());
        assert!(r.human.contains("+ [target.\"api.example.com\"]"));
        assert!(r.human.contains("- [target.\"api.gone.example\"]"));
        // --diff never writes.
        assert_eq!(
            fs::read_to_string(dir.path().join("keel.toml")).unwrap(),
            "[target.\"api.gone.example\"]\ntimeout = \"30s\"\n"
        );
    }

    // ---- keel init --agents ----

    #[test]
    fn agents_creates_then_is_idempotent() {
        let dir = TempDir::new().unwrap();
        let opts = InitOptions {
            agents: true,
            ..InitOptions::default()
        };
        let r1 = run(dir.path(), opts);
        assert_eq!(r1.exit, crate::EXIT_OK);
        assert!(r1.json["wrote"].as_bool().unwrap());
        let path = dir.path().join("AGENTS.md");
        let c1 = fs::read_to_string(&path).unwrap();
        assert!(c1.contains("## Keel (resilience & durable execution)"));
        assert!(c1.contains("keel doctor --json"));

        // Re-run: nothing to change → already current, file byte-identical.
        let r2 = run(dir.path(), opts);
        assert!(r2.json["already_current"].as_bool().unwrap());
        assert_eq!(fs::read_to_string(&path).unwrap(), c1);
    }

    #[test]
    fn splice_appends_then_replaces_region_without_duplicating() {
        let block = agents_block();
        // Append below existing prose.
        let (out, replaced, wrote) = splice_agents_block("# My project\n", &block);
        assert!(!replaced && wrote);
        assert!(out.starts_with("# My project\n\n"));
        assert!(out.contains(AGENTS_BEGIN) && out.contains(AGENTS_END));
        // Re-splicing the same block replaces in place and is a no-op write.
        let (out2, replaced2, wrote2) = splice_agents_block(&out, &block);
        assert!(replaced2 && !wrote2);
        assert_eq!(out2, out);
        assert_eq!(
            out2.matches(AGENTS_BEGIN).count(),
            1,
            "exactly one Keel block"
        );
    }

    #[test]
    fn gitignore_is_created_when_absent() {
        let dir = TempDir::new().unwrap();
        assert!(update_gitignore(dir.path()).unwrap());
        assert_eq!(
            fs::read_to_string(dir.path().join(".gitignore")).unwrap(),
            ".keel/\n"
        );
    }

    #[test]
    fn gitignore_is_appended_when_keel_line_missing() {
        let dir = TempDir::new().unwrap();
        // No trailing newline: the appender must add one before `.keel/`.
        fs::write(dir.path().join(".gitignore"), "node_modules/\n*.log").unwrap();

        assert!(update_gitignore(dir.path()).unwrap());

        assert_eq!(
            fs::read_to_string(dir.path().join(".gitignore")).unwrap(),
            "node_modules/\n*.log\n.keel/\n"
        );
    }

    #[test]
    fn gitignore_is_a_noop_when_already_ignored() {
        let dir = TempDir::new().unwrap();
        let original = "build/\n.keel/\ncoverage/\n";
        fs::write(dir.path().join(".gitignore"), original).unwrap();

        assert!(!update_gitignore(dir.path()).unwrap());

        assert_eq!(
            fs::read_to_string(dir.path().join(".gitignore")).unwrap(),
            original
        );
    }
}
