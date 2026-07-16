//! The Python static scan: an `ast`-walker executed out-of-process via
//! `python3 -`.
//!
//! Parsing Python with Python's own `ast` is exact where a regex would guess:
//! it sees real imports and string-literal constants with true line numbers.
//! The walker script is embedded and fed on stdin; it prints one JSON object.
//! If `python3` is absent the pass yields nothing and reports
//! [`available`](PyScan::available)`= false` so the caller can say so out loud
//! rather than silently under-reporting coverage.

use std::io::Write;
use std::path::Path;
use std::process::{Command, Stdio};

use serde::Deserialize;

use super::{FunctionFacts, LangFindings, Sighting};

/// The embedded `ast` walker. Deterministic: directories and files are visited
/// in sorted order, output keys are sorted. Finds imports of the known effect
/// libraries and URL/DSN string literals, each with `file:line` — and, for
/// `keel flows suggest`, attributes effect / time / random / replay-unsafe
/// calls to their enclosing **module-level** function defs (real AST
/// containment: nested defs and lambdas inside a function count toward it;
/// class methods are not flow entrypoints and are not attributed).
const AST_WALKER: &str = r#"
import ast, json, os, sys
from urllib.parse import urlsplit

HTTP_LIBS = {"httpx", "requests", "aiohttp", "urllib3"}
LLM_LIBS = {"openai", "anthropic", "google.genai"}
OTHER_LIBS = {"psycopg", "boto3"}
# The agent-framework packs (dx-spec agent-first-class work). pydantic_ai,
# crewai, langgraph, and openai-agents (import name `agents`) are plain
# top-level module names, classified exactly like HTTP_LIBS/LLM_LIBS/
# OTHER_LIBS above. `mcp` is the SDK's own import name (mcp_pack) — a normal
# adapter lib here, not special-cased. `google.adk` is handled separately via
# dotted-prefix matching (see `import_entries`): bare `google` is a namespace
# package shared with unrelated distributions (google-protobuf and friends)
# and must record nothing.
AGENT_LIBS = {"pydantic_ai", "crewai", "langgraph", "agents", "mcp"}
KNOWN = HTTP_LIBS | LLM_LIBS | OTHER_LIBS | AGENT_LIBS | {"google.adk"}
# Known Python resilience libraries — a `keel doctor` signal that a target may
# already have its own retry/backoff, separate from KNOWN (which is about
# libraries Keel *adapts*; these are libraries Keel never adapts, so mixing
# them into KNOWN would misclassify them as an "invisible" coverage gap).
RESILIENCE_LIBS = {"tenacity", "backoff", "retrying", "stamina"}
TIME_LIBS = {"time", "datetime"}
RANDOM_LIBS = {"random", "uuid", "secrets"}
UNSAFE_LIBS = {"threading", "multiprocessing", "subprocess", "socket"}
TRACKED = KNOWN | TIME_LIBS | RANDOM_LIBS | UNSAFE_LIBS | {"os"}
TIME_NAMES = {"time", "time_ns", "monotonic", "monotonic_ns",
              "perf_counter", "perf_counter_ns", "gmtime", "localtime"}
DT_NAMES = {"now", "utcnow", "today"}
UUID_NAMES = {"uuid1", "uuid3", "uuid4", "uuid5"}
OS_UNSAFE = {"system", "popen", "fork", "forkpty", "execv", "execve",
             "execvp", "execvpe", "spawnl", "spawnv", "spawnvp"}
SKIP = {".keel", ".git", "__pycache__", "node_modules", ".venv", "venv",
        ".mypy_cache", ".pytest_cache", "dist", "build", "target"}
# The two `google` submodules Keel adapts. Bare `import google` (the
# namespace package) records nothing — it also hosts unrelated distributions
# (google-protobuf and friends) that are none of Keel's business.
GOOGLE_SUBMODULES = {"adk", "genai"}


def top(mod):
    return mod.split(".", 1)[0] if mod else ""


