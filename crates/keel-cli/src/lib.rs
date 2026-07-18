//! `keel` — the command-line face of the product (dx-spec §1–2, §5–6).
//!
//! Subcommands: [`run`] (dispatch a script into a language front end),
//! [`init`] (evidence-merged policy generation), [`doctor`] (the honesty
//! report; `--effective-policy` prints the composed `defaults < packs < user`
//! policy via [`effective`]), [`status`] (the "what is Keel doing for me" screen),
//! [`explain`] (the frozen error taxonomy), [`tail`] (the live Tier 1 event
//! view), [`fsck`] (journal integrity check/repair and retention pruning),
//! the Tier 2 flow inspectors [`flows`] (list durable flows), [`flows::trace`]
//! (`keel trace`), and [`replay`] (`keel replay` — a journal-driven dry run of
//! what a re-entry would substitute vs. re-execute), [`mcp`] (`keel mcp`: the
//! CLI doubles as an MCP server over stdio whose six tools return the same
//! bytes as the corresponding `--json` twins), and the Level 2 on-ramp
//! [`flows_suggest`] (`keel flows suggest`, a replay-safety analysis over
//! candidate entrypoints), [`flows_add`] (`keel flows add <entrypoint>`,
//! one-command durability designation), and [`resume`] (`keel flows resume` —
//! actually re-invoke a resumable flow's recorded entrypoint through
//! `keel run`).
//!
//! Every command obeys the DX invariants: a `--json` twin with byte-deterministic
//! output (sorted keys, no wall-clock timestamps), and stable exit codes —
//! [`EXIT_OK`], [`EXIT_FAILURE`], [`EXIT_USAGE`]. The command modules are the
//! testable core; [`main`](../keel/index.html) is a thin clap front.

pub mod agents_cli;
pub mod diff;
pub mod doctor;
pub mod effective;
pub mod exec;
pub mod explain;
pub mod flows;
pub mod flows_add;
pub mod flows_suggest;
pub mod fsck;
pub mod init;
pub mod mcp;
pub mod record;
pub mod render;
pub mod replay;
pub mod resume;
pub mod run;
pub mod scan;
pub mod sim;
pub mod status;
pub mod tail;

mod evidence;

/// Success. The command did what was asked.
pub const EXIT_OK: i32 = 0;
/// The underlying program or verb failed (a run's child exited non-zero, an
/// error surfaced by the report). Distinct from a *usage* problem.
pub const EXIT_FAILURE: i32 = 1;
/// A usage or policy error: bad arguments, an unknown error code, an invalid
/// `keel.toml`. The caller must fix the request or the policy.
pub const EXIT_USAGE: i32 = 2;

/// A fully rendered command result: the two audiences (`human` prose and the
/// `json` twin) plus the exit code the process should carry. Commands build one
/// of these; [`emit`](render::emit) prints the right half and the caller exits.
#[derive(Debug, Clone)]
pub struct Rendered {
    /// Human-facing prose (stdout on success).
    pub human: String,
    /// The `--json` twin — byte-deterministic, sorted keys.
    pub json: serde_json::Value,
    /// The exit code this result carries.
    pub exit: i32,
    /// When true, `human`/`json` are diagnostics and belong on stderr.
    pub to_stderr: bool,
}

impl Rendered {
    /// A success result destined for stdout.
    pub fn ok(human: impl Into<String>, json: serde_json::Value) -> Self {
        Self {
            human: human.into(),
            json,
            exit: EXIT_OK,
            to_stderr: false,
        }
    }

    /// Carry a non-success exit code on an otherwise-rendered result.
    #[must_use]
    pub fn with_exit(mut self, exit: i32) -> Self {
        self.exit = exit;
        self
    }

    /// Route this result to stderr (diagnostics, error reports).
    #[must_use]
    pub fn on_stderr(mut self) -> Self {
        self.to_stderr = true;
        self
    }
}
