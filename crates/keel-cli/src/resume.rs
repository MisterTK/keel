//! `keel flows resume <FLOW> [-- ARGS…]` / `keel flows resume --all` —
//! re-invoke a resumable flow's recorded entrypoint through `keel run`
//! semantics (architecture spec §4.3 "Recovery … re-execute"; §6 laptop model
//! "a crashed script resumes when re-invoked").
//!
//! ## What the journal actually lets the CLI know
//!
//! The frozen schema (`contracts/journal.sql`) records a flow's *identity* —
//! `entrypoint` (`"py:<module>:<function>"`) and `args_hash` (a one-way
//! digest) — never the script path the user originally typed at
//! `keel run <script>`, nor the literal CLI arguments. That is enough to
//! recognize a flow's provenance but never enough to reconstruct the exact
//! command that created it. This command is the honest middle ground between
//! pure re-invocation (dx-spec/§6, which asks the operator to remember the
//! command) and a fully automatic background scan (§4.3, which would face the
//! identical gap with nobody present to fill it in) — it does the lookup and
//! the eligibility checks, and is explicit about what it cannot know:
//!
//! - **The script.** A `py:<module>:<function>` entrypoint's module maps to a
//!   candidate file the same way the Python front end's `match_flow`
//!   (`python/keel/src/keel/_flow.py`) matches one on the way in: a dotted
//!   module (`jobs.nightly`) names an exact path (`jobs/nightly.py`); a
//!   single-component module (`pipeline`) matches any `pipeline.py` under the
//!   project, so this command searches the tree for one. Zero matches, or more
//!   than one for a single-component module, and it stops rather than guess.
//! - **The arguments.** `args_hash` cannot be inverted. Passing none
//!   reproduces the original flow_id only when the original invocation also
//!   had none; otherwise the front end derives a *different* deterministic
//!   flow_id (spec §4.3) and a fresh flow starts next to the one this command
//!   was asked to resume. Pass the original arguments after `--` when the
//!   flow took any. After the child exits, this command re-reads the flow's
//!   `updated_at` and says plainly when it looks like the wrong flow ran.
//!
//! Non-`py:` entrypoints (no other front end designates durable flows yet),
//! `completed` flows (nothing to resume — see `keel replay`), and `dead` flows
//! (KEEL-E032, never auto-resumed) are refused with a precise what/why/next;
//! see [`Ineligible`].

use std::path::{Path, PathBuf};

use rusqlite::Connection;
use serde::Serialize;

use crate::render::to_json;
use crate::{EXIT_FAILURE, EXIT_OK, EXIT_USAGE, Rendered, evidence, flows, run};

/// `keel flows resume` options: either one `flow` (id or substring) or `all`,
/// never both — `args` only makes sense for a single flow.
#[derive(Debug, Clone, Default)]
pub struct ResumeOptions {
    /// A flow_id, or a substring of an id/entrypoint that names one flow.
    pub flow: Option<String>,
    /// Resume every currently-resumable flow instead of one.
    pub all: bool,
    /// Arguments forwarded to the resumed script (single-flow only).
    pub args: Vec<String>,
}

/// One flow row with the columns eligibility needs (a superset of
/// [`flows::ResolvedFlow`], which does not carry the lease/age columns).
struct FlowRow {
    flow_id: String,
    entrypoint: String,
    status: String,
    lease_holder: Option<String>,
    lease_expires: Option<i64>,
    updated_at: i64,
}

/// Why a flow cannot be resumed from the CLI right now — each renders as a
/// precise what/why/next.
#[derive(Debug)]
enum Ineligible {
    /// Nothing to resume; `keel replay` inspects a completed flow instead.
    Completed,
    /// Dead flows are never auto-resumed (KEEL-E032).
    Dead,
    /// A lease not yet expired: another process may still hold it (KEEL-E030).
    LeaseLive { holder: String, expires: i64 },
    /// The entrypoint scheme this CLI knows how to re-invoke is `py:` only.
    UnsupportedEntrypoint,
    /// The module's file could not be found under the project.
    ScriptNotFound { candidate: String },
    /// A single-component module matched more than one file; resuming the
    /// wrong one would replay a stranger's steps, so this refuses to guess.
    AmbiguousScript { candidates: Vec<String> },
}