def import_entries(node):
    """Yield (module_key, bound_name) for each name an Import/ImportFrom node
    binds. Handles the `google.adk` / `google.genai` dotted-prefix special
    case: `import google.adk`, `from google.adk import ...`, and
    `from google import genai, adk` all resolve to `google.<name>`; a bare
    `google` import (or `from google import <something else>`) yields
    nothing — the google namespace package alone is not evidence of either
    library."""
    if isinstance(node, ast.Import):
        for alias in node.names:
            parts = alias.name.split(".")
            if len(parts) >= 2 and parts[0] == "google" and parts[1] in GOOGLE_SUBMODULES:
                key = "google." + parts[1]
            else:
                key = parts[0]
            yield key, (alias.asname or alias.name).split(".", 1)[0]
    elif isinstance(node, ast.ImportFrom):
        mod = node.module or ""
        mparts = mod.split(".")
        if len(mparts) >= 2 and mparts[0] == "google" and mparts[1] in GOOGLE_SUBMODULES:
            key = "google." + mparts[1]
            for alias in node.names:
                yield key, alias.asname or alias.name
        elif mod == "google":
            for alias in node.names:
                if alias.name in GOOGLE_SUBMODULES:
                    yield "google." + alias.name, alias.asname or alias.name
        else:
            key = top(mod)
            for alias in node.names:
                yield key, alias.asname or alias.name


def host(s):
    if "://" not in s:
        return None
    try:
        parts = urlsplit(s.strip())
    except ValueError:
        return None
    if not parts.scheme or not parts.hostname:
        return None
    return parts.hostname


def call_root(f):
    """The Name at the base of a call's attribute chain, or None. Deliberately
    does NOT see through intermediate calls: in `httpx.get(u).json()` only the
    inner `httpx.get(u)` has a root, so a chained method on a call result is
    never double-counted as a second effect."""
    while isinstance(f, ast.Attribute):
        f = f.value
    return f.id if isinstance(f, ast.Name) else None


# Call names that construct a handle rather than perform an effect: CapWords
# constructors (OpenAI(), Client()) plus the well-known factory methods.
FACTORY_NAMES = {"client", "resource", "session", "connect"}


def is_constructor(name):
    return name is None or name[:1].isupper() or name in FACTORY_NAMES


def aliases_of(tree):
    """Binding name -> tracked module (top-level, or `google.adk`/
    `google.genai` — see `import_entries`). Also follows one hop of
    constructor assignment (client = OpenAI() -> client is an openai handle),
    the dominant SDK-client pattern."""
    a = {}
    for node in ast.walk(tree):
        if isinstance(node, (ast.Import, ast.ImportFrom)):
            for key, bound in import_entries(node):
                if key in TRACKED:
                    a[bound] = key
    for node in ast.walk(tree):
        if isinstance(node, ast.Assign) and isinstance(node.value, ast.Call):
            lib = a.get(call_root(node.value.func))
            if lib in KNOWN:
                for tgt in node.targets:
                    if isinstance(tgt, ast.Name):
                        a[tgt.id] = lib
    return a


def url_consts_of(tree):
    """Module-level NAME = "scheme://host/..." constants -> host, so a URL
    hoisted to a constant still attributes to the functions that use it."""
    consts = {}
    for node in tree.body:
        if (isinstance(node, ast.Assign) and isinstance(node.value, ast.Constant)
                and isinstance(node.value.value, str)):
            h = host(node.value.value)
            if h:
                for tgt in node.targets:
                    if isinstance(tgt, ast.Name):
                        consts[tgt.id] = h
    return consts


