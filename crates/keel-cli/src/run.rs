//! `keel run <script> [args…]` — dispatch a program into its language front end.
//!
//! The heavy lifting (bootstrap, import hook, adapters, discovery) lives in the
//! Python and Node packages; `run` is only the dispatcher (dx-spec §1, Level 0):
//!
//! - `*.py`                     → `python3 -m keel run <script> [args…]`
//! - `*.{mjs,js,ts,cjs,…}`      → `node --import keelrun/hook <script> [args…]`
//! - a `package.json`, or a dir containing one → resolve its `main`, then
//!   the Node path
//! - any other dir              → a conventional entry name (`main.py`,
//!   `__main__.py`, `index.*`), then — if exactly one Python or Node source
//!   file sits directly inside it — that file; ambiguous or empty is a
//!   precise error, never a guess
//! - anything else              → a precise what/why/next error, exit 2
//!
//! The child inherits the environment (so every `KEEL_*` var passes through);
//! `--disable` layers `KEEL_DISABLE=1` on top. On top of that, `keel run`
//! also layers `KEEL_ENABLE=1`/`KEEL_CWD` (set-if-absent — see
//! [`activation_env`]) so any subprocess the child itself spawns
//! self-activates via the keelrun wheel's `.pth` (#63); `--disable` skips
//! this too. The child's exit code is the process's exit code — wrapping is
//! invisible on the success path.

use std::path::{Path, PathBuf};
use std::process::Command;

use serde::Serialize;

use crate::{EXIT_FAILURE, EXIT_USAGE, Rendered};

/// Node's resolver name for the preload hook (the `keelrun` package's `./hook`
/// export). Resolved from the project's `node_modules`, exactly as
/// `node --import keelrun/hook` would in a project that installed `keelrun`.
const NODE_HOOK: &str = "keelrun/hook";

/// The concrete plan: which interpreter to exec with which argv, and whether to
/// disable Keel in the child. Pure data, so dispatch is unit-testable without
/// spawning anything.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RunPlan {
    /// The program to exec (`python3` or `node`).
    pub program: String,
    /// Its full argument vector (excluding `program` itself).
    pub argv: Vec<String>,
    /// Whether to set `KEEL_DISABLE=1` in the child.
    pub disable: bool,
    /// Whether this plan is command mode (#62): an arbitrary PATH-resolvable
    /// command exec'd directly, rather than a script Keel dispatches into a
    /// language front end. Command-mode children skip the Python pre-flight
    /// (the program isn't `python3 -m keel`) — they self-activate purely via
    /// `activation_env`.
    pub command_mode: bool,
}

/// Why a target could not be dispatched — each rendered as what/why/next.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RunError {
    /// The target file/dir does not exist.
    NotFound { target: String },
    /// The extension is not one `keel run` knows how to dispatch.
    UnknownKind { target: String },
    /// A Node package dir/`package.json` had no resolvable entry file.
    NoEntry { target: String },
    /// A directory had more than one plausible entry file and no
    /// `package.json`/conventional name to disambiguate.
    AmbiguousEntry {
        target: String,
        candidates: Vec<String>,
    },
    /// A Node target's `node_modules` (walked up to the filesystem root) has
    /// no `keelrun` — `node --import keelrun/hook` would spawn fine and then
    /// die inside Node's own ESM loader with a raw, unbranded
    /// `ERR_MODULE_NOT_FOUND`, so this is caught before exec'ing at all.
    MissingKeelrun { target: String },
    /// The Python interpreter that `keel run` dispatches to cannot import OUR
    /// `keel` package (`import keel._run`) — either `keelrun` is not installed,
    /// or the UNRELATED PyPI package also named `keel` (an ncurses process
    /// killer) is shadowing it. Without this pre-flight the user gets a raw
    /// "No module named keel.__main__" two steps after `pip install keel`.
    MissingPythonKeelrun { target: String },
    /// The caller's environment sets `KEEL_CWD` to a directory with no
    /// `keel.toml` — the child would refuse to activate (WS1), so fail here.
    PolicyMissingAtKeelCwd { keel_cwd: String },
}

impl RunError {
    pub(crate) fn render(&self) -> Rendered {
        let (what, why, next, kind) = match self {
            Self::NotFound { target } => (
                format!("Cannot run `{target}`: no such file or directory."),
                "The path does not exist relative to the current directory.".to_owned(),
                "Check the path; `keel run` takes a script file, a package.json, a project directory, or a PATH-resolvable command (`keel run -- uvicorn app:app`).".to_owned(),
                "not-found",
            ),
            Self::UnknownKind { target } => (
                format!("Cannot run `{target}`: unrecognized program type."),
                "`keel run` dispatches Python (.py) and Node (.mjs/.js/.ts/.cjs/.mts/.cts/.jsx/.tsx, or a package.json main); this target is neither.".to_owned(),
                "Rename to a supported extension, point at the project's package.json, or invoke the interpreter directly, or launch via the activation env: `KEEL_ENABLE=1 <your command>` with the `keelrun` package installed.".to_owned(),
                "unknown-kind",
            ),
            Self::NoEntry { target } => (
                format!("Cannot run `{target}`: no entry file found."),
                "The directory/package.json has no resolvable `main` (and no index.js).".to_owned(),
                "Add a `main` to package.json, or pass the entry script directly.".to_owned(),
                "no-entry",
            ),
            Self::AmbiguousEntry { target, candidates } => (
                format!("Cannot run `{target}`: multiple possible entry files."),
                format!(
                    "No package.json or conventional entry (main.py, __main__.py, index.*) — \
                     found {} candidate scripts directly inside this directory: {}.",
                    candidates.len(),
                    candidates.join(", ")
                ),
                "Pass the entry script directly, e.g. `keel run <path-to-script>`.".to_owned(),
                "ambiguous-entry",
            ),
            Self::MissingKeelrun { target } => (
                format!("Cannot run `{target}`: the `keelrun` package is not installed."),
                "Node targets are dispatched via `node --import keelrun/hook`, which needs the \
                 `keelrun` package in `node_modules` for runtime interception — only \
                 `keelrun-cli` (the binary) was found."
                    .to_owned(),
                "Run `npm install keelrun` alongside `keelrun-cli` in this project.".to_owned(),
                "missing-keelrun",
            ),
            Self::MissingPythonKeelrun { target } => (
                format!("Cannot run `{target}`: the `keelrun` package is not importable."),
                "Python targets are dispatched via `python3 -m keel run`, which needs the \
                 `keelrun` PyPI package (import name `keel`). Either it is not installed, \
                 or an unrelated package also named `keel` is installed instead — \
                 `pip install keel` is a different project's process-killer utility, not \
                 this tool."
                    .to_owned(),
                "Run `pip install keelrun` — NOT `pip install keel` — in this project's \
                 Python environment."
                    .to_owned(),
                "missing-keelrun-py",
            ),
            Self::PolicyMissingAtKeelCwd { keel_cwd } => (
                format!("Cannot run with KEEL_CWD={keel_cwd}: {keel_cwd}/keel.toml does not exist — Keel NOT activated."),
                "KEEL_CWD asserts where keel.toml lives; when the file is not there the policy \
                 did not ship (a missing COPY in the image, a wrong path) and Keel refuses to run \
                 on production defaults silently."
                    .to_owned(),
                "Point KEEL_CWD at the directory that holds keel.toml (or unset it to use the \
                 working directory), or set KEEL_POLICY=optional to run on production defaults."
                    .to_owned(),
                "policy-missing-at-keel-cwd",
            ),
        };
        let human = format!("keel \u{25b8} {what}\n  why:  {why}\n  next: {next}");
        let report = RunErrorReport {
            error: kind,
            next: &next,
            what: &what,
            why: &why,
        };
        Rendered {
            human,
            json: crate::render::to_json(&report),
            exit: EXIT_USAGE,
            to_stderr: true,
        }
    }
}

