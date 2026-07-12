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

use super::{LangFindings, Sighting};

/// The embedded `ast` walker. Deterministic: directories and files are visited
/// in sorted order, output keys are sorted. Finds imports of the known effect
/// libraries and URL/DSN string literals, each with `file:line`.
const AST_WALKER: &str = r#"
import ast, json, os, sys
from urllib.parse import urlsplit

HTTP_LIBS = {"httpx", "requests", "aiohttp", "urllib3"}
LLM_LIBS = {"openai", "anthropic"}
OTHER_LIBS = {"psycopg", "boto3"}
KNOWN = HTTP_LIBS | LLM_LIBS | OTHER_LIBS
SKIP = {".keel", ".git", "__pycache__", "node_modules", ".venv", "venv",
        ".mypy_cache", ".pytest_cache", "dist", "build", "target"}


def top(mod):
    return mod.split(".", 1)[0] if mod else ""


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


root = sys.argv[1] if len(sys.argv) > 1 else "."
imports = []
urls = []
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
            if isinstance(node, ast.Import):
                for alias in node.names:
                    t = top(alias.name)
                    if t in KNOWN:
                        imports.append({"lib": t, "file": rel, "line": node.lineno})
            elif isinstance(node, ast.ImportFrom):
                t = top(node.module or "")
                if t in KNOWN:
                    imports.append({"lib": t, "file": rel, "line": node.lineno})
            elif isinstance(node, ast.Constant) and isinstance(node.value, str):
                h = host(node.value)
                if h:
                    urls.append({"host": h, "file": rel, "line": node.lineno})

print(json.dumps({"files_scanned": files, "imports": imports, "urls": urls},
                 sort_keys=True))
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

/// The walker's JSON output, typed.
#[derive(Debug, Deserialize)]
struct WalkerOutput {
    files_scanned: usize,
    imports: Vec<Import>,
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
}

const HTTP_LIBS: &[&str] = &["httpx", "requests", "aiohttp", "urllib3"];

/// Run the walker over `project`. A missing `python3`, or a walker that fails,
/// yields an empty unavailable result — never a panic.
pub fn scan(project: &Path) -> PyScan {
    let Some(output) = run_walker(project) else {
        return PyScan::default();
    };
    let mut findings = LangFindings::default();
    for imp in &output.imports {
        findings.libs.insert(imp.lib.clone());
        let sighting = Sighting {
            file: imp.file.clone(),
            line: imp.line,
        };
        match imp.lib.as_str() {
            "openai" => findings.llm.push(("openai".to_owned(), sighting)),
            "anthropic" => findings.llm.push(("anthropic".to_owned(), sighting)),
            lib if HTTP_LIBS.contains(&lib) => findings.http_in_use = true,
            // psycopg/boto3: recorded as effect libraries via their DSN/URL
            // literals (if any); no synthetic host target from the import alone.
            _ => {}
        }
    }
    // A DSN literal (postgres://…) is itself evidence of an outbound call even
    // without one of the HTTP libraries imported.
    if !output.urls.is_empty() {
        findings.http_in_use = true;
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
    PyScan {
        available: true,
        files_scanned: output.files_scanned,
        findings,
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