impl Ineligible {
    fn message(&self, row: &FlowRow) -> String {
        let flow_id = &row.flow_id;
        match self {
            Self::Completed => format!(
                "flow {flow_id} is already completed; nothing to resume. Inspect it with `keel \
                 replay {flow_id}`, or run the script again to start a new flow."
            ),
            Self::Dead => format!(
                "flow {flow_id} is dead \u{2014} dead flows are never auto-resumed (KEEL-E032). \
                 Inspect with `keel trace {flow_id}`, fix the cause, then rerun with a new \
                 identity; see `keel explain KEEL-E032`."
            ),
            Self::LeaseLive { holder, expires } => format!(
                "flow {flow_id}'s lease is held by {holder} until {expires} (KEEL-E030); another \
                 process may still be running it. Wait for the lease to expire (or confirm that \
                 process is gone) and retry; see `keel explain KEEL-E030`."
            ),
            Self::UnsupportedEntrypoint => format!(
                "flow {flow_id} ({}) cannot be resumed from the CLI: only `py:` entrypoints can \
                 be re-invoked today (no other front end designates durable flows yet). \
                 Re-invoke the original script directly with `keel run`.",
                row.entrypoint
            ),
            Self::ScriptNotFound { candidate } => format!(
                "flow {flow_id} ({}) cannot be resumed from the CLI: expected its script at \
                 `{candidate}` (derived from the entrypoint's module), but no such file exists \
                 under the project. If the module lives elsewhere on PYTHONPATH, run `keel run \
                 <script>` on it directly instead.",
                row.entrypoint
            ),
            Self::AmbiguousScript { candidates } => format!(
                "flow {flow_id} ({}) cannot be resumed from the CLI: its single-component module \
                 matches {} files under the project ({}) and this command will not guess which \
                 one wrote this journal. Run `keel run <script>` on the right one directly \
                 instead.",
                row.entrypoint,
                candidates.len(),
                candidates.join(", ")
            ),
        }
    }
}

/// The `py:` entrypoint scheme this CLI knows how to re-invoke (Tier 2 durable
/// flows are Python-only as of this build).
const PY_SCHEME: &str = "py";

/// Split `"py:pipeline.ingest:main"` into `(lang, module, function)`.
fn parse_entrypoint(entrypoint: &str) -> Option<(&str, &str, &str)> {
    let mut parts = entrypoint.splitn(3, ':');
    Some((parts.next()?, parts.next()?, parts.next()?))
}

/// Every file under `dir` (skipping [`crate::scan::SKIP_DIRS`], never
/// following symlinks — `DirEntry::file_type` reports the symlink itself,
/// not its target) whose file name is exactly `name`. Sorted for
/// deterministic ambiguity reports.
fn find_files_named(dir: &Path, name: &str, out: &mut Vec<PathBuf>) {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return;
    };
    for entry in entries.flatten() {
        let Ok(file_type) = entry.file_type() else {
            continue;
        };
        if file_type.is_dir() {
            let skip = entry
                .file_name()
                .to_str()
                .is_some_and(|n| crate::scan::SKIP_DIRS.contains(&n));
            if !skip {
                find_files_named(&entry.path(), name, out);
            }
        } else if file_type.is_file() && entry.file_name().to_str() == Some(name) {
            out.push(entry.path());
        }
    }
}

/// The exact path a dotted module names: `"jobs.nightly"` → `jobs/nightly.py`
/// under `project` (the module-path suffix `_flow.py::match_flow` requires,
/// applied in reverse).
fn dotted_module_path(project: &Path, module: &str) -> PathBuf {
    let mut path = project.to_path_buf();
    for part in module.split('.') {
        path.push(part);
    }
    path.set_extension("py");
    path
}

/// Where a module's script lives, or why that cannot be determined — see the
/// module docs for the single-component-vs-dotted matching rule.
enum ScriptLookup {
    Found(PathBuf),
    NotFound { candidate: String },
    Ambiguous { candidates: Vec<String> },
}