/// The machine twin of a dispatch failure.
#[derive(Debug, Serialize)]
struct RunErrorReport<'a> {
    error: &'static str,
    next: &'a str,
    what: &'a str,
    why: &'a str,
}

/// Node source extensions `keel run` dispatches.
const NODE_EXTS: &[&str] = &["mjs", "js", "ts", "cjs", "mts", "cts", "jsx", "tsx"];

/// Build the [`RunPlan`] for `target` and `args`. Reads the filesystem to
/// classify the target and (for a package) to resolve its entry file.
pub fn plan(target: &str, args: &[String], disable: bool) -> Result<RunPlan, RunError> {
    let path = Path::new(target);

    // package.json passed explicitly, or a directory containing one.
    if path.file_name().is_some_and(|n| n == "package.json") {
        return node_package(path, args, disable);
    }
    if path.is_dir() {
        let manifest = path.join("package.json");
        if manifest.exists() {
            return node_package(&manifest, args, disable);
        }
        return resolve_directory(target, path, args, disable);
    }
    if !path.exists() {
        if let Some(command) = command_plan(target, args, disable) {
            return Ok(command);
        }
        return Err(RunError::NotFound {
            target: target.to_owned(),
        });
    }

    match path.extension().and_then(|e| e.to_str()) {
        Some("py") => Ok(python_plan(target, args, disable)),
        Some(ext) if NODE_EXTS.contains(&ext) => node_plan(target, args, disable),
        _ => Err(RunError::UnknownKind {
            target: target.to_owned(),
        }),
    }
}

fn python_plan(target: &str, extra: &[String], disable: bool) -> RunPlan {
    let mut argv = vec![
        "-m".to_owned(),
        "keel".to_owned(),
        "run".to_owned(),
        target.to_owned(),
    ];
    argv.extend_from_slice(extra);
    RunPlan {
        program: "python3".to_owned(),
        argv,
        disable,
        command_mode: false,
    }
}

/// Pin a Node target as a path operand, never a Node option. Node parses any
/// argv entry beginning with `-` (before the entry point) as a flag, so a file
/// literally named `--inspect-brk=0.0.0.0:9229.js` — a valid filename that
/// passes the exists/extension checks — would open an unauthenticated debug port
/// instead of running as a script. Prefixing a relative target with `./`
/// (absolute paths and already-dot-prefixed paths are left as-is) makes it
/// unambiguously a path. The Python path is immune (its target lands after
/// `-m keel run`).
fn as_script_operand(target: &str) -> String {
    if target.starts_with('/') || target.starts_with("./") || target.starts_with("../") {
        target.to_owned()
    } else {
        format!("./{target}")
    }
}

fn node_plan(target: &str, extra: &[String], disable: bool) -> Result<RunPlan, RunError> {
    if !keelrun_resolvable(Path::new(target)) {
        return Err(RunError::MissingKeelrun {
            target: target.to_owned(),
        });
    }
    let mut argv = vec![
        "--import".to_owned(),
        NODE_HOOK.to_owned(),
        as_script_operand(target),
    ];
    argv.extend_from_slice(extra);
    Ok(RunPlan {
        program: "node".to_owned(),
        argv,
        disable,
        command_mode: false,
    })
}