def fn_facts(fn, rel, mod, aliases, url_consts):
    effects = unsafe_idem = t_reads = r_reads = 0
    targets = set()
    reasons = []
    for node in ast.walk(fn):
        if isinstance(node, ast.Name) and node.id in url_consts:
            targets.add(url_consts[node.id])
        elif isinstance(node, ast.Constant) and isinstance(node.value, str):
            h = host(node.value)
            if h:
                targets.add(h)
        if not isinstance(node, ast.Call):
            continue
        lib = aliases.get(call_root(node.func))
        if lib is None:
            continue
        attr = node.func.attr if isinstance(node.func, ast.Attribute) else None
        name = attr if attr is not None else call_root(node.func)
        if lib in KNOWN:
            if is_constructor(name):
                continue  # a handle being built, not an effect performed
            effects += 1
            if name in {"post", "patch"}:
                unsafe_idem += 1
            if lib in LLM_LIBS:
                targets.add("llm:" + lib)
        elif lib == "time" and name in TIME_NAMES:
            t_reads += 1
        elif lib == "datetime" and name in DT_NAMES:
            t_reads += 1
        elif lib in {"random", "secrets"}:
            r_reads += 1
        elif lib == "uuid" and name in UUID_NAMES:
            r_reads += 1
        elif lib == "os" and name == "urandom":
            r_reads += 1
        elif lib in UNSAFE_LIBS:
            reasons.append((node.lineno, "%s use at %s:%d" % (lib, rel, node.lineno)))
        elif lib == "os" and name in OS_UNSAFE:
            reasons.append((node.lineno, "os.%s at %s:%d" % (name, rel, node.lineno)))
    return {"effects": effects, "file": rel, "idempotent_unsafe": unsafe_idem,
            "line": fn.lineno, "module": mod, "name": fn.name,
            "random_reads": r_reads, "targets": sorted(targets),
            "time_reads": t_reads,
            "unsafe_reasons": [t for _, t in sorted(reasons)]}


root = sys.argv[1] if len(sys.argv) > 1 else "."
imports = []
urls = []
functions = []
resilience_libs = set()
files = 0
for dirpath, dirnames, filenames in os.walk(root):
    dirnames[:] = sorted(d for d in dirnames if d not in SKIP and not d.startswith("."))
    for fn in sorted(filenames):
        if not fn.endswith(".py"):
            continue
        path = os.path.join(dirpath, fn)
        rel = os.path.relpath(path, root).replace(os.sep, "/")
        try:
            with open(path, "r", encoding="utf-8") as fh:
                tree = ast.parse(fh.read())
        except (OSError, SyntaxError, UnicodeDecodeError, ValueError):
            continue
        files += 1
        for node in ast.walk(tree):
            if isinstance(node, (ast.Import, ast.ImportFrom)):
                for key, _bound in import_entries(node):
                    if key in KNOWN:
                        imports.append({"lib": key, "file": rel, "line": node.lineno})
                    elif key in RESILIENCE_LIBS:
                        resilience_libs.add(key)
            elif isinstance(node, ast.Constant) and isinstance(node.value, str):
                h = host(node.value)
                if h:
                    urls.append({"host": h, "file": rel, "line": node.lineno})
        mod = rel[:-3].replace("/", ".")
        if mod.endswith(".__init__"):
            mod = mod[: -len(".__init__")]
        aliases = aliases_of(tree)
        consts = url_consts_of(tree)
        for node in tree.body:
            if isinstance(node, (ast.FunctionDef, ast.AsyncFunctionDef)):
                functions.append(fn_facts(node, rel, mod, aliases, consts))

print(json.dumps({"files_scanned": files, "functions": functions,
                  "imports": imports, "resilience_libs": sorted(resilience_libs),
                  "urls": urls}, sort_keys=True))
"#;

/// One import finding from the walker.
#[derive(Debug, Deserialize)]
struct Import {
    lib: String,
    file: String,
    line: u32,
}

/// One URL-literal finding from the walker.
#[derive(Debug, Deserialize)]
struct Url {
    host: String,
    file: String,
    line: u32,
}

/// One module-level function's facts from the walker.
#[derive(Debug, Deserialize)]
struct PyFunction {
    effects: u32,
    file: String,
    idempotent_unsafe: u32,
    line: u32,
    module: String,
    name: String,
    random_reads: u32,
    targets: Vec<String>,
    time_reads: u32,
    unsafe_reasons: Vec<String>,
}

