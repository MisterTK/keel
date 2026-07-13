//! `keel` — the binary. A thin clap front over [`keel_cli`]: parse, dispatch,
//! print the right half of the [`Rendered`](keel_cli::Rendered) result, exit
//! with its code. All behavior lives in the library so it is unit-testable
//! without spawning a process.

use std::path::PathBuf;
use std::process::exit;

use clap::{Parser, Subcommand};

use keel_cli::render::emit;
use keel_cli::{
    doctor, effective, explain, flows, flows_add, flows_suggest, init, mcp, replay, run, status,
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
    /// a subcommand, the Level 2 on-ramp: `suggest` candidates, `add` one.
    Flows {
        /// Show only `dead` flows (those that exhausted their resume cap).
        /// Ignored when a subcommand is given.
        #[arg(long)]
        dead: bool,
        #[command(subcommand)]
        action: Option<FlowsCommand>,
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
            None => emit(&flows::flows(&project, dead, SystemClock.now_ms()), json),
        },
        Command::Mcp => {
            // The server speaks JSON-RPC regardless of --json; it exits on EOF.
            let stdin = std::io::stdin();
            let stdout = std::io::stdout();
            mcp::Server::new(project, || SystemClock.now_ms()).serve(stdin.lock(), stdout.lock())
        }
        Command::Replay { flow, step } => emit(&replay::replay(&project, &flow, step), json),
        Command::Trace { flow } => emit(&flows::trace(&project, &flow), json),
        Command::Explain { code } => emit(&explain::run(&code), json),
    };
    exit(code);
}
