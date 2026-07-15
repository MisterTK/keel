//! `keel run <script> [args…]` — dispatch a program into its language front end.
//!
//! The heavy lifting (bootstrap, import hook, adapters, discovery) lives in the
//! Python and Node packages; `run` is only the dispatcher (dx-spec §1, Level 0):
//!
//! - `*.py`                     → `python3 -m keel run <script> [args…]`
//! - `*.{mjs,js,ts,cjs,…}`      → `node --import keel/hook <script> [args…]`
//! - a `package.json`, or a dir containing one → resolve its `main`, then
//!   the Node path
//! - any other dir              → a conventional entry name (`main.py`,
//!   `__main__.py`, `index.*`), then — if exactly one Python or Node source
//!   file sits directly inside it — that file; ambiguous or empty is a
//!   precise error, never a guess
//! - anything else              → a precise what/why/next error, exit 2
//!
//! The child inherits the environment (so every `KEEL_*` var passes through);
//! `--disable` layers `KEEL_DISABLE=1` on top. The child's exit code is the
//! process's exit code — wrapping is invisible on the success path.

use std::path::{Path, PathBuf};
use std::process::Command;

use serde::Serialize;

use crate::{EXIT_FAILURE, EXIT_USAGE, Rendered};

/// Node's resolver name for the preload hook (the `keel` package's `./hook`
/// export). Resolved from the project's `node_modules`, exactly as
/// `node --import keel/hook` would in a project that installed `keel`.
const NODE_HOOK: &str = "keel/hook";

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
}

impl RunError {
    pub(crate) fn render(&self) -> Rendered {
        let (what, why, next, kind) = match self {
            Self::NotFound { target } => (
                format!("Cannot run `{target}`: no such file or directory."),
                "The path does not exist relative to the current directory.".to_owned(),
                "Check the path; `keel run` takes a script file, a package.json, or a project directory.".to_owned(),
                "not-found",
            ),
            Self::UnknownKind { target } => (
                format!("Cannot run `{target}`: unrecognized program type."),
                "`keel run` dispatches Python (.py) and Node (.mjs/.js/.ts/.cjs/.mts/.cts/.jsx/.tsx, or a package.json main); this target is neither.".to_owned(),
                "Rename to a supported extension, point at the project's package.json, or invoke the interpreter directly.".to_owned(),
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
        return Err(RunError::NotFound {
            target: target.to_owned(),
        });
    }

    match path.extension().and_then(|e| e.to_str()) {
        Some("py") => Ok(python_plan(target, args, disable)),
        Some(ext) if NODE_EXTS.contains(&ext) => Ok(node_plan(target, args, disable)),
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

fn node_plan(target: &str, extra: &[String], disable: bool) -> RunPlan {
    let mut argv = vec![
        "--import".to_owned(),
        NODE_HOOK.to_owned(),
        as_script_operand(target),
    ];
    argv.extend_from_slice(extra);
    RunPlan {
        program: "node".to_owned(),
        argv,
        disable,
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
    Ok(node_plan(&entry.to_string_lossy(), args, disable))
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
            return Ok(node_plan(&candidate.to_string_lossy(), args, disable));
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
                Ok(node_plan(&only.to_string_lossy(), args, disable))
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
            let next = if plan.program == "python3" {
                "Install Python 3 and the `keel` package (`pip install keel`)."
            } else {
                "Install Node.js and the `keel` package (`npm i -D keel`)."
            };
            let human = format!("keel \u{25b8} {what}\n  why:  {why}\n  next: {next}");
            let report = RunErrorReport {
                error: "spawn-failed",
                next,
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

/// The whole `keel run` command: plan, then exec. On a dispatch error render it;
/// on success return the child's exit code.
pub fn run(target: &str, args: &[String], disable: bool) -> (Option<Rendered>, i32) {
    match plan(target, args, disable) {
        Err(e) => {
            let r = e.render();
            let code = r.exit;
            (Some(r), code)
        }
        Ok(plan) => match exec(&plan) {
            Ok(code) => (None, code),
            Err(r) => {
                let code = r.exit;
                (Some(r), code)
            }
        },
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use tempfile::TempDir;

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
        let plan = plan(&script.to_string_lossy(), &[], true).unwrap();
        assert_eq!(plan.program, "node");
        assert_eq!(plan.argv[0], "--import");
        assert_eq!(plan.argv[1], "keel/hook");
        assert!(plan.disable);
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
        let plan = plan(&dir.path().to_string_lossy(), &[], false).unwrap();
        assert_eq!(plan.program, "node");
        assert!(plan.argv[2].ends_with("start.mjs"));
    }

    #[test]
    fn package_json_defaults_to_index_js() {
        let dir = TempDir::new().unwrap();
        fs::write(dir.path().join("package.json"), "{}").unwrap();
        fs::write(dir.path().join("index.js"), "// entry\n").unwrap();
        let plan = plan(&dir.path().to_string_lossy(), &[], false).unwrap();
        assert!(plan.argv[2].ends_with("index.js"));
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
        };
        let code = exec_with(&plan, |cmd| {
            cmd.env("KEEL_RECORD_TEST", "marker");
        })
        .expect("sh should spawn");
        assert_eq!(code, 0);
    }

    #[test]
    fn spawn_failure_is_a_framed_error_with_exit_1() {
        // A program that cannot be spawned surfaces a what/why/next error on
        // stderr and exit 1 (an underlying failure, not a usage error).
        let plan = RunPlan {
            program: "keel-nonexistent-program-9f3a".to_owned(),
            argv: vec![],
            disable: false,
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
}
