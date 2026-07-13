//! `keel fsck` — journal integrity check, safe repairs, and retention pruning
//! (architecture spec §6: "journal corruption → SQLite WAL recovery, plus
//! `keel fsck`").
//!
//! The check pass is read-only and safe against a live application. It runs,
//! in order:
//!
//! 1. `PRAGMA integrity_check` — page-level SQLite corruption. Unrepairable
//!    here (WAL recovery already ran at open), so a failure short-circuits the
//!    structural checks and points at restore-or-recreate.
//! 2. The frozen-schema presence check — is this file a keel journal at all?
//! 3. The flow-ledger invariants ([`keel_journal::admin`] documents each and
//!    why its repair is safe): **orphan steps** (dangerous: deterministic flow
//!    ids mean a rerun would replay a stranger's steps), **dangling leases**
//!    (lease fields on a non-`running` flow), **stale running steps** (a
//!    `running` step inside a `completed`/`dead` flow), and the **expired
//!    cache** backlog. **Dead flows** are reported for visibility, never
//!    repaired — they are the evidence `keel flows --dead` inspects.
//!
//! `--fix` applies exactly those safe repairs (plus a WAL checkpoint to
//! reclaim space); `checks` in the report always describes the *pre-fix*
//! state and `repairs` what was done, so one run tells the whole story.
//!
//! ## Retention: `--prune <AGE>`
//!
//! There is no retention key in the frozen policy schema, so retention is an
//! explicit operator action: `keel fsck --prune 30d` deletes `completed`
//! flows (and their steps) not updated for the given age — never `running`
//! (resumable), `failed` (resumable), or `dead` (evidence), and never a flow
//! with outbox rows. **Caveat printed with every prune**: a pruned flow
//! cannot replay; rerunning the same entrypoint+args starts a fresh flow that
//! executes live.
//!
//! Determinism (dx-spec §5): the `--json` twin is a pure function of the
//! journal bytes and the injected `now` — no wall-clock reads here.

use std::path::Path;

use keel_journal::admin::JournalAdmin;
use serde::Serialize;

use crate::render::to_json;
use crate::{EXIT_FAILURE, EXIT_USAGE, Rendered, evidence};

/// The `keel fsck` switches: apply safe repairs, and/or prune completed flows
/// older than an age (`"90s"`, `"45m"`, `"12h"`, `"30d"`).
#[derive(Debug, Clone, Default)]
pub struct FsckOptions {
    /// Apply the safe repairs (and a WAL checkpoint).
    pub fix: bool,
    /// Prune completed flows older than this age, e.g. `"30d"`.
    pub prune: Option<String>,
}

/// `PRAGMA integrity_check`, folded for the report.
#[derive(Debug, Serialize)]
struct IntegrityReport {
    detail: Vec<String>,
    ok: bool,
}

/// A finding that names flows: how many, and which.
#[derive(Debug, Serialize)]
struct FlowFinding {
    count: u64,
    flow_ids: Vec<String>,
}

/// One stale `running` step inside a terminal flow.
#[derive(Debug, Serialize)]
struct StaleStep {
    flow_id: String,
    seq: i64,
}

/// The structural (ledger) checks — always the *pre-fix* state.
#[derive(Debug, Serialize)]
struct ChecksReport {
    dangling_leases: FlowFinding,
    dead_flows: FlowFinding,
    expired_cache_rows: u64,
    orphan_steps: OrphanReport,
    stale_running_steps: Vec<StaleStep>,
}

/// Orphan steps: row count plus the (sorted) missing flow ids they point at.
#[derive(Debug, Serialize)]
struct OrphanReport {
    missing_flow_ids: Vec<String>,
    rows: u64,
}

/// What `--fix` did.
#[derive(Debug, Serialize)]
struct RepairsReport {
    dangling_leases_cleared: u64,
    expired_cache_swept: u64,
    orphan_steps_deleted: u64,
    stale_running_steps_deleted: u64,
    wal_checkpointed: bool,
}

/// What `--prune <AGE>` did.
#[derive(Debug, Serialize)]
struct PruneReport {
    age: String,
    flow_ids: Vec<String>,
    flows_deleted: u64,
    steps_deleted: u64,
}

