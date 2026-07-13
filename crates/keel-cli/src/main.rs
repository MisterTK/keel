//! `keel` — the binary. A thin clap front over [`keel_cli`]: parse, dispatch,
//! print the right half of the [`Rendered`](keel_cli::Rendered) result, exit
//! with its code. All behavior lives in the library so it is unit-testable
//! without spawning a process.

use std::path::PathBuf;
use std::process::exit;

use clap::{Parser, Subcommand};

use keel_cli::render::emit;
use keel_cli::{
    doctor, effective, explain, flows, flows_add, flows_suggest, fsck, init, mcp, replay, resume,
    run, status, tail,
};
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
    /// List durable (Tier 2) flows: id, entrypoint, status, steps, age. Or, with
    /// a subcommand, the Level 2 on-ramp (`suggest` candidates, `add` one) or
    /// `resume` (re-invoke a resumable flow's recorded entrypoint).
    Flows {
        /// Show only `dead` flows (those that exhausted their resume cap).
        /// Ignored when a subcommand is given.
        #[arg(long)]
        dead: bool,
        #[command(subcommand)]
        action: Option<FlowsCommand>,
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
    /// Serve this project over MCP on stdio (JSON-RPC 2.0). Six tools —
    /// get_status, get_doctor_report, propose_policy, get_trace, list_flows,
    /// explain_error — each byte-identical to the matching `--json` command.
    Mcp,
    /// Inspect what re-entering a flow would do — a journal-driven dry run:
    /// which steps substitute, which re-execute, where replay resumes.
    Replay {
        /// A flow_id, or a substring of an id/entrypoint that names one flow.
        flow: String,
        /// Show one recorded step in full detail (payload, timings, action).
        #[arg(long, value_name = "SEQ")]
        step: Option<i64>,
    },
    /// Live view of attempts, backoffs, and breaker transitions while your
    /// program runs (reads `.keel/events/`; no daemon). `--json` streams the
    /// raw NDJSON events with sorted keys.
    Tail {
        /// Print the recorded events and exit instead of following live.
        #[arg(long)]
        no_follow: bool,
        /// Tail a specific run id instead of the newest run.
        #[arg(long)]
        run: Option<String>,
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

/// `keel flows <action>` — the Level 2 on-ramp (dx-spec §1).
#[derive(Debug, Subcommand)]
enum FlowsCommand {
    /// Designate `<entrypoint>` as a durable flow: appends it to `[flows]
    /// entrypoints` in `keel.toml` (creating the table if absent). Idempotent —
    /// re-running with the same entrypoint is a no-op.
    Add {
        /// `py:module.path:function`, `ts:path/file.ts#function`, or a bare
        /// form printed by `keel flows suggest` (its language is inferred).
        entrypoint: String,
        /// Preview the change as a diff without writing `keel.toml`.
        #[arg(long)]
        diff: bool,
    },
    /// Analyze candidate flow entrypoints for replay-safety: effect counts,
    /// idempotent-unsafe effects, time/random reads Tier 2 would virtualize,
    /// and an estimated replay-safe verdict.
    Suggest,
    /// Re-invoke a resumable flow's recorded entrypoint through `keel run`.
    /// See [`keel_cli::resume`] for what it can and cannot know.
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
        Command::Status => emit(&status::run(&project, SystemClock.now_ms()), json),
        Command::Flows { dead, action } => match action {
            Some(FlowsCommand::Add { entrypoint, diff }) => {
                emit(&flows_add::run(&project, &entrypoint, diff), json)
            }
            Some(FlowsCommand::Suggest) => emit(&flows_suggest::run(&project), json),
            Some(FlowsCommand::Resume { flow, all, args }) => {
                let options = resume::ResumeOptions { flow, all, args };
                let (rendered, code) = resume::run(&project, &options, SystemClock.now_ms());
                if let Some(r) = rendered {
                    emit(&r, json);
                }
                code
            }
            None => emit(&flows::flows(&project, dead, SystemClock.now_ms()), json),
        },
        Command::Fsck { fix, prune } => {
            let options = fsck::FsckOptions { fix, prune };
            emit(&fsck::run(&project, &options, SystemClock.now_ms()), json)
        }
        Command::Mcp => {
            // The server speaks JSON-RPC regardless of --json; it exits on EOF.
            let stdin = std::io::stdin();
            let stdout = std::io::stdout();
            mcp::Server::new(project, || SystemClock.now_ms()).serve(stdin.lock(), stdout.lock())
        }
        Command::Replay { flow, step } => emit(&replay::replay(&project, &flow, step), json),
        Command::Tail { no_follow, run } => {
            let opts = tail::TailOptions {
                color: tail::color_enabled(),
                follow: !no_follow,
                json,
                run,
            };
            let mut stdout = std::io::stdout().lock();
            match tail::run(
                &project,
                &opts,
                &mut stdout,
                &mut tail::SleepTicker::default(),
            ) {
                Ok(()) => keel_cli::EXIT_OK,
                Err(report) => emit(&report, json),
            }
        }
        Command::Trace { flow } => emit(&flows::trace(&project, &flow), json),
        Command::Explain { code } => emit(&explain::run(&code), json),
    };
    exit(code);
}