/// Command mode (#62): `keel run <cmd> [args…]` (also reachable as
/// `keel run -- <cmd> …` — clap strips the `--`). The target must be a bare
/// word (no path separator), must NOT carry a known script extension (a
/// typo'd `app.py` stays a NotFound, never a surprise exec), and must resolve
/// on PATH. Keel wraps nothing in-process here; children self-activate via
/// `activation_env` + the keelrun wheel's `.pth`, which covers console
/// scripts (`uvicorn`), `uv run …`, and `python -m pkg` launches.
fn command_plan(target: &str, args: &[String], disable: bool) -> Option<RunPlan> {
    if target.contains(std::path::MAIN_SEPARATOR) || target.contains('/') {
        return None;
    }
    let ext = Path::new(target).extension().and_then(|e| e.to_str());
    if matches!(ext, Some("py")) || ext.is_some_and(|e| NODE_EXTS.contains(&e)) {
        return None;
    }
    // #68: on Windows, `dir.join(target).is_file()` alone misses a bare
    // command name that only exists with a PATHEXT extension (`uvicorn` ->
    // `uvicorn.exe`) — command mode would silently degrade to NotFound even
    // though the OS's own search (and Rust's `Command::new` spawn below)
    // would find it fine. `cfg!(windows)` is a compile-time constant, so
    // this is dead-code-eliminated on non-Windows builds.
    let found = std::env::split_paths(&std::env::var_os("PATH")?).any(|dir| {
        if dir.as_os_str().is_empty() {
            return false;
        }
        if dir.join(target).is_file() {
            return true;
        }
        cfg!(windows)
            && Path::new(target).extension().is_none()
            && pathext_candidates(
                target,
                &std::env::var("PATHEXT").unwrap_or_else(|_| ".COM;.EXE;.BAT;.CMD".to_owned()),
            )
            .iter()
            .any(|c| dir.join(c).is_file())
    });
    if !found {
        return None;
    }
    Some(RunPlan {
        program: target.to_owned(),
        argv: args.to_vec(),
        disable,
        command_mode: true,
    })
}

/// Extensions Windows' `PATHEXT` search would try for a bare command name
/// (e.g. `uvicorn` -> `uvicorn.EXE`). Not `cfg(windows)`-gated so it is
/// unit-testable on any host — only its call site above is Windows-only;
/// PATHEXT does not exist as a concept on Unix, where an exact-name match
/// is already correct.
fn pathext_candidates(target: &str, pathext: &str) -> Vec<String> {
    pathext
        .split(';')
        .map(str::trim)
        .filter(|ext| !ext.is_empty())
        .map(|ext| format!("{target}{ext}"))
        .collect()
}

/// Whether `keelrun` would resolve from `target`, mirroring Node's own
/// `node_modules` walk-up: starting at `target`'s containing directory
/// (canonicalized, so a bare relative filename resolves against the real
/// current directory), check `<dir>/node_modules/keelrun`, then each parent
/// in turn up to and including the filesystem root.
fn keelrun_resolvable(target: &Path) -> bool {
    let start = if target.is_dir() {
        target.to_path_buf()
    } else {
        match target.parent() {
            Some(p) if !p.as_os_str().is_empty() => p.to_path_buf(),
            _ => PathBuf::from("."),
        }
    };
    let mut dir = std::fs::canonicalize(&start).unwrap_or(start);
    loop {
        if dir.join("node_modules").join("keelrun").is_dir() {
            return true;
        }
        if !dir.pop() {
            return false;
        }
    }
}

/// Resolve a package's entry file from its `package.json` `main` (default
/// `index.js`), then dispatch it via the Node path.
fn node_package(manifest: &Path, args: &[String], disable: bool) -> Result<RunPlan, RunError> {
    let dir = manifest.parent().unwrap_or_else(|| Path::new("."));
    let text = std::fs::read_to_string(manifest).map_err(|_| RunError::NoEntry {
        target: manifest.display().to_string(),
    })?;
    let main = serde_json::from_str::<serde_json::Value>(&text)
        .ok()
        .and_then(|v| v.get("main").and_then(|m| m.as_str()).map(str::to_owned))
        .unwrap_or_else(|| "index.js".to_owned());
    let entry: PathBuf = dir.join(main);
    if !entry.exists() {
        return Err(RunError::NoEntry {
            target: manifest.display().to_string(),
        });
    }
    node_plan(&entry.to_string_lossy(), args, disable)
}

/// Conventional entry file names tried, in order, before falling back to a
/// directory walk.
const PY_CONVENTIONAL_ENTRIES: &[&str] = &["main.py", "__main__.py"];
const NODE_CONVENTIONAL_ENTRIES: &[&str] = &[
    "index.mjs",
    "index.js",
    "index.cjs",
    "index.ts",
    "index.mts",
    "index.cts",
];

/// Resolve a directory target with no `package.json`: a conventional entry
/// name first, then — if exactly one Python or Node source file sits
/// directly inside the directory (not recursively; a nested script is never
/// guessed at) — that file. Ambiguous or empty is a precise error, never a
/// silent guess (dx-spec's "a Level 0 surprise is a P0 bug" invariant).
fn resolve_directory(
    target: &str,
    dir: &Path,
    args: &[String],
    disable: bool,
) -> Result<RunPlan, RunError> {
    for name in PY_CONVENTIONAL_ENTRIES {
        let candidate = dir.join(name);
        if candidate.is_file() {
            return Ok(python_plan(&candidate.to_string_lossy(), args, disable));
        }
    }
    for name in NODE_CONVENTIONAL_ENTRIES {
        let candidate = dir.join(name);
        if candidate.is_file() {
            return node_plan(&candidate.to_string_lossy(), args, disable);
        }
    }

    let py_files = top_level_files_with_extension(dir, &["py"]);
    let node_files = top_level_files_with_extension(dir, NODE_EXTS);
    let mut candidates: Vec<PathBuf> = py_files.iter().chain(&node_files).cloned().collect();
    candidates.sort();

    match candidates.as_slice() {
        [] => Err(RunError::UnknownKind {
            target: target.to_owned(),
        }),
        [only] => {
            if py_files.contains(only) {
                Ok(python_plan(&only.to_string_lossy(), args, disable))
            } else {
                node_plan(&only.to_string_lossy(), args, disable)
            }
        }
        many => Err(RunError::AmbiguousEntry {
            target: target.to_owned(),
            candidates: many.iter().map(|p| p.display().to_string()).collect(),
        }),
    }
}

/// Regular files directly inside `dir` (no recursion into subdirectories)
/// whose extension is one of `extensions`, sorted. Deliberately shallow —
/// unlike [`crate::scan::collect_files`]'s recursive project-wide scan, `keel
/// run`'s directory disambiguation only ever considers the top level.
fn top_level_files_with_extension(dir: &Path, extensions: &[&str]) -> Vec<PathBuf> {
    let mut out = Vec::new();
    let Ok(entries) = std::fs::read_dir(dir) else {
        return out;
    };
    for entry in entries.flatten() {
        let path = entry.path();
        if path.is_file()
            && path
                .extension()
                .and_then(|e| e.to_str())
                .is_some_and(|e| extensions.contains(&e))
        {
            out.push(path);
        }
    }
    out.sort();
    out
}