/// The whole `keel fsck` result — one struct so the human view and the
/// `--json` twin cannot drift.
#[derive(Debug, Serialize)]
struct FsckReport {
    checks: Option<ChecksReport>,
    fix: bool,
    integrity: Option<IntegrityReport>,
    journal: String,
    journal_present: bool,
    ok: bool,
    prune: Option<PruneReport>,
    repairs: Option<RepairsReport>,
    schema_present: Option<bool>,
}

/// `keel fsck [--fix] [--prune <AGE>]` for `project`, judging cache expiry and
/// prune cutoffs against the injected `now_ms`.
pub fn run(project: &Path, options: &FsckOptions, now_ms: i64) -> Rendered {
    // Parse the age first: a bad flag is a usage error before any file I/O.
    let prune_age = match &options.prune {
        None => None,
        Some(raw) => match parse_age_ms(raw) {
            Ok(ms) => Some((raw.clone(), ms)),
            Err(why) => return usage_error(raw, &why),
        },
    };

    let resolved = evidence::resolved_journal(project);
    if !resolved.path.exists() {
        let report = FsckReport {
            checks: None,
            fix: options.fix,
            integrity: None,
            journal: resolved.display,
            journal_present: false,
            ok: true,
            prune: None,
            repairs: None,
            schema_present: None,
        };
        return Rendered::ok(
            format!(
                "keel \u{25b8} fsck {}: no journal yet \u{2014} nothing to check.",
                report.journal
            ),
            to_json(&report),
        );
    }

    let writes = options.fix || prune_age.is_some();
    let admin = match open(&resolved.path, writes) {
        Ok(a) => a,
        Err(e) => return soft_error(&e),
    };

    build_report(&admin, resolved.display, options.fix, prune_age, now_ms)
}

/// The check/fix/prune pass over an opened journal, rendered.
fn build_report(
    admin: &JournalAdmin,
    journal: String,
    fix: bool,
    prune_age: Option<(String, i64)>,
    now_ms: i64,
) -> Rendered {
    let integrity = admin.integrity_check();
    if !integrity.ok {
        let report = FsckReport {
            checks: None,
            fix,
            integrity: Some(IntegrityReport {
                detail: integrity.detail,
                ok: false,
            }),
            journal,
            journal_present: true,
            ok: false,
            prune: None,
            repairs: None,
            schema_present: None,
        };
        let human = format!(
            "keel \u{25b8} fsck {}: FAILED SQLite integrity_check.\n  why:  the file is corrupt \
             beyond what WAL recovery repairs (detail in --json).\n  next: restore the journal \
             from a backup, or delete it \u{2014} flows lose replay, Tier 1 is unaffected.",
            report.journal
        );
        return Rendered::ok(human, to_json(&report)).with_exit(EXIT_FAILURE);
    }

    let schema_present = admin.schema_present().unwrap_or(false);
    if !schema_present {
        let report = FsckReport {
            checks: None,
            fix,
            integrity: Some(IntegrityReport {
                detail: Vec::new(),
                ok: true,
            }),
            journal,
            journal_present: true,
            ok: false,
            prune: None,
            repairs: None,
            schema_present: Some(false),
        };
        let human = format!(
            "keel \u{25b8} fsck {}: not a keel journal.\n  why:  the file is valid SQLite but \
             lacks the frozen journal schema (flows/steps/cache).\n  next: check the `journal` \
             key in keel.toml \u{2014} it points at some other database.",
            report.journal
        );
        return Rendered::ok(human, to_json(&report)).with_exit(EXIT_FAILURE);
    }

    // The structural checks (pre-fix state).
    let checks = match read_checks(admin, now_ms) {
        Ok(c) => c,
        Err(e) => return soft_error(&e),
    };

    let repairs = if fix {
        match apply_repairs(admin, now_ms) {
            Ok(r) => Some(r),
            Err(e) => return soft_error(&e),
        }
    } else {
        None
    };

    let prune = match prune_age {
        None => None,
        Some((age, age_ms)) => {
            let cutoff = now_ms.saturating_sub(age_ms);
            match apply_prune(admin, age, cutoff) {
                Ok(p) => Some(p),
                Err(e) => return soft_error(&e),
            }
        }
    };

    // Repairables count against `ok` only while unrepaired; expired cache and
    // dead flows are routine, not integrity findings.
    let findings = checks.orphan_steps.rows
        + checks.dangling_leases.count
        + checks.stale_running_steps.len() as u64;
    let ok = fix || findings == 0;

    let report = FsckReport {
        checks: Some(checks),
        fix,
        integrity: Some(IntegrityReport {
            detail: Vec::new(),
            ok: true,
        }),
        journal,
        journal_present: true,
        ok,
        prune,
        repairs,
        schema_present: Some(true),
    };
    let human = human(&report);
    let exit = if ok { crate::EXIT_OK } else { EXIT_FAILURE };
    Rendered::ok(human, to_json(&report)).with_exit(exit)
}