/// The walker's JSON output, typed.
#[derive(Debug, Deserialize)]
struct WalkerOutput {
    files_scanned: usize,
    #[serde(default)]
    functions: Vec<PyFunction>,
    imports: Vec<Import>,
    #[serde(default)]
    resilience_libs: Vec<String>,
    urls: Vec<Url>,
}

/// The Python pass result.
#[derive(Debug, Clone, Default)]
pub struct PyScan {
    /// Whether `python3` ran the walker.
    pub available: bool,
    /// Files the walker parsed.
    pub files_scanned: usize,
    /// Findings, ready to merge.
    pub findings: LangFindings,
    /// Per-function attribution (module-level defs), for `keel flows suggest`.
    pub functions: Vec<FunctionFacts>,
}

const HTTP_LIBS: &[&str] = &["httpx", "requests", "aiohttp", "urllib3"];

/// Normalize a walker-reported import key to the name `keel doctor`'s
/// `REGISTRY` (see `crate::doctor`) keys its adapters by. The walker emits
/// raw Python identifiers (`google.adk`, `pydantic_ai`, `agents`); doctor's
/// registry — and the docs/findings that reference it — use the packs'
/// PyPI-ish public names (`google-adk`, `pydantic-ai`, `openai-agents`).
/// Everything else passes through unchanged (`crewai`, `langgraph`, `mcp`,
/// `httpx`, `openai`, …).
fn normalize_lib(lib: &str) -> String {
    match lib {
        "google.adk" => "google-adk".to_owned(),
        "google.genai" => "google-genai".to_owned(),
        "pydantic_ai" => "pydantic-ai".to_owned(),
        "agents" => "openai-agents".to_owned(),
        other => other.to_owned(),
    }
}

/// Run the walker over `project`. A missing `python3`, or a walker that fails,
/// yields an empty unavailable result — never a panic.
pub fn scan(project: &Path) -> PyScan {
    let Some(output) = run_walker(project) else {
        return PyScan::default();
    };
    let mut findings = LangFindings::default();
    for imp in &output.imports {
        let lib = normalize_lib(&imp.lib);
        findings.libs.insert(lib.clone());
        let sighting = Sighting {
            file: imp.file.clone(),
            line: imp.line,
        };
        match lib.as_str() {
            "openai" => findings.llm.push(("openai".to_owned(), sighting)),
            "anthropic" => findings.llm.push(("anthropic".to_owned(), sighting)),
            "google-genai" => findings.llm.push(("google-genai".to_owned(), sighting)),
            lib if HTTP_LIBS.contains(&lib) => findings.http_in_use = true,
            // psycopg/boto3/agent-pack libs: recorded as effect libraries via
            // their DSN/URL literals (if any) or the doctor adapter registry;
            // no synthetic host target from the import alone.
            _ => {}
        }
    }
    // A DSN literal (postgres://…) is itself evidence of an outbound call even
    // without one of the HTTP libraries imported.
    if !output.urls.is_empty() {
        findings.http_in_use = true;
    }
    for lib in &output.resilience_libs {
        findings.resilience_libs.insert(lib.clone());
    }
    for url in &output.urls {
        // The walker already returned a bare hostname (urlsplit.hostname), so it
        // is lowercased and port-stripped; normalize defensively.
        findings.hosts.push((
            url.host.to_ascii_lowercase(),
            Sighting {
                file: url.file.clone(),
                line: url.line,
            },
        ));
    }
    let functions = output
        .functions
        .into_iter()
        .map(|f| FunctionFacts {
            entrypoint: format!("py:{}:{}", f.module, f.name),
            file: f.file,
            line: f.line,
            effects: f.effects,
            idempotent_unsafe: f.idempotent_unsafe,
            time_reads: f.time_reads,
            random_reads: f.random_reads,
            unsafe_reasons: f.unsafe_reasons,
            targets: f.targets.into_iter().collect(),
        })
        .collect();
    PyScan {
        available: true,
        files_scanned: output.files_scanned,
        findings,
        functions,
    }
}