/// Execute a [`RunPlan`], inheriting the environment (so `KEEL_*` passes
/// through). Returns the child's exit code, or a rendered spawn error.
pub fn exec(plan: &RunPlan) -> Result<i32, Rendered> {
    exec_with(plan, |_cmd| {})
}

/// Like [`exec`], but lets the caller layer extra environment onto the child
/// before it spawns (`keel record run` sets `KEEL_RECORD` this way — see
/// `crate::record`). `exec` is exactly `exec_with(plan, |_| {})`.
pub(crate) fn exec_with(
    plan: &RunPlan,
    configure: impl FnOnce(&mut Command),
) -> Result<i32, Rendered> {
    let mut cmd = Command::new(&plan.program);
    cmd.args(&plan.argv);
    if plan.disable {
        cmd.env("KEEL_DISABLE", "1");
    }
    configure(&mut cmd);
    match cmd.status() {
        Ok(status) => Ok(status.code().unwrap_or(EXIT_FAILURE)),
        Err(err) => {
            let what = format!("Cannot run `{}`: {err}.", plan.program);
            let why = format!(
                "`{}` was not found on PATH or could not be started.",
                plan.program
            );
            // #62/finding 4: command mode execs `plan.program` directly (no
            // Python/Node dispatch involved at all) — the install hint below
            // only applies to the two interpreters `keel run` itself
            // dispatches into.
            let next: String = if plan.command_mode {
                format!(
                    "`{}` is not installed, not on PATH, or not executable — check the command \
                     name and permissions.",
                    plan.program
                )
            } else if plan.program == "python3" {
                "Install Python 3 and the `keelrun` package (`pip install keelrun`).".to_owned()
            } else {
                "Install Node.js and the `keelrun` package (`npm i -D keelrun`).".to_owned()
            };
            let human = format!("keel \u{25b8} {what}\n  why:  {why}\n  next: {next}");
            let report = RunErrorReport {
                error: "spawn-failed",
                next: &next,
                what: &what,
                why: &why,
            };
            Err(Rendered {
                human,
                json: crate::render::to_json(&report),
                exit: EXIT_FAILURE,
                to_stderr: true,
            })
        }
    }
}

/// `python3 -c` probe: exit 0 iff our package is importable. `find_spec`
/// resolves without importing, so the foreign `keel==0.1` (no `_run`
/// submodule) and a missing install both exit non-zero.
const PY_KEELRUN_PROBE: &str =
    "import importlib.util, sys; sys.exit(0 if importlib.util.find_spec('keel._run') else 3)";

fn keelrun_importable(python: &str) -> bool {
    Command::new(python)
        .args(["-c", PY_KEELRUN_PROBE])
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .status()
        // Interpreter missing/unspawnable: not this error's job — fall through
        // so exec_with's spawn-failed path reports it with its install hint.
        .map_or(true, |s| s.success())
}

/// Pre-flight for Python plans, mirroring the Node `MissingKeelrun` walk:
/// catch the wrong-`keel`-package trap before exec'ing (#61). ~one python
/// startup (tens of ms), only on the run/record paths.
pub(crate) fn python_preflight(target: &str, plan: &RunPlan) -> Option<Rendered> {
    if plan.program == "python3" && !keelrun_importable(&plan.program) {
        return Some(
            RunError::MissingPythonKeelrun {
                target: target.to_owned(),
            }
            .render(),
        );
    }
    None
}

/// Env pairs that make CHILD processes of the wrapped program self-activate
/// via the keelrun wheel's `.pth` (`KEEL_ENABLE` gate) — the zero-effort
/// subprocess-inheritance path (#63). Set-if-absent: a user's explicit value
/// (including a falsy one) always wins; `--disable` exports nothing (the
/// child env already carries KEEL_DISABLE=1, which beats KEEL_ENABLE anyway).
pub(crate) fn activation_env(plan: &RunPlan) -> Vec<(String, String)> {
    let cwd = std::env::current_dir().unwrap_or_else(|_| PathBuf::from("."));
    activation_env_in(plan, &cwd, &|k| std::env::var_os(k).is_some())
}

/// The testable core of [`activation_env`]: `cwd` is the directory whose
/// `keel.toml` would be advertised, `ambient(name)` says whether the caller's
/// environment already sets `name`. `KEEL_CWD` is exported ONLY when
/// `<cwd>/keel.toml` exists — since 0.5.5 a child refuses to activate under a
/// `KEEL_CWD` with no policy (WS1), so advertising an empty root would break
/// every unconfigured project's `keel run`. An unconfigured child falls back
/// to its own cwd and Level 0 defaults exactly as before.
pub(crate) fn activation_env_in(
    plan: &RunPlan,
    cwd: &Path,
    ambient: &dyn Fn(&str) -> bool,
) -> Vec<(String, String)> {
    if plan.disable {
        return Vec::new();
    }
    let mut env = Vec::new();
    if !ambient("KEEL_ENABLE") {
        env.push(("KEEL_ENABLE".to_owned(), "1".to_owned()));
    }
    if !ambient("KEEL_CWD") && cwd.join("keel.toml").is_file() {
        env.push(("KEEL_CWD".to_owned(), cwd.to_string_lossy().into_owned()));
    }
    env
}

/// An ambient `KEEL_CWD` naming a directory with no `keel.toml` would make the
/// child refuse to activate (WS1); an explicit `keel run` can fail properly
/// instead, before anything launches. `KEEL_POLICY=optional` opts out.
pub(crate) fn keel_cwd_preflight_in(
    keel_cwd: Option<&str>,
    policy_optional: bool,
) -> Option<Rendered> {
    let root = keel_cwd?.trim();
    if root.is_empty() || policy_optional || Path::new(root).join("keel.toml").is_file() {
        return None;
    }
    Some(
        RunError::PolicyMissingAtKeelCwd {
            keel_cwd: root.to_owned(),
        }
        .render(),
    )
}