fn locate_script(project: &Path, module: &str) -> ScriptLookup {
    if module.contains('.') {
        let candidate = dotted_module_path(project, module);
        if candidate.is_file() {
            ScriptLookup::Found(candidate)
        } else {
            ScriptLookup::NotFound {
                candidate: candidate.display().to_string(),
            }
        }
    } else {
        let name = format!("{module}.py");
        let mut found = Vec::new();
        find_files_named(project, &name, &mut found);
        found.sort();
        match found.len() {
            0 => ScriptLookup::NotFound {
                candidate: project.join(&name).display().to_string(),
            },
            1 => ScriptLookup::Found(found.into_iter().next().expect("exactly one")),
            _ => ScriptLookup::Ambiguous {
                candidates: found.iter().map(|p| p.display().to_string()).collect(),
            },
        }
    }
}

/// Whether `row` can be resumed right now, and if so, the script to re-invoke.
/// Resumability here matches `keel replay`'s notion (`running` **or**
/// `failed` — both resume through the core's `enter()`; only `completed` is
/// pure replay and only `dead` is refused), which is broader than
/// `SqliteJournal::incomplete_flows`'s narrower recovery-scan set
/// (`running` only, per `crates/keel-journal/src/sqlite.rs`).
fn eligibility(project: &Path, row: &FlowRow, now_ms: i64) -> Result<PathBuf, Ineligible> {
    match row.status.as_str() {
        "completed" => return Err(Ineligible::Completed),
        "dead" => return Err(Ineligible::Dead),
        _ => {}
    }
    if let (Some(holder), Some(expires)) = (&row.lease_holder, row.lease_expires)
        && expires > now_ms
    {
        return Err(Ineligible::LeaseLive {
            holder: holder.clone(),
            expires,
        });
    }
    let Some((lang, module, _function)) = parse_entrypoint(&row.entrypoint) else {
        return Err(Ineligible::UnsupportedEntrypoint);
    };
    if lang != PY_SCHEME {
        return Err(Ineligible::UnsupportedEntrypoint);
    }
    match locate_script(project, module) {
        ScriptLookup::Found(path) => Ok(path),
        ScriptLookup::NotFound { candidate } => Err(Ineligible::ScriptNotFound { candidate }),
        ScriptLookup::Ambiguous { candidates } => Err(Ineligible::AmbiguousScript { candidates }),
    }
}

/// Read one flow's resume-relevant columns by exact id.
fn read_flow_row(conn: &Connection, flow_id: &str) -> Result<FlowRow, String> {
    conn.query_row(
        "SELECT flow_id, entrypoint, status, lease_holder, lease_expires, updated_at \
         FROM flows WHERE flow_id = ?1",
        [flow_id],
        |r| {
            Ok(FlowRow {
                flow_id: r.get(0)?,
                entrypoint: r.get(1)?,
                status: r.get(2)?,
                lease_holder: r.get(3)?,
                lease_expires: r.get(4)?,
                updated_at: r.get(5)?,
            })
        },
    )
    .map_err(|e| flows::q(&e))
}

/// Every `running`/`failed` flow, sorted by id — the candidate set `--all`
/// filters through [`eligibility`].
fn resumable_candidates(conn: &Connection) -> Result<Vec<FlowRow>, String> {
    let mut stmt = conn
        .prepare(
            "SELECT flow_id, entrypoint, status, lease_holder, lease_expires, updated_at \
             FROM flows WHERE status IN ('running', 'failed') ORDER BY flow_id",
        )
        .map_err(|e| flows::q(&e))?;
    stmt.query_map([], |r| {
        Ok(FlowRow {
            flow_id: r.get(0)?,
            entrypoint: r.get(1)?,
            status: r.get(2)?,
            lease_holder: r.get(3)?,
            lease_expires: r.get(4)?,
            updated_at: r.get(5)?,
        })
    })
    .map_err(|e| flows::q(&e))?
    .collect::<rusqlite::Result<Vec<_>>>()
    .map_err(|e| flows::q(&e))
}