fn open(path: &Path, writes: bool) -> Result<JournalAdmin, String> {
    let open = if writes {
        JournalAdmin::open_readwrite(path)
    } else {
        JournalAdmin::open_readonly(path)
    };
    open.map_err(|e| format!("could not open {}: {e}", path.display()))
}

fn read_checks(admin: &JournalAdmin, now_ms: i64) -> Result<ChecksReport, String> {
    let q = |e: keel_journal::Error| format!("journal query failed: {e}");
    let dead = admin.dead_flow_ids().map_err(q)?;
    let leases = admin.dangling_lease_flow_ids().map_err(q)?;
    Ok(ChecksReport {
        dangling_leases: FlowFinding {
            count: leases.len() as u64,
            flow_ids: leases,
        },
        dead_flows: FlowFinding {
            count: dead.len() as u64,
            flow_ids: dead,
        },
        expired_cache_rows: admin.expired_cache_count(now_ms).map_err(q)?,
        orphan_steps: OrphanReport {
            missing_flow_ids: admin.orphan_step_flow_ids().map_err(q)?,
            rows: admin.orphan_step_count().map_err(q)?,
        },
        stale_running_steps: admin
            .stale_running_steps()
            .map_err(q)?
            .into_iter()
            .map(|s| StaleStep {
                flow_id: s.flow_id,
                seq: s.seq,
            })
            .collect(),
    })
}

fn apply_repairs(admin: &JournalAdmin, now_ms: i64) -> Result<RepairsReport, String> {
    let q = |e: keel_journal::Error| format!("journal repair failed: {e}");
    let orphan_steps_deleted = admin.delete_orphan_steps().map_err(q)?;
    let dangling_leases_cleared = admin.clear_dangling_leases().map_err(q)?;
    let stale_running_steps_deleted = admin.sweep_stale_running_steps().map_err(q)?;
    let expired_cache_swept = admin.sweep_expired_cache(now_ms).map_err(q)?;
    admin.wal_checkpoint().map_err(q)?;
    Ok(RepairsReport {
        dangling_leases_cleared,
        expired_cache_swept,
        orphan_steps_deleted,
        stale_running_steps_deleted,
        wal_checkpointed: true,
    })
}

fn apply_prune(admin: &JournalAdmin, age: String, cutoff_ms: i64) -> Result<PruneReport, String> {
    let q = |e: keel_journal::Error| format!("journal prune failed: {e}");
    let flow_ids = admin.prunable_completed_flow_ids(cutoff_ms).map_err(q)?;
    let outcome = admin.prune_completed_flows(cutoff_ms).map_err(q)?;
    Ok(PruneReport {
        age,
        flow_ids,
        flows_deleted: outcome.flows_deleted,
        steps_deleted: outcome.steps_deleted,
    })
}

