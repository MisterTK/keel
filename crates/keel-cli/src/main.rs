//! `keel` — the binary. A thin clap front over [`keel_cli`]: parse, dispatch,
//! print the right half of the [`Rendered`](keel_cli::Rendered) result, exit
//! with its code. All behavior lives in the library so it is unit-testable
//! without spawning a process.

use std::path::PathBuf;
use std::process::exit;

use clap::{Parser, Subcommand};

use keel_cli::render::emit;
use keel_cli::{doctor, explain, flows, init, run, status};
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
    Doctor,
    /// Show one screen of coverage and flow state.
    Status,
    /// List durable (Tier 2) flows: id, entrypoint, status, steps, age.
    Flows {
        /// Show only `dead` flows (those that exhausted their resume cap).
        #[arg(long)]
        dead: bool,
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
            let r = init::run(&project, init::InitOptions { diff, stamp, agents });
            emit(&r, json)
        }
        Command::Doctor => emit(&doctor::run(&project), json),
        Command::Status => emit(&status::run(&project), json),
        Command::Flows { dead } => emit(&flows::flows(&project, dead, SystemClock.now_ms()), json),
        Command::Trace { flow } => emit(&flows::trace(&project, &flow), json),
        Command::Explain { code } => emit(&explain::run(&code), json),
    };
    exit(code);
}