/// `keel flows resume` for `project`: dispatch to the single-flow or `--all`
/// path, judging leases and the retention-adjacent "already gone" case
/// against `now_ms`. `Rendered` is only ever produced for a precondition
/// failure or the `--all` summary — a single successful resume is invisible
/// on the way in (like `keel run`), because the child IS the resume.
pub fn run(project: &Path, options: &ResumeOptions, now_ms: i64) -> (Option<Rendered>, i32) {
    if options.flow.is_some() == options.all {
        return usage_pair(if options.all {
            "cannot give both a FLOW_ID and --all; resume one flow by id, or every resumable \
             flow with --all alone."
        } else {
            "`keel flows resume` needs a FLOW_ID, or --all to resume every resumable flow."
        });
    }
    if options.all && !options.args.is_empty() {
        return usage_pair(
            "cannot forward arguments with --all: different flows may need different original \
             arguments. Resume them individually: `keel flows resume <FLOW> -- <args>`.",
        );
    }

    let path = evidence::resolved_journal(project).path;
    if !path.exists() {
        return soft_pair("no journal yet (.keel/journal.db). Run a flow first with `keel run`.");
    }
    let conn = match flows::open_ro(&path) {
        Ok(c) => c,
        Err(e) => return soft_pair(&e),
    };

    if options.all {
        resume_all(project, &conn, now_ms)
    } else {
        let flow = options.flow.as_deref().expect("checked above");
        resume_one(project, &conn, flow, &options.args, now_ms)
    }
}

fn resume_one(
    project: &Path,
    conn: &Connection,
    flow: &str,
    args: &[String],
    now_ms: i64,
) -> (Option<Rendered>, i32) {
    let resolved = match flows::resolve_flow(conn, flow) {
        Ok(r) => r,
        Err(e) => return soft_pair(&e),
    };
    let row = match read_flow_row(conn, &resolved.flow_id) {
        Ok(r) => r,
        Err(e) => return soft_pair(&e),
    };
    let script = match eligibility(project, &row, now_ms) {
        Ok(s) => s,
        Err(ineligible) => return soft_pair(&ineligible.message(&row)),
    };
    let plan = match run::plan(&script.to_string_lossy(), args, false) {
        Ok(p) => p,
        Err(e) => {
            return soft_pair(&format!(
                "could not plan a run for {}: {e:?}",
                script.display()
            ));
        }
    };
    announce(&row, &script, args);
    match run::exec(&plan) {
        Ok(code) => {
            if code == EXIT_OK && !progressed(conn, &row) {
                eprintln!(
                    "keel \u{25b8} flow {} does not look resumed \u{2014} its journal record did \
                     not change. Check that the arguments after `--` matched the original \
                     invocation exactly (see `keel explain KEEL-E040` if this is unexpected).",
                    row.flow_id
                );
            }
            (None, code)
        }
        Err(r) => {
            let code = r.exit;
            (Some(r), code)
        }
    }
}

/// One `--all` attempt's outcome.
#[derive(Debug, Serialize)]
struct AttemptEntry {
    exit_code: i32,
    flow_id: String,
    /// Whether the flow's journal record changed after the child ran — a
    /// signal (not a guarantee) that the intended flow, not a fresh one with
    /// mismatched arguments, actually progressed.
    progressed: bool,
    script: String,
}

/// One flow `--all` could not attempt, and why.
#[derive(Debug, Serialize)]
struct SkipEntry {
    flow_id: String,
    reason: String,
}

/// The `keel flows resume --all` report.
#[derive(Debug, Serialize)]
struct AllReport {
    attempted: Vec<AttemptEntry>,
    ok: bool,
    skipped: Vec<SkipEntry>,
}