/// The human report, derived entirely from [`FsckReport`].
fn human(report: &FsckReport) -> String {
    let checks = report.checks.as_ref().expect("human() runs on full reports");
    let verdict = if report.ok { "clean" } else { "FINDINGS" };
    let mut lines = vec![format!(
        "keel \u{25b8} fsck {} \u{2014} {verdict}\n",
        report.journal
    )];
    lines.push("  integrity:           ok\n".to_owned());
    lines.push(format!(
        "  orphan steps:        {} row(s){}\n",
        checks.orphan_steps.rows,
        if checks.orphan_steps.missing_flow_ids.is_empty() {
            String::new()
        } else {
            format!(
                " pointing at missing flow(s) {}",
                checks.orphan_steps.missing_flow_ids.join(", ")
            )
        }
    ));
    lines.push(format!(
        "  dangling leases:     {}\n",
        checks.dangling_leases.count
    ));
    lines.push(format!(
        "  stale running steps: {}\n",
        checks.stale_running_steps.len()
    ));
    lines.push(format!(
        "  expired cache rows:  {}\n",
        checks.expired_cache_rows
    ));
    lines.push(format!(
        "  dead flows:          {}{}\n",
        checks.dead_flows.count,
        if checks.dead_flows.count == 0 {
            ""
        } else {
            " (inspect with `keel flows --dead`)"
        }
    ));
    if let Some(r) = &report.repairs {
        lines.push(format!(
            "  repaired: {} orphan step(s), {} lease(s), {} stale step(s); swept {} expired \
             cache row(s); WAL checkpointed.\n",
            r.orphan_steps_deleted,
            r.dangling_leases_cleared,
            r.stale_running_steps_deleted,
            r.expired_cache_swept
        ));
    }
    if let Some(p) = &report.prune {
        lines.push(format!(
            "  pruned: {} completed flow(s) older than {} ({} step row(s)).\n  note: a pruned \
             flow cannot replay \u{2014} rerunning the same entrypoint+args starts fresh.\n",
            p.flows_deleted, p.age, p.steps_deleted
        ));
    }
    if !report.ok {
        lines.push(
            "  next: run `keel fsck --fix` to repair the findings above (safe: repairs never \
             touch resumable flows).\n"
                .to_owned(),
        );
    }
    lines.concat()
}

/// Parse `"90s"`, `"45m"`, `"12h"`, `"30d"` into milliseconds.
fn parse_age_ms(raw: &str) -> Result<i64, String> {
    let (digits, unit) = raw.split_at(raw.len().saturating_sub(1));
    let per_unit: i64 = match unit {
        "s" => 1_000,
        "m" => 60_000,
        "h" => 3_600_000,
        "d" => 86_400_000,
        _ => return Err("age needs a unit suffix: s, m, h, or d".to_owned()),
    };
    let n: i64 = digits
        .parse()
        .map_err(|_| "age needs a whole number before the unit".to_owned())?;
    if n < 0 {
        return Err("age cannot be negative".to_owned());
    }
    n.checked_mul(per_unit)
        .ok_or_else(|| "age is out of range".to_owned())
}

fn usage_error(raw: &str, why: &str) -> Rendered {
    #[derive(Serialize)]
    struct UsageReport<'a> {
        error: &'static str,
        next: &'static str,
        what: String,
        why: &'a str,
    }
    let what = format!("Cannot parse --prune {raw:?} as an age.");
    let next = "Use <number><unit> with unit s/m/h/d, e.g. `keel fsck --prune 30d`.";
    let human = format!("keel \u{25b8} {what}\n  why:  {why}\n  next: {next}");
    Rendered {
        human,
        json: to_json(&UsageReport {
            error: "bad-age",
            next,
            what,
            why,
        }),
        exit: EXIT_USAGE,
        to_stderr: true,
    }
}

