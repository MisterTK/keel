//! `keel` — the binary. A thin clap front over [`keel_cli`]: parse, dispatch,
//! print the right half of the [`Rendered`](keel_cli::Rendered) result, exit
//! with its code. All behavior lives in the library so it is unit-testable
//! without spawning a process.

use std::path::PathBuf;
use std::process::exit;

use clap::{Parser, Subcommand};

use keel_cli::render::emit;
use keel_cli::{doctor, effective, explain, flows, fsck, init, replay, resume, run, status};
use keel_journal::{Clock, SystemClock};

/// Production-grade resilience for anything, with zero code changes.
#[derive(Debug, Parser)]
#[command(name = "keel", version, about, long_about = None)]
struct Cli {
    /// Emit byte-deterministic JSON instead of prose (sorted keys, no
    /// wall-clock timestamps). Humans get prose; machines get structure.
    #[arg(long, global = true)]
    json: bool,

    #[command(subcommand)]
    command: Command,
}

#[derive(Debug, Subcommand)]
enum Command {
    /// Run a script under Keel, dispatching to its language front end.
    Run {
        /// Disable Keel for this run (sets `KEEL_DISABLE=1`); the program runs
        /// byte-identically to having no Keel installed.
        #[arg(long)]
        disable: bool,
        /// The script (`.py`, `.mjs`/`.js`/`.ts`…), a `package.json`, or a
        /// project directory to run.
        target: String,
        /// Arguments passed through to the program unchanged.
        #[arg(trailing_var_arg = true, allow_hyphen_values = true)]
        args: Vec<String>,
    },
    /// Generate `keel.toml` from static + observed evidence.
    Init {
        /// Preview changes against an existing `keel.toml` without writing.
        #[arg(long)]
        diff: bool,
        /// Stamp today's date into the header (off by default for determinism).
        #[arg(long)]
        stamp: bool,
        /// Drop the Keel section into AGENTS.md for coding agents (dx-spec §5),
        /// instead of generating a policy.
        #[arg(long)]
        agents: bool,
    },
    /// Report coverage, adapters, and policy validity (the honesty report).
    Doctor {
        /// Print the composed effective policy (defaults < packs < user) that
        /// `keel_configure` receives, instead of the coverage report.
        #[arg(long)]
        effective_policy: bool,
    },
    /// Show one screen of coverage and flow state.
    Status,
    /// List durable (Tier 2) flows: id, entrypoint, status, steps, age. With no
    /// `resume` subcommand this is the only mode; `keel flows resume` acts
    /// instead of listing.
    Flows {
        /// Show only `dead` flows (those that exhausted their resume cap).
        #[arg(long)]
        dead: bool,
        #[command(subcommand)]
        action: Option<FlowsAction>,
    },
    /// Journal integrity check, safe repairs, and retention pruning
    /// (architecture spec §6).
    Fsck {
        /// Apply the safe repairs (orphan steps, dangling leases, stale
        /// running steps, expired cache) and checkpoint the WAL.
        #[arg(long)]
        fix: bool,
        /// Prune `completed` flows (and their steps) not updated for this age,
        /// e.g. `30d`, `12h`, `45m`, `90s`. There is no retention key in the
        /// frozen policy schema, so this is an explicit operator action.
        #[arg(long, value_name = "AGE")]
        prune: Option<String>,
    },
    /// Inspect what re-entering a flow would do — a journal-driven dry run:
    /// which steps substitute, which re-execute, where replay resumes.
    Replay {
        /// A flow_id, or a substring of an id/entrypoint that names one flow.
        flow: String,
        /// Show one recorded step in full detail (payload, timings, action).
        #[arg(long, value_name = "SEQ")]
        step: Option<i64>,
    },
    /// Trace one flow's steps step-by-step (outcomes, attempts, timings).
    Trace {
        /// A flow_id, or a substring of an id/entrypoint that names one flow.
        flow: String,
    },
    /// Explain a `KEEL-E0NN` error code (what / why / next).
    Explain {
        /// The error code, e.g. `KEEL-E014`.
        code: String,
    },
}

/// `keel flows resume` — the one action nested under `flows` (dx-spec §1
/// Level 2). See [`keel_cli::resume`] for what it can and cannot know.
#[derive(Debug, Subcommand)]
enum FlowsAction {
    /// Re-invoke a resumable flow's recorded entrypoint through `keel run`.
    Resume {
        /// A flow_id, or a substring of an id/entrypoint that names one flow.
        /// Omit and pass `--all` to resume every resumable flow instead.
        flow: Option<String>,
        /// Resume every currently-resumable flow (no live lease) instead of
        /// naming one.
        #[arg(long)]
        all: bool,
        /// Arguments forwarded to the resumed script (single-flow only —
        /// `--all` cannot forward args since different flows may need
        /// different ones).
        #[arg(trailing_var_arg = true, allow_hyphen_values = true)]
        args: Vec<String>,
    },
}

fn main() {
    let cli = Cli::parse();
    let json = cli.json;
    let project = PathBuf::from(".");

    let code = match cli.command {
        Command::Run {
            disable,
            target,
            args,
        } => {
            let (rendered, code) = run::run(&target, &args, disable);
            if let Some(r) = rendered {
                emit(&r, json);
            }
            code
        }
        Command::Init {
            diff,
            stamp,
            agents,
        } => {
            let r = init::run(
                &project,
                init::InitOptions {
                    diff,
                    stamp,
                    agents,
                },
            );
            emit(&r, json)
        }
        Command::Doctor { effective_policy } => {
            let r = if effective_policy {
                effective::run(&project)
            } else {
                doctor::run(&project)
            };
            emit(&r, json)
        }
        Command::Status => emit(&status::run(&project), json),
        Command::Flows { dead, action } => match action {
            None => emit(&flows::flows(&project, dead, SystemClock.now_ms()), json),
            Some(FlowsAction::Resume { flow, all, args }) => {
                let options = resume::ResumeOptions { flow, all, args };
                let (rendered, code) = resume::run(&project, &options, SystemClock.now_ms());
                if let Some(r) = rendered {
                    emit(&r, json);
                }
                code
            }
        },
        Command::Fsck { fix, prune } => {
            let options = fsck::FsckOptions { fix, prune };
            emit(&fsck::run(&project, &options, SystemClock.now_ms()), json)
        }
        Command::Replay { flow, step } => emit(&replay::replay(&project, &flow, step), json),
        Command::Trace { flow } => emit(&flows::trace(&project, &flow), json),
        Command::Explain { code } => emit(&explain::run(&code), json),
    };
    exit(code);
}