fn resume_all(project: &Path, conn: &Connection, now_ms: i64) -> (Option<Rendered>, i32) {
    let candidates = match resumable_candidates(conn) {
        Ok(c) => c,
        Err(e) => return soft_pair(&e),
    };
    let mut attempted = Vec::new();
    let mut skipped = Vec::new();
    for row in candidates {
        match eligibility(project, &row, now_ms) {
            Err(ineligible) => skipped.push(SkipEntry {
                flow_id: row.flow_id.clone(),
                reason: ineligible.message(&row),
            }),
            Ok(script) => {
                let plan = match run::plan(&script.to_string_lossy(), &[], false) {
                    Ok(p) => p,
                    Err(e) => {
                        skipped.push(SkipEntry {
                            flow_id: row.flow_id.clone(),
                            reason: format!("could not plan a run for {}: {e:?}", script.display()),
                        });
                        continue;
                    }
                };
                announce(&row, &script, &[]);
                let exit_code = match run::exec(&plan) {
                    Ok(code) => code,
                    Err(rendered) => {
                        eprint!("{}", rendered.human);
                        EXIT_FAILURE
                    }
                };
                attempted.push(AttemptEntry {
                    exit_code,
                    progressed: progressed(conn, &row),
                    flow_id: row.flow_id,
                    script: script.display().to_string(),
                });
            }
        }
    }
    // A flow this pass could not even attempt (a live lease, an unresolvable
    // script) is exactly as much an outstanding finding as a failed attempt —
    // `ok` requires an empty `skipped` too, not just successful attempts (the
    // vacuous "all attempts ok" over zero attempts must not read as success).
    let ok = skipped.is_empty()
        && attempted
            .iter()
            .all(|a| a.exit_code == EXIT_OK && a.progressed);
    let report = AllReport {
        attempted,
        ok,
        skipped,
    };
    let human = all_human(&report);
    let exit = if ok { EXIT_OK } else { EXIT_FAILURE };
    (
        Some(Rendered {
            human,
            json: to_json(&report),
            exit,
            to_stderr: false,
        }),
        exit,
    )
}

fn all_human(report: &AllReport) -> String {
    if report.attempted.is_empty() && report.skipped.is_empty() {
        return "keel \u{25b8} no resumable flows (running/failed with no live lease).".to_owned();
    }
    let mut lines = vec![format!(
        "keel \u{25b8} flows resume --all: {} attempted, {} skipped\n",
        report.attempted.len(),
        report.skipped.len()
    )];
    for a in &report.attempted {
        let flag = if a.exit_code == EXIT_OK && a.progressed {
            "ok"
        } else if a.exit_code != EXIT_OK {
            "child failed"
        } else {
            "did not progress"
        };
        lines.push(format!(
            "  {}  {}  exit {} \u{2014} {flag}\n",
            a.flow_id, a.script, a.exit_code
        ));
    }
    for s in &report.skipped {
        lines.push(format!("  {}  skipped: {}\n", s.flow_id, s.reason));
    }
    lines.concat()
}

/// Print the resume notice (and, for a no-args single resume, the honest
/// caveat about `args_hash`) to stderr before spawning.
fn announce(row: &FlowRow, script: &Path, args: &[String]) {
    let extra = if args.is_empty() {
        String::new()
    } else {
        format!(" {}", args.join(" "))
    };
    eprintln!(
        "keel \u{25b8} resuming flow {} ({}) via `keel run {}{extra}`",
        row.flow_id,
        row.entrypoint,
        script.display()
    );
    if args.is_empty() {
        eprintln!(
            "  note: no arguments given \u{2014} this only resumes flow {} if its original \
             invocation also had none; otherwise a new flow starts. Pass the original arguments \
             after `--` to match.",
            row.flow_id
        );
    }
}

/// Whether `row`'s flow record changed since it was read (a signal, not a
/// guarantee, that the flow this command meant to resume is the one that
/// ran). Best-effort: a read failure counts as "did not progress" rather than
/// aborting a resume that has already happened.
fn progressed(conn: &Connection, row: &FlowRow) -> bool {
    read_flow_row(conn, &row.flow_id)
        .is_ok_and(|after| after.updated_at != row.updated_at || after.status != row.status)
}

fn usage_pair(message: &str) -> (Option<Rendered>, i32) {
    #[derive(Serialize)]
    struct UsageReport<'a> {
        error: &'static str,
        what: &'a str,
    }
    let human = format!("keel \u{25b8} {message}");
    let r = Rendered {
        human,
        json: to_json(&UsageReport {
            error: "bad-usage",
            what: message,
        }),
        exit: EXIT_USAGE,
        to_stderr: true,
    };
    (Some(r), EXIT_USAGE)
}

fn soft_pair(message: &str) -> (Option<Rendered>, i32) {
    let r = flows::soft_error(message);
    let code = r.exit;
    (Some(r), code)
}

#[cfg(test)]
mod tests {
    use super::*;
    use rusqlite::params;
    use std::path::PathBuf;

    const T0: i64 = 1_783_728_000_000;