/// A read/repair failure (exit 1, on stderr) — mirrors `keel flows`.
fn soft_error(message: &str) -> Rendered {
    #[derive(Serialize)]
    struct ErrReport<'a> {
        error: &'a str,
    }
    Rendered {
        human: format!("keel \u{25b8} {message}"),
        json: to_json(&ErrReport { error: message }),
        exit: EXIT_FAILURE,
        to_stderr: true,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use rusqlite::{Connection, params};
    use std::path::PathBuf;

    const T0: i64 = 1_783_728_000_000;

    /// A project dir whose `.keel/journal.db` is built from the frozen schema
    /// plus the named golden fixtures (`conformance/fixtures/journal/`).
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
                std::fs::read_to_string(root.join("conformance/fixtures/journal").join(f))
                    .unwrap();
            conn.execute_batch(&sql).unwrap();
        }
        let project = dir.path().to_path_buf();
        (dir, project)
    }

    fn damage(project: &Path) {
        let conn = Connection::open(project.join(".keel/journal.db")).unwrap();
        conn.execute_batch("PRAGMA foreign_keys = OFF;").unwrap();
        // Orphan step, dangling lease on the dead flow, stale running step in
        // the completed flow, and an expired + a live cache row.
        conn.execute(
            "INSERT INTO steps VALUES ('09GHOST', 1, 'x#1', 'effect', 1, 'ok', NULL, NULL, ?1, ?1)",
            params![T0],
        )
        .unwrap();
        conn.execute(
            "UPDATE flows SET lease_holder = 'host-z:pid-9', lease_expires = ?1 \
             WHERE flow_id = '01JZWY0A0000000000000003'",
            params![T0],
        )
        .unwrap();
        conn.execute(
            "INSERT INTO steps VALUES ('01JZWY0A0000000000000001', 6, 'y#1', 'effect', 1, \
             'running', NULL, NULL, ?1, NULL)",
            params![T0],
        )
        .unwrap();
        conn.execute(
            "INSERT INTO cache (key, value, expires_at) VALUES ('old', X'C0', ?1)",
            params![T0 - 1],
        )
        .unwrap();
        conn.execute(
            "INSERT INTO cache (key, value, expires_at) VALUES ('new', X'C0', ?1)",
            params![T0 + 3_600_000],
        )
        .unwrap();
    }

    #[test]
    fn clean_fixtures_pass() {
        let (_d, project) =
            project_with_fixtures(&["completed-flow.sql", "interrupted-flow.sql", "dead-flow.sql"]);
        let r = run(&project, &FsckOptions::default(), T0);
        assert_eq!(r.exit, crate::EXIT_OK);
        assert_eq!(r.json["ok"], true);
        assert_eq!(r.json["integrity"]["ok"], true);
        assert_eq!(r.json["checks"]["orphan_steps"]["rows"], 0);
        assert_eq!(r.json["checks"]["dangling_leases"]["count"], 0);
        // The interrupted flow's `running` step is crash evidence, not stale.
        assert_eq!(
            r.json["checks"]["stale_running_steps"].as_array().unwrap().len(),
            0
        );
        // The dead flow is visible but does not fail fsck.
        assert_eq!(r.json["checks"]["dead_flows"]["count"], 1);
        assert!(r.human.contains("clean"));
    }

    #[test]
    fn findings_fail_the_check_and_fix_repairs_them() {
        let (_d, project) =
            project_with_fixtures(&["completed-flow.sql", "interrupted-flow.sql", "dead-flow.sql"]);
        damage(&project);

        let r = run(&project, &FsckOptions::default(), T0);
        assert_eq!(r.exit, crate::EXIT_FAILURE);
        assert_eq!(r.json["ok"], false);
        assert_eq!(r.json["checks"]["orphan_steps"]["rows"], 1);
        assert_eq!(r.json["checks"]["orphan_steps"]["missing_flow_ids"][0], "09GHOST");
        assert_eq!(r.json["checks"]["dangling_leases"]["count"], 1);
        assert_eq!(r.json["checks"]["stale_running_steps"][0]["seq"], 6);
        assert_eq!(r.json["checks"]["expired_cache_rows"], 1);
        assert!(r.human.contains("next: run `keel fsck --fix`"));

        let fixed = run(
            &project,
            &FsckOptions {
                fix: true,
                prune: None,
            },
            T0,
        );
        assert_eq!(fixed.exit, crate::EXIT_OK);
        assert_eq!(fixed.json["ok"], true);
        assert_eq!(fixed.json["repairs"]["orphan_steps_deleted"], 1);
        assert_eq!(fixed.json["repairs"]["dangling_leases_cleared"], 1);
        assert_eq!(fixed.json["repairs"]["stale_running_steps_deleted"], 1);
        assert_eq!(fixed.json["repairs"]["expired_cache_swept"], 1);

        // A re-check is clean.
        let again = run(&project, &FsckOptions::default(), T0);
        assert_eq!(again.exit, crate::EXIT_OK);
        assert_eq!(again.json["checks"]["orphan_steps"]["rows"], 0);
        assert_eq!(again.json["checks"]["expired_cache_rows"], 0);
    }

    #[test]
    fn prune_removes_old_completed_flows_only() {
        let (_d, project) =
            project_with_fixtures(&["completed-flow.sql", "interrupted-flow.sql", "dead-flow.sql"]);
        // 40 days after T0, prune anything older than 30d: only the completed
        // flow qualifies (running is resumable, dead is evidence).
        let now = T0 + 40 * 86_400_000;
        let r = run(
            &project,
            &FsckOptions {
                fix: false,
                prune: Some("30d".to_owned()),
            },
            now,
        );
        assert_eq!(r.exit, crate::EXIT_OK);
        assert_eq!(r.json["prune"]["flows_deleted"], 1);
        assert_eq!(r.json["prune"]["flow_ids"][0], "01JZWY0A0000000000000001");
        assert_eq!(r.json["prune"]["steps_deleted"], 5);
        assert!(r.human.contains("cannot replay"));

        // The running and dead flows survive.
        let conn = Connection::open(project.join(".keel/journal.db")).unwrap();
        let left: i64 = conn
            .query_row("SELECT COUNT(*) FROM flows", [], |row| row.get(0))
            .unwrap();
        assert_eq!(left, 2);
    }

    #[test]
    fn prune_age_too_young_removes_nothing() {
        let (_d, project) = project_with_fixtures(&["completed-flow.sql"]);
        let r = run(
            &project,
            &FsckOptions {
                fix: false,
                prune: Some("30d".to_owned()),
            },
            T0 + 86_400_000, // one day later: the flow is younger than 30d
        );
        assert_eq!(r.json["prune"]["flows_deleted"], 0);
        assert_eq!(r.json["prune"]["flow_ids"].as_array().unwrap().len(), 0);
    }

    #[test]
    fn bad_age_is_a_usage_error() {
        let dir = tempfile::TempDir::new().unwrap();
        for bad in ["30", "d", "", "-3d", "30w", "3.5h"] {
            let r = run(
                dir.path(),
                &FsckOptions {
                    fix: false,
                    prune: Some(bad.to_owned()),
                },
                T0,
            );
            assert_eq!(r.exit, EXIT_USAGE, "{bad:?} must be rejected");
            assert!(r.to_stderr);
            assert_eq!(r.json["error"], "bad-age");
            assert!(r.human.contains("next:"));
        }
    }

    #[test]
    fn age_parses_all_units() {
        assert_eq!(parse_age_ms("90s").unwrap(), 90_000);
        assert_eq!(parse_age_ms("45m").unwrap(), 2_700_000);
        assert_eq!(parse_age_ms("12h").unwrap(), 43_200_000);
        assert_eq!(parse_age_ms("30d").unwrap(), 2_592_000_000);
        assert_eq!(parse_age_ms("0d").unwrap(), 0);
        assert!(parse_age_ms("99999999999999999999d").is_err());
    }

    #[test]
    fn missing_journal_is_a_clean_no_op() {
        let dir = tempfile::TempDir::new().unwrap();
        let r = run(dir.path(), &FsckOptions::default(), T0);
        assert_eq!(r.exit, crate::EXIT_OK);
        assert_eq!(r.json["journal_present"], false);
        assert_eq!(r.json["ok"], true);
        assert!(r.human.contains("nothing to check"));
    }

    #[test]
    fn garbage_file_fails_integrity_with_what_why_next() {
        let dir = tempfile::TempDir::new().unwrap();
        let keel = dir.path().join(".keel");
        std::fs::create_dir_all(&keel).unwrap();
        std::fs::write(keel.join("journal.db"), b"not a database").unwrap();
        let r = run(dir.path(), &FsckOptions::default(), T0);
        assert_eq!(r.exit, EXIT_FAILURE);
        assert_eq!(r.json["ok"], false);
        assert_eq!(r.json["integrity"]["ok"], false);
        assert!(r.human.contains("why:"));
        assert!(r.human.contains("next:"));
    }

    #[test]
    fn foreign_sqlite_file_is_not_a_keel_journal() {
        let dir = tempfile::TempDir::new().unwrap();
        let keel = dir.path().join(".keel");
        std::fs::create_dir_all(&keel).unwrap();
        let conn = Connection::open(keel.join("journal.db")).unwrap();
        conn.execute_batch("CREATE TABLE t (x INTEGER);").unwrap();
        drop(conn);
        let r = run(dir.path(), &FsckOptions::default(), T0);
        assert_eq!(r.exit, EXIT_FAILURE);
        assert_eq!(r.json["schema_present"], false);
        assert!(r.human.contains("not a keel journal"));
    }

    #[test]
    fn check_pass_never_writes() {
        // fsck without --fix opens read-only: run it against fixtures on a
        // read-only directory... approximated portably by asserting the same
        // findings twice (no repair happened in between).
        let (_d, project) = project_with_fixtures(&["completed-flow.sql"]);
        damage(&project);
        let first = run(&project, &FsckOptions::default(), T0);
        let second = run(&project, &FsckOptions::default(), T0);
        assert_eq!(first.json, second.json, "a check pass must not mutate");
    }
}