pub(crate) fn keel_cwd_preflight() -> Option<Rendered> {
    let keel_cwd = std::env::var("KEEL_CWD").ok();
    let optional =
        std::env::var("KEEL_POLICY").is_ok_and(|v| v.trim().eq_ignore_ascii_case("optional"));
    keel_cwd_preflight_in(keel_cwd.as_deref(), optional)
}

/// The stderr banner `keel run` prints once a plan resolves to command mode
/// (#62): which self-activation mechanism the exec'd child gets. `None`
/// under `plan.disable` — `--disable` makes [`activation_env`] export
/// nothing (no `KEEL_ENABLE=1` reaches the child; `exec_with` sets
/// `KEEL_DISABLE=1` instead), so claiming `KEEL_ENABLE=1` there would be
/// actively wrong, not just incomplete.
fn command_mode_banner(target: &str, plan: &RunPlan) -> Option<String> {
    if plan.disable {
        return None;
    }
    Some(format!(
        "keel \u{25b8} command mode: exec `{target}` with KEEL_ENABLE=1 \u{2014} Python \
         children self-activate via the `keelrun` wheel (pip install keelrun); Node \
         children need NODE_OPTIONS=\"--import keelrun/register\"."
    ))
}

/// The whole `keel run` command: plan, then exec. On a dispatch error render it;
/// on success return the child's exit code.
pub fn run(target: &str, args: &[String], disable: bool) -> (Option<Rendered>, i32) {
    match plan(target, args, disable) {
        Err(e) => {
            let r = e.render();
            let code = r.exit;
            (Some(r), code)
        }
        Ok(plan) => {
            if let Some(r) = keel_cwd_preflight() {
                let code = r.exit;
                return (Some(r), code);
            }
            if plan.command_mode {
                if let Some(banner) = command_mode_banner(target, &plan) {
                    eprintln!("{banner}");
                }
            } else if let Some(r) = python_preflight(target, &plan) {
                let code = r.exit;
                return (Some(r), code);
            }
            match exec_with(&plan, |cmd| {
                cmd.envs(activation_env(&plan));
            }) {
                Ok(code) => (None, code),
                Err(r) => {
                    let code = r.exit;
                    (Some(r), code)
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use tempfile::TempDir;

    #[test]
    fn pathext_candidates_appends_each_extension() {
        assert_eq!(
            pathext_candidates("uvicorn", ".COM;.EXE;.BAT;.CMD"),
            vec!["uvicorn.COM", "uvicorn.EXE", "uvicorn.BAT", "uvicorn.CMD"]
        );
    }

    #[test]
    fn pathext_candidates_ignores_empty_segments() {
        assert_eq!(
            pathext_candidates("x", ";.EXE;;.BAT;"),
            vec!["x.EXE", "x.BAT"]
        );
    }

    #[cfg(windows)]
    #[test]
    fn command_plan_finds_a_bare_name_via_pathext() {
        use std::fs;
        use tempfile::TempDir;

        let dir = TempDir::new().unwrap();
        fs::write(dir.path().join("uvicorn.EXE"), b"").unwrap();
        let orig_path = std::env::var_os("PATH");
        // SAFETY: test-local env mutation, restored immediately after.
        unsafe {
            std::env::set_var("PATH", dir.path());
        }
        let plan = command_plan("uvicorn", &[], false);
        unsafe {
            match orig_path {
                Some(p) => std::env::set_var("PATH", p),
                None => std::env::remove_var("PATH"),
            }
        }
        assert!(
            plan.is_some(),
            "PATHEXT-extended name must be found on PATH"
        );
        assert!(plan.unwrap().command_mode);
    }

    #[test]
    fn python_target_dispatches_to_python_module() {
        let dir = TempDir::new().unwrap();
        let script = dir.path().join("app.py");
        fs::write(&script, "print('hi')\n").unwrap();
        let plan = plan(&script.to_string_lossy(), &["--flag".into()], false).unwrap();
        assert_eq!(plan.program, "python3");
        assert_eq!(
            plan.argv,
            vec![
                "-m",
                "keel",
                "run",
                script.to_string_lossy().as_ref(),
                "--flag"
            ]
        );
    }

    #[test]
    fn node_target_dispatches_with_hook_import() {
        let dir = TempDir::new().unwrap();
        let script = dir.path().join("app.mjs");
        fs::write(&script, "console.log('hi')\n").unwrap();
        fs::create_dir_all(dir.path().join("node_modules").join("keelrun")).unwrap();
        let plan = plan(&script.to_string_lossy(), &[], true).unwrap();
        assert_eq!(plan.program, "node");
        assert_eq!(plan.argv[0], "--import");
        assert_eq!(plan.argv[1], "keelrun/hook");
        assert!(plan.disable);
    }

    #[test]
    fn node_target_without_keelrun_installed_is_a_precise_error() {
        // keelrun-cli alone (no `node_modules/keelrun`) — the documented
        // CLI-only install gap (issue #33): node would spawn fine and then
        // die inside its own ESM loader with a raw, unbranded error.
        let dir = TempDir::new().unwrap();
        let script = dir.path().join("app.mjs");
        fs::write(&script, "console.log('hi')\n").unwrap();
        let err = plan(&script.to_string_lossy(), &[], false).unwrap_err();
        assert_eq!(
            err,
            RunError::MissingKeelrun {
                target: script.to_string_lossy().into_owned()
            }
        );
        let rendered = err.render();
        assert_eq!(rendered.exit, EXIT_USAGE);
        assert!(rendered.human.contains("npm install keelrun"));
        assert_eq!(rendered.json["error"], "missing-keelrun");
    }

    #[test]
    fn node_target_finds_keelrun_in_an_ancestor_node_modules() {
        // Mirrors Node's own walk-up resolution: `node_modules/keelrun` one
        // level above the script's directory still resolves.
        let dir = TempDir::new().unwrap();
        fs::create_dir_all(dir.path().join("node_modules").join("keelrun")).unwrap();
        let sub = dir.path().join("src");
        fs::create_dir_all(&sub).unwrap();
        let script = sub.join("app.mjs");
        fs::write(&script, "console.log('hi')\n").unwrap();
        let plan = plan(&script.to_string_lossy(), &[], false).unwrap();
        assert_eq!(plan.program, "node");
    }

    #[test]
    fn node_dash_named_target_is_pinned_as_a_path_operand() {
        // A relative target that would parse as a Node option is prefixed with
        // `./`; absolute and dot-prefixed paths are already unambiguous.
        assert_eq!(
            as_script_operand("--inspect-brk=0.0.0.0:9229.js"),
            "./--inspect-brk=0.0.0.0:9229.js"
        );
        assert_eq!(as_script_operand("app.mjs"), "./app.mjs");
        assert_eq!(as_script_operand("sub/app.mjs"), "./sub/app.mjs");
        assert_eq!(as_script_operand("/abs/app.mjs"), "/abs/app.mjs");
        assert_eq!(as_script_operand("./app.mjs"), "./app.mjs");
        assert_eq!(as_script_operand("../app.mjs"), "../app.mjs");
    }

    #[test]
    fn package_json_main_is_resolved() {
        let dir = TempDir::new().unwrap();
        fs::write(
            dir.path().join("package.json"),
            "{ \"main\": \"start.mjs\" }",
        )
        .unwrap();
        fs::write(dir.path().join("start.mjs"), "// entry\n").unwrap();
        fs::create_dir_all(dir.path().join("node_modules").join("keelrun")).unwrap();
        let plan = plan(&dir.path().to_string_lossy(), &[], false).unwrap();
        assert_eq!(plan.program, "node");
        assert!(plan.argv[2].ends_with("start.mjs"));
    }

    #[test]
    fn package_json_defaults_to_index_js() {
        let dir = TempDir::new().unwrap();
        fs::write(dir.path().join("package.json"), "{}").unwrap();
        fs::write(dir.path().join("index.js"), "// entry\n").unwrap();
        fs::create_dir_all(dir.path().join("node_modules").join("keelrun")).unwrap();
        let plan = plan(&dir.path().to_string_lossy(), &[], false).unwrap();
        assert!(plan.argv[2].ends_with("index.js"));
    }

    #[test]
    fn path_resolvable_bare_word_enters_command_mode() {
        // `sh` exists on PATH everywhere we test.
        let plan = plan("sh", &["-c".to_owned(), "exit 0".to_owned()], false).unwrap();
        assert!(plan.command_mode);
        assert_eq!(plan.program, "sh");
        assert_eq!(plan.argv, vec!["-c", "exit 0"]);
    }

    #[test]
    fn nonexistent_word_not_on_path_is_still_not_found() {
        let err = plan("definitely-not-a-real-binary-xyz", &[], false).unwrap_err();
        assert!(matches!(err, RunError::NotFound { .. }));
    }

    #[test]
    fn nonexistent_script_extension_never_enters_command_mode() {
        // A typo'd script name must stay a NotFound, even if a same-named binary
        // could exist: known extensions always mean "script mode intended".
        let err = plan("missing.py", &[], false).unwrap_err();
        assert!(matches!(err, RunError::NotFound { .. }));
    }

    #[test]
    fn path_separator_targets_never_enter_command_mode() {
        let err = plan("./no/such/dir", &[], false).unwrap_err();
        assert!(matches!(err, RunError::NotFound { .. }));
    }

    #[test]
    fn command_mode_exit_code_propagates() {
        let plan = plan("sh", &["-c".to_owned(), "exit 7".to_owned()], false).unwrap();
        assert_eq!(exec(&plan).unwrap(), 7);
    }

    #[test]
    fn missing_file_is_not_found() {
        assert_eq!(
            plan("does-not-exist.py", &[], false),
            Err(RunError::NotFound {
                target: "does-not-exist.py".into()
            })
        );
    }

    #[test]
    fn unknown_extension_is_a_precise_error() {
        let dir = TempDir::new().unwrap();
        let f = dir.path().join("script.rb");
        fs::write(&f, "puts 1\n").unwrap();
        let err = plan(&f.to_string_lossy(), &[], false).unwrap_err();
        assert!(matches!(err, RunError::UnknownKind { .. }));
        let rendered = err.render();
        assert_eq!(rendered.exit, EXIT_USAGE);
        assert!(rendered.human.contains("next:"));
        assert_eq!(rendered.json["error"], "unknown-kind");
    }

    #[test]
    fn package_dir_without_entry_errors() {
        let dir = TempDir::new().unwrap();
        fs::write(dir.path().join("package.json"), "{ \"main\": \"nope.js\" }").unwrap();
        let err = plan(&dir.path().to_string_lossy(), &[], false).unwrap_err();
        assert!(matches!(err, RunError::NoEntry { .. }));
    }

    #[test]
    fn dir_with_conventional_python_entry_resolves_to_main_py() {
        let dir = TempDir::new().unwrap();
        fs::write(dir.path().join("main.py"), "print('hi')\n").unwrap();
        fs::write(dir.path().join("helpers.py"), "# not the entry\n").unwrap();
        let plan = plan(&dir.path().to_string_lossy(), &[], false).unwrap();
        assert_eq!(plan.program, "python3");
        assert!(plan.argv.last().unwrap().ends_with("main.py"));
    }

    #[test]
    fn dir_with_dunder_main_resolves_when_no_main_py() {
        let dir = TempDir::new().unwrap();
        fs::write(dir.path().join("__main__.py"), "print('hi')\n").unwrap();
        let plan = plan(&dir.path().to_string_lossy(), &[], false).unwrap();
        assert!(plan.argv.last().unwrap().ends_with("__main__.py"));
    }

    #[test]
    fn dir_with_conventional_node_entry_resolves_without_package_json() {
        let dir = TempDir::new().unwrap();
        fs::write(dir.path().join("index.mjs"), "// entry\n").unwrap();
        fs::create_dir_all(dir.path().join("node_modules").join("keelrun")).unwrap();
        let plan = plan(&dir.path().to_string_lossy(), &[], false).unwrap();
        assert_eq!(plan.program, "node");
        assert!(plan.argv[2].ends_with("index.mjs"));
    }

    #[test]
    fn dir_with_sole_python_file_resolves_by_walk() {
        let dir = TempDir::new().unwrap();
        fs::write(dir.path().join("pipeline.py"), "print('hi')\n").unwrap();
        let plan = plan(&dir.path().to_string_lossy(), &[], false).unwrap();
        assert_eq!(plan.program, "python3");
        assert!(plan.argv.last().unwrap().ends_with("pipeline.py"));
    }

    #[test]
    fn dir_with_multiple_candidates_is_ambiguous() {
        let dir = TempDir::new().unwrap();
        fs::write(dir.path().join("a.py"), "print(1)\n").unwrap();
        fs::write(dir.path().join("b.py"), "print(2)\n").unwrap();
        let err = plan(&dir.path().to_string_lossy(), &[], false).unwrap_err();
        assert!(matches!(err, RunError::AmbiguousEntry { .. }));
        let rendered = err.render();
        assert_eq!(rendered.exit, EXIT_USAGE);
        assert_eq!(rendered.json["error"], "ambiguous-entry");
    }

    #[test]
    fn empty_dir_is_still_unknown_kind() {
        let dir = TempDir::new().unwrap();
        let err = plan(&dir.path().to_string_lossy(), &[], false).unwrap_err();
        assert!(matches!(err, RunError::UnknownKind { .. }));
    }

    #[test]
    fn nested_scripts_are_never_guessed_at() {
        // A subdirectory's scripts are not candidates — resolution is
        // top-level only, so an otherwise-empty dir with only nested files
        // still errors rather than reaching into a subdirectory.
        let dir = TempDir::new().unwrap();
        let sub = dir.path().join("nested");
        fs::create_dir(&sub).unwrap();
        fs::write(sub.join("deep.py"), "print('hi')\n").unwrap();
        let err = plan(&dir.path().to_string_lossy(), &[], false).unwrap_err();
        assert!(matches!(err, RunError::UnknownKind { .. }));
    }

    #[test]
    fn child_exit_code_propagates() {
        // `keel run` is invisible on the success path — the child's exit code is
        // the process's exit code (dx-spec §1). Drive the exec path directly with
        // a shell that exits 7.
        let plan = RunPlan {
            program: "sh".to_owned(),
            argv: vec!["-c".to_owned(), "exit 7".to_owned()],
            disable: false,
            command_mode: false,
        };
        assert_eq!(exec(&plan).expect("sh should spawn"), 7);
    }

    #[test]
    fn exec_with_layers_extra_env_onto_the_child() {
        // `keel record run` (crate::record) relies on this to thread
        // `KEEL_RECORD` into the child without duplicating `exec`'s spawn
        // logic — prove the closure actually reaches the child's environment.
        let plan = RunPlan {
            program: "sh".to_owned(),
            argv: vec![
                "-c".to_owned(),
                "[ \"$KEEL_RECORD_TEST\" = \"marker\" ] && exit 0 || exit 9".to_owned(),
            ],
            disable: false,
            command_mode: false,
        };
        let code = exec_with(&plan, |cmd| {
            cmd.env("KEEL_RECORD_TEST", "marker");
        })
        .expect("sh should spawn");
        assert_eq!(code, 0);
    }

    #[test]
    fn missing_python_keelrun_is_a_precise_error() {
        let r = RunError::MissingPythonKeelrun {
            target: "app.py".to_owned(),
        }
        .render();
        assert_eq!(r.exit, EXIT_USAGE);
        assert!(r.to_stderr);
        assert!(r.human.contains("pip install keelrun"));
        assert!(r.human.contains("not `keel`") || r.human.contains("NOT `pip install keel`"));
        assert_eq!(r.json["error"], "missing-keelrun-py");
    }

    #[cfg(unix)]
    #[test]
    fn keelrun_probe_trusts_a_zero_exit_and_distrusts_nonzero() {
        // The probe passes ["-c", <code>] to the interpreter; a shim that ignores
        // its args and exits 0/1 exercises the plumbing without needing python.
        let dir = TempDir::new().unwrap();
        let ok = dir.path().join("ok.sh");
        fs::write(&ok, "#!/bin/sh\nexit 0\n").unwrap();
        let bad = dir.path().join("bad.sh");
        fs::write(&bad, "#!/bin/sh\nexit 3\n").unwrap();
        for p in [&ok, &bad] {
            let mut perms = fs::metadata(p).unwrap().permissions();
            std::os::unix::fs::PermissionsExt::set_mode(&mut perms, 0o755);
            fs::set_permissions(p, perms).unwrap();
        }
        assert!(keelrun_importable(ok.to_str().unwrap()));
        assert!(!keelrun_importable(bad.to_str().unwrap()));
        // A missing interpreter is NOT this error's job — spawn failure reports it.
        assert!(keelrun_importable(
            dir.path().join("absent").to_str().unwrap()
        ));
    }

    fn plan_sh(script: &str) -> RunPlan {
        RunPlan {
            program: "sh".to_owned(),
            argv: vec!["-c".to_owned(), script.to_owned()],
            disable: false,
            command_mode: false,
        }
    }

    #[test]
    fn activation_env_exports_keel_cwd_only_when_a_policy_exists_there() {
        // WS1: KEEL_CWD is an assertion that policy lives there, so never
        // export one that would make the child refuse to activate.
        let dir = TempDir::new().unwrap();
        let plan = plan_sh("exit 0");
        let none = |_: &str| false;
        let without = activation_env_in(&plan, dir.path(), &none);
        assert_eq!(
            without.iter().map(|(k, _)| k.as_str()).collect::<Vec<_>>(),
            vec!["KEEL_ENABLE"],
            "no keel.toml → no KEEL_CWD export"
        );
        fs::write(dir.path().join("keel.toml"), "").unwrap();
        let mut with = activation_env_in(&plan, dir.path(), &none);
        with.sort();
        assert_eq!(
            with,
            vec![
                (
                    "KEEL_CWD".to_owned(),
                    dir.path().to_string_lossy().into_owned()
                ),
                ("KEEL_ENABLE".to_owned(), "1".to_owned()),
            ]
        );
    }

    #[test]
    fn activation_env_is_set_if_absent() {
        let dir = TempDir::new().unwrap();
        fs::write(dir.path().join("keel.toml"), "").unwrap();
        let plan = plan_sh("exit 0");
        let all = |_: &str| true; // the caller already set both
        assert!(activation_env_in(&plan, dir.path(), &all).is_empty());
    }

    #[test]
    fn activation_env_reaches_the_child() {
        let dir = TempDir::new().unwrap();
        fs::write(dir.path().join("keel.toml"), "").unwrap();
        let plan = plan_sh(r#"[ "$KEEL_ENABLE" = "1" ] && [ -n "$KEEL_CWD" ] && exit 0 || exit 9"#);
        let none = |_: &str| false;
        let env = activation_env_in(&plan, dir.path(), &none);
        assert_eq!(
            exec_with(&plan, |cmd| {
                cmd.envs(env.clone());
            })
            .unwrap(),
            0
        );
    }

    #[test]
    fn keel_cwd_preflight_rejects_a_root_without_keel_toml() {
        let dir = TempDir::new().unwrap();
        let root = dir.path().to_string_lossy().into_owned();
        let r = keel_cwd_preflight_in(Some(&root), false).expect("must refuse");
        assert_eq!(r.exit, EXIT_USAGE);
        assert!(r.to_stderr);
        assert_eq!(r.json["error"], "policy-missing-at-keel-cwd");
        assert!(r.human.contains(&format!("KEEL_CWD={root}")), "{}", r.human);
        // Present file, optional, unset, blank: all pass.
        fs::write(dir.path().join("keel.toml"), "").unwrap();
        assert!(keel_cwd_preflight_in(Some(&root), false).is_none());
        fs::remove_file(dir.path().join("keel.toml")).unwrap();
        assert!(keel_cwd_preflight_in(Some(&root), true).is_none());
        assert!(keel_cwd_preflight_in(None, false).is_none());
        assert!(keel_cwd_preflight_in(Some("  "), false).is_none());
    }

    #[test]
    fn disable_suppresses_activation_env() {
        let plan = RunPlan {
            program: "sh".to_owned(),
            argv: vec![],
            disable: true,
            command_mode: false,
        };
        assert!(activation_env(&plan).is_empty());
    }

    #[test]
    fn spawn_failure_is_a_framed_error_with_exit_1() {
        // A program that cannot be spawned surfaces a what/why/next error on
        // stderr and exit 1 (an underlying failure, not a usage error).
        let plan = RunPlan {
            program: "keel-nonexistent-program-9f3a".to_owned(),
            argv: vec![],
            disable: false,
            command_mode: false,
        };
        let rendered = exec(&plan).expect_err("nonexistent program cannot spawn");

        assert_eq!(rendered.exit, EXIT_FAILURE);
        assert!(rendered.to_stderr);
        assert_eq!(rendered.json["error"], "spawn-failed");
        // The human message is framed: what (keel ▸ …), why, and next.
        assert!(rendered.human.starts_with("keel \u{25b8} "));
        assert!(rendered.human.contains("keel-nonexistent-program-9f3a"));
        assert!(rendered.human.contains("why:"));
        assert!(rendered.human.contains("next:"));
    }

    /// #62/finding 4: a command-mode plan's spawn failure is a missing/
    /// non-executable command on PATH — not the Python/Node dispatch failure
    /// `exec_with`'s hint used to assume unconditionally, which told a user
    /// to `pip install keelrun` for what is really a typo'd binary name.
    #[test]
    fn spawn_failure_hint_is_command_accurate_in_command_mode() {
        let plan = RunPlan {
            program: "keel-nonexistent-program-9f3a".to_owned(),
            argv: vec![],
            disable: false,
            command_mode: true,
        };
        let rendered = exec(&plan).expect_err("nonexistent program cannot spawn");

        assert_eq!(rendered.json["error"], "spawn-failed");
        assert!(rendered.human.contains("next:"));
        assert!(
            !rendered.human.contains("pip install keelrun"),
            "command mode must not suggest the Python dispatch fix: {}",
            rendered.human
        );
        assert!(
            !rendered.human.contains("npm i -D keelrun"),
            "command mode must not suggest the Node dispatch fix: {}",
            rendered.human
        );
        assert!(
            rendered.human.contains("PATH"),
            "command mode's hint should point at PATH/executability: {}",
            rendered.human
        );
    }

    /// #62/finding 3: the command-mode banner claims the child gets
    /// `KEEL_ENABLE=1` — true only when [`activation_env`] actually exports
    /// it, which `--disable` suppresses entirely (and sets `KEEL_DISABLE=1`
    /// instead). The banner must not make that claim under `--disable`.
    #[test]
    fn command_mode_banner_names_the_activation_mechanism_when_enabled() {
        let plan = RunPlan {
            program: "sh".to_owned(),
            argv: vec![],
            disable: false,
            command_mode: true,
        };
        let banner = command_mode_banner("mytool", &plan).expect("banner shown when enabled");
        assert!(banner.contains("KEEL_ENABLE=1"));
        assert!(banner.contains("mytool"));
    }

    #[test]
    fn command_mode_banner_is_suppressed_under_disable() {
        let plan = RunPlan {
            program: "sh".to_owned(),
            argv: vec![],
            disable: true,
            command_mode: true,
        };
        assert_eq!(
            command_mode_banner("mytool", &plan),
            None,
            "must not claim KEEL_ENABLE=1 under --disable, where activation_env exports nothing"
        );
    }
}