    /// A project dir with `.keel/journal.db` built from the frozen schema plus
    /// the named golden fixtures, ready for a real script tree to be added.
    fn project_with_fixtures(fixtures: &[&str]) -> (tempfile::TempDir, PathBuf) {
        let root = Path::new(env!("CARGO_MANIFEST_DIR")).join("../..");
        let dir = tempfile::TempDir::new().unwrap();
        let keel = dir.path().join(".keel");
        std::fs::create_dir_all(&keel).unwrap();
        let conn = Connection::open(keel.join("journal.db")).unwrap();
        let schema = std::fs::read_to_string(root.join("contracts/journal.sql")).unwrap();
        conn.execute_batch(&schema).unwrap();
        for f in fixtures {
            let sql =
                std::fs::read_to_string(root.join("conformance/fixtures/journal").join(f)).unwrap();
            conn.execute_batch(&sql).unwrap();
        }
        let project = dir.path().to_path_buf();
        (dir, project)
    }

    fn write_script(project: &Path, rel: &str) {
        let path = project.join(rel);
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(&path, "def main():\n    pass\n").unwrap();
    }

    #[test]
    fn missing_journal_is_a_soft_error() {
        let dir = tempfile::TempDir::new().unwrap();
        let (r, code) = run(
            dir.path(),
            &ResumeOptions {
                flow: Some("anything".to_owned()),
                ..Default::default()
            },
            T0,
        );
        assert_eq!(code, EXIT_FAILURE);
        assert!(r.unwrap().human.contains("no journal yet"));
    }

    #[test]
    fn neither_flow_nor_all_is_a_usage_error() {
        let (_d, project) = project_with_fixtures(&["completed-flow.sql"]);
        let (r, code) = run(&project, &ResumeOptions::default(), T0);
        assert_eq!(code, EXIT_USAGE);
        assert!(r.unwrap().human.contains("needs a FLOW_ID"));
    }

    #[test]
    fn both_flow_and_all_is_a_usage_error() {
        let (_d, project) = project_with_fixtures(&["completed-flow.sql"]);
        let (r, code) = run(
            &project,
            &ResumeOptions {
                flow: Some("x".to_owned()),
                all: true,
                ..Default::default()
            },
            T0,
        );
        assert_eq!(code, EXIT_USAGE);
        assert!(r.unwrap().human.contains("cannot give both"));
    }

    #[test]
    fn args_with_all_is_a_usage_error() {
        let (_d, project) = project_with_fixtures(&["completed-flow.sql"]);
        let (r, code) = run(
            &project,
            &ResumeOptions {
                all: true,
                args: vec!["--x".to_owned()],
                ..Default::default()
            },
            T0,
        );
        assert_eq!(code, EXIT_USAGE);
        assert!(r.unwrap().human.contains("cannot forward arguments"));
    }

    #[test]
    fn completed_flow_is_refused_as_nothing_to_resume() {
        let (_d, project) = project_with_fixtures(&["completed-flow.sql"]);
        let (r, code) = run(
            &project,
            &ResumeOptions {
                flow: Some("01JZWY0A0000000000000001".to_owned()),
                ..Default::default()
            },
            T0,
        );
        assert_eq!(code, EXIT_FAILURE);
        let human = r.unwrap().human;
        assert!(human.contains("already completed"));
        assert!(human.contains("keel replay"));
    }

    #[test]
    fn dead_flow_is_refused_with_e032() {
        let (_d, project) = project_with_fixtures(&["dead-flow.sql"]);
        let (r, code) = run(
            &project,
            &ResumeOptions {
                flow: Some("01JZWY0A0000000000000003".to_owned()),
                ..Default::default()
            },
            T0,
        );
        assert_eq!(code, EXIT_FAILURE);
        let human = r.unwrap().human;
        assert!(human.contains("KEEL-E032"));
        assert!(human.contains("keel trace"));
    }

    #[test]
    fn live_lease_is_refused_with_e030() {
        let (_d, project) = project_with_fixtures(&["interrupted-flow.sql"]);
        write_script(&project, "pipeline/ingest.py");
        // The fixture's lease expires at T0+30s; ask before that.
        let (r, code) = run(
            &project,
            &ResumeOptions {
                flow: Some("01JZWY0A0000000000000002".to_owned()),
                ..Default::default()
            },
            T0 + 1_000,
        );
        assert_eq!(code, EXIT_FAILURE);
        let human = r.unwrap().human;
        assert!(human.contains("KEEL-E030"));
        assert!(human.contains("host-a:pid-4242"));
    }