/// Spawn `python3 - <root>`, feed the walker on stdin, parse its stdout.
fn run_walker(project: &Path) -> Option<WalkerOutput> {
    let mut child = Command::new("python3")
        .arg("-")
        .arg(project)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .ok()?;
    child.stdin.take()?.write_all(AST_WALKER.as_bytes()).ok()?;
    let out = child.wait_with_output().ok()?;
    if !out.status.success() {
        return None;
    }
    serde_json::from_slice(&out.stdout).ok()
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use tempfile::TempDir;

    fn python3_present() -> bool {
        Command::new("python3")
            .arg("--version")
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status()
            .is_ok_and(|s| s.success())
    }

    #[test]
    fn walks_imports_and_url_literals() {
        if !python3_present() {
            eprintln!("skip: python3 not available");
            return;
        }
        let dir = TempDir::new().unwrap();
        fs::write(
            dir.path().join("app.py"),
            "import httpx\nfrom openai import OpenAI\n\nURL = \"https://api.example.com/v1\"\n",
        )
        .unwrap();
        let scan = scan(dir.path());
        assert!(scan.available);
        assert_eq!(scan.files_scanned, 1);
        assert!(scan.findings.http_in_use);
        assert!(
            scan.findings
                .llm
                .iter()
                .any(|(p, s)| p == "openai" && s.file == "app.py" && s.line == 2)
        );
        assert!(
            scan.findings
                .hosts
                .iter()
                .any(|(h, s)| h == "api.example.com" && s.line == 4)
        );
    }

    #[test]
    fn agent_pack_imports_and_the_google_dotted_prefix_are_classified() {
        if !python3_present() {
            eprintln!("skip: python3 not available");
            return;
        }
        let dir = TempDir::new().unwrap();
        fs::write(
            dir.path().join("app.py"),
            "import google\n\
             import google.protobuf\n\
             from google import protobuf\n\
             import google.adk\n\
             from google.genai import types\n\
             from google import genai, adk\n\
             from pydantic_ai import Agent\n\
             import crewai\n\
             from langgraph.graph import StateGraph\n\
             from agents import Agent as OpenAIAgent\n\
             import mcp\n\
             from tenacity import retry\n",
        )
        .unwrap();
        let scan = scan(dir.path());
        assert!(scan.available);
        // The six agent-framework packs + the two google submodules, all
        // normalized to doctor's REGISTRY names.
        for lib in [
            "google-adk",
            "google-genai",
            "pydantic-ai",
            "crewai",
            "langgraph",
            "openai-agents",
            "mcp",
        ] {
            assert!(scan.findings.libs.contains(lib), "missing lib: {lib}");
        }
        // google.genai joins the llm findings, alongside openai/anthropic.
        assert!(scan.findings.llm.iter().any(|(p, _)| p == "google-genai"));
        // tenacity is resilience, not a coverage-relevant lib.
        assert_eq!(
            scan.findings.resilience_libs,
            ["tenacity".to_owned()].into_iter().collect()
        );
        // Bare `google` (the namespace package) must record NOTHING — it
        // also hosts unrelated distributions (google-protobuf and friends).
        // Neither must `import google.protobuf` nor `from google import
        // protobuf`: `protobuf` isn't one of Keel's two adapted submodules
        // (`GOOGLE_SUBMODULES = {"adk", "genai"}`), so both forms must fall
        // through to the same "namespace package, not evidence" handling —
        // never surfacing as `google`, `google.protobuf`, or `protobuf`.
        for leaked in ["google", "google.protobuf", "protobuf"] {
            assert!(
                !scan.findings.libs.contains(leaked),
                "google.protobuf import must not record {leaked}"
            );
        }
    }

    #[test]
    fn resilience_lib_imports_are_detected_separately_from_known_libs() {
        if !python3_present() {
            eprintln!("skip: python3 not available");
            return;
        }
        let dir = TempDir::new().unwrap();
        fs::write(
            dir.path().join("app.py"),
            "import httpx\nfrom tenacity import retry\nimport backoff\n",
        )
        .unwrap();
        let scan = scan(dir.path());
        assert_eq!(
            scan.findings.resilience_libs,
            ["backoff".to_owned(), "tenacity".to_owned()]
                .into_iter()
                .collect()
        );
        // Resilience libs never pollute `libs` (which feeds doctor's
        // "invisible/unadapted effect library" coverage classification —
        // tenacity/backoff were never adapter candidates).
        assert!(!scan.findings.libs.contains("tenacity"));
        assert!(!scan.findings.libs.contains("backoff"));
        assert!(scan.findings.libs.contains("httpx"));
    }

    #[test]
    fn attributes_effects_time_random_to_module_level_functions() {
        if !python3_present() {
            eprintln!("skip: python3 not available");
            return;
        }
        let dir = TempDir::new().unwrap();
        fs::write(
            dir.path().join("pipeline.py"),
            r#"import time
import random
import httpx
from openai import OpenAI

API = "https://api.example.com/v1/data"
client = OpenAI()


def main():
    started = time.time()
    seed = random.random()
    data = httpx.get(API).json()
    httpx.post(API, json=data)
    client.responses.create(model="gpt-4.1", input="hi")
    return started, seed


def helper():
    return 41 + 1
"#,
        )
        .unwrap();
        let s = scan(dir.path());
        let main = s
            .functions
            .iter()
            .find(|f| f.entrypoint == "py:pipeline:main")
            .expect("main attributed");
        // get + post + create — the chained .json() must NOT double-count.
        assert_eq!(main.effects, 3);
        assert_eq!(main.idempotent_unsafe, 1, "only the POST");
        assert_eq!(main.time_reads, 1);
        assert_eq!(main.random_reads, 1);
        assert!(main.unsafe_reasons.is_empty());
        assert!(main.targets.contains("api.example.com"), "URL via constant");
        assert!(main.targets.contains("llm:openai"), "client = OpenAI() hop");
        assert_eq!((main.file.as_str(), main.line), ("pipeline.py", 10));
        let helper = s
            .functions
            .iter()
            .find(|f| f.entrypoint == "py:pipeline:helper")
            .expect("helper attributed");
        assert_eq!(helper.effects, 0);
    }

    #[test]
    fn threads_and_subprocess_defeat_the_replay_safe_estimate() {
        if !python3_present() {
            eprintln!("skip: python3 not available");
            return;
        }
        let dir = TempDir::new().unwrap();
        fs::write(
            dir.path().join("jobs.py"),
            r#"import subprocess
import threading
import requests


def risky():
    requests.post("https://api.example.com/v1/x")
    threading.Thread(target=print).start()
    subprocess.run(["ls"])
"#,
        )
        .unwrap();
        let s = scan(dir.path());
        let f = s
            .functions
            .iter()
            .find(|f| f.entrypoint == "py:jobs:risky")
            .expect("risky attributed");
        assert_eq!(f.effects, 1);
        assert_eq!(
            f.unsafe_reasons,
            vec![
                "threading use at jobs.py:8".to_owned(),
                "subprocess use at jobs.py:9".to_owned(),
            ]
        );
    }

    #[test]
    fn syntax_error_file_is_skipped_not_fatal() {
        if !python3_present() {
            eprintln!("skip: python3 not available");
            return;
        }
        let dir = TempDir::new().unwrap();
        fs::write(dir.path().join("broken.py"), "def (:\n").unwrap();
        fs::write(dir.path().join("ok.py"), "import requests\n").unwrap();
        let scan = scan(dir.path());
        assert_eq!(scan.files_scanned, 1, "only the parseable file counts");
        assert!(scan.findings.http_in_use);
    }
}