    #[test]
    fn expired_lease_with_missing_script_reports_the_expected_path() {
        let (_d, project) = project_with_fixtures(&["interrupted-flow.sql"]);
        // No pipeline/ingest.py written: past the lease, script is still missing.
        let (r, code) = run(
            &project,
            &ResumeOptions {
                flow: Some("01JZWY0A0000000000000002".to_owned()),
                ..Default::default()
            },
            T0 + 60_000,
        );
        assert_eq!(code, EXIT_FAILURE);
        let human = r.unwrap().human;
        assert!(human.contains("cannot be resumed from the CLI"));
        assert!(human.contains("pipeline"));
        assert!(human.ends_with("directly instead.\n") || human.contains("directly instead."));
    }

    #[test]
    fn dotted_module_resolves_to_the_exact_nested_path() {
        let (_d, project) = project_with_fixtures(&["dead-flow.sql"]);
        // dead-flow.sql uses py:jobs.nightly:run — a dotted module. Even with a
        // decoy jobs/nightly.py, a dead flow is refused before script lookup.
        write_script(&project, "jobs/nightly.py");
        let script = locate_script(&project, "jobs.nightly");
        match script {
            ScriptLookup::Found(p) => assert_eq!(p, project.join("jobs").join("nightly.py")),
            _ => panic!("expected the dotted module to resolve"),
        }
    }

    #[test]
    fn single_component_module_is_found_anywhere_under_the_project() {
        let dir = tempfile::TempDir::new().unwrap();
        write_script(dir.path(), "scripts/pipeline.py");
        match locate_script(dir.path(), "pipeline") {
            ScriptLookup::Found(p) => {
                assert_eq!(p, dir.path().join("scripts").join("pipeline.py"));
            }
            _ => panic!("expected a single-component module match"),
        }
    }

    #[test]
    fn single_component_module_ambiguity_refuses_to_guess() {
        let dir = tempfile::TempDir::new().unwrap();
        write_script(dir.path(), "a/pipeline.py");
        write_script(dir.path(), "b/pipeline.py");
        match locate_script(dir.path(), "pipeline") {
            ScriptLookup::Ambiguous { candidates } => assert_eq!(candidates.len(), 2),
            _ => panic!("expected ambiguity"),
        }
    }

    #[test]
    fn tree_walk_skips_dependency_and_vcs_directories() {
        let dir = tempfile::TempDir::new().unwrap();
        write_script(dir.path(), "src/pipeline.py");
        write_script(dir.path(), "node_modules/pkg/pipeline.py");
        write_script(dir.path(), ".venv/lib/pipeline.py");
        match locate_script(dir.path(), "pipeline") {
            ScriptLookup::Found(p) => assert_eq!(p, dir.path().join("src").join("pipeline.py")),
            _ => panic!("expected exactly one match outside skipped directories"),
        }
    }

    #[test]
    fn unsupported_entrypoint_scheme_is_refused() {
        let (_d, project) = project_with_fixtures(&["completed-flow.sql"]);
        let conn = Connection::open(project.join(".keel/journal.db")).unwrap();
        conn.execute(
            "INSERT INTO flows (flow_id, entrypoint, args_hash, status, created_at, updated_at) \
             VALUES ('01NODEFLOW', 'js:server.mjs:handler', 'ah-1', 'running', ?1, ?1)",
            params![T0],
        )
        .unwrap();
        let (r, code) = run(
            &project,
            &ResumeOptions {
                flow: Some("01NODEFLOW".to_owned()),
                ..Default::default()
            },
            T0,
        );
        assert_eq!(code, EXIT_FAILURE);
        let human = r.unwrap().human;
        assert!(human.contains("only `py:` entrypoints"));
    }

    #[test]
    fn ambiguous_flow_lookup_is_a_soft_error_before_eligibility() {
        // Two flows sharing a substring: resolve_flow's ambiguity fires first.
        let (_d, project) = project_with_fixtures(&["completed-flow.sql", "dead-flow.sql"]);
        let (r, code) = run(
            &project,
            &ResumeOptions {
                flow: Some("01JZWY0A".to_owned()),
                ..Default::default()
            },
            T0,
        );
        assert_eq!(code, EXIT_FAILURE);
        assert!(r.unwrap().human.contains("matches"));
    }

    #[test]
    fn unknown_flow_is_a_soft_error() {
        let (_d, project) = project_with_fixtures(&["completed-flow.sql"]);
        let (r, code) = run(
            &project,
            &ResumeOptions {
                flow: Some("does-not-exist".to_owned()),
                ..Default::default()
            },
            T0,
        );
        assert_eq!(code, EXIT_FAILURE);
        assert!(r.unwrap().human.contains("no flow matches"));
    }

    #[test]
    fn eligible_flow_builds_the_expected_run_plan() {
        // A failed flow (no lease, per complete_flow's always-clear rule) with
        // its script present is eligible: the resume machinery hands off to
        // `run::plan`, which we can assert on without spawning python3.
        let (_d, project) = project_with_fixtures(&["completed-flow.sql"]);
        let conn = Connection::open(project.join(".keel/journal.db")).unwrap();
        conn.execute(
            "INSERT INTO flows (flow_id, entrypoint, args_hash, status, created_at, updated_at) \
             VALUES ('01FAILEDFLOW', 'py:pipeline.retry:main', 'ah-2', 'failed', ?1, ?1)",
            params![T0],
        )
        .unwrap();
        write_script(&project, "pipeline/retry.py");
        let row = read_flow_row(&conn, "01FAILEDFLOW").unwrap();
        let script = eligibility(&project, &row, T0).expect("eligible");
        assert_eq!(script, project.join("pipeline").join("retry.py"));
        let plan = run::plan(&script.to_string_lossy(), &["--x".to_owned()], false).unwrap();
        assert_eq!(plan.program, "python3");
        assert_eq!(plan.argv[0], "-m");
        assert_eq!(plan.argv[1], "keel");
        assert_eq!(plan.argv[2], "run");
        assert!(plan.argv[3].ends_with("retry.py"));
        assert_eq!(plan.argv[4], "--x");
    }

    #[test]
    fn resumable_candidates_excludes_completed_and_dead() {
        let (_d, project) = project_with_fixtures(&[
            "completed-flow.sql",
            "interrupted-flow.sql",
            "dead-flow.sql",
        ]);
        let conn = Connection::open(project.join(".keel/journal.db")).unwrap();
        let ids: Vec<String> = resumable_candidates(&conn)
            .unwrap()
            .into_iter()
            .map(|r| r.flow_id)
            .collect();
        assert_eq!(ids, vec!["01JZWY0A0000000000000002"]); // only the running one
    }

    #[test]
    fn resume_all_with_nothing_resumable_is_a_clean_no_op() {
        let (_d, project) = project_with_fixtures(&["completed-flow.sql", "dead-flow.sql"]);
        let (r, code) = run(
            &project,
            &ResumeOptions {
                all: true,
                ..Default::default()
            },
            T0,
        );
        assert_eq!(code, EXIT_OK);
        let rendered = r.unwrap();
        assert!(rendered.human.contains("no resumable flows"));
        assert_eq!(rendered.json["attempted"].as_array().unwrap().len(), 0);
        assert_eq!(rendered.json["skipped"].as_array().unwrap().len(), 0);
    }

    #[test]
    fn resume_all_skips_ineligible_candidates_with_reasons() {
        // Running flow, lease still live at this `now`: skipped, not attempted.
        let (_d, project) = project_with_fixtures(&["interrupted-flow.sql"]);
        let (r, code) = run(
            &project,
            &ResumeOptions {
                all: true,
                ..Default::default()
            },
            T0 + 1_000, // before the fixture's lease expiry
        );
        assert_eq!(code, EXIT_FAILURE); // nothing attempted successfully
        let rendered = r.unwrap();
        assert_eq!(rendered.json["attempted"].as_array().unwrap().len(), 0);
        let skipped = rendered.json["skipped"].as_array().unwrap();
        assert_eq!(skipped.len(), 1);
        assert!(skipped[0]["reason"].as_str().unwrap().contains("KEEL-E030"));
    }
}
