//! The JS/TS static scan — a **documented simplification**.
//!
//! Unlike the Python pass (which uses a real parser), this is a line-oriented
//! textual scan: no JS AST, no dependency on a Node toolchain at `keel init`
//! time. It looks for the seams `keel init` needs and nothing more:
//!
//! - HTTP in use: a `fetch(` call, or an import/require of `undici` or
//!   `node:http`/`node:https` (`http`/`https`).
//! - provider SDKs: an import/require of `openai` or `@anthropic-ai/sdk`
//!   (also bare `anthropic`) → `llm:*` targets.
//! - URL literals: `http(s)://host…` inside a string → host targets.
//!
//! The tradeoff (accepted here): a URL built by concatenation, or an exotic
//! import form, is missed — exactly the ~20% the import-time and observed-run
//! evidence sources exist to catch (dx-spec §2). Line numbers are exact.
//!
//! ## Function attribution (for `keel flows suggest`) — what this pass CAN
//! and CANNOT do
//!
//! Being line-oriented, the attribution is a brace-depth heuristic, not a
//! parse. It **can** attribute to *top-level named functions*: declarations
//! (`function f(…) {`, `export async function f(…) {`) and initializers
//! (`const f = async (…) => {`, `const f = function (…) {`), including
//! single-line arrow bodies. Everything found between a tracked declaration
//! and the brace that closes it counts toward that function — so effects
//! inside nested callbacks, inner arrows, and helper closures attribute to
//! the enclosing top-level function (usually what you want for a flow
//! verdict, but it is containment-by-lines, not by scope).
//!
//! It **cannot** attribute: class methods, unnamed `export default` values,
//! object-literal methods, functions whose opening `{` sits on its own line
//! (Allman style), or code after a template-literal `${…}` that desyncs the
//! per-line quote tracking. Those findings still appear in the file-level
//! scan; they are just not credited to a function. The Python pass has none
//! of these limits (real AST); `keel flows suggest` says so in its notes.

use std::path::Path;

use super::{FunctionFacts, LangFindings, SKIP_DIRS, Sighting, host_from_url};

/// Extensions the scan reads.
const JS_EXTS: &[&str] = &["js", "mjs", "cjs", "ts", "mts", "cts", "jsx", "tsx"];

/// The JS pass result.
#[derive(Debug, Clone, Default)]
pub struct JsScan {
    /// Files read.
    pub files_scanned: usize,
    /// Findings, ready to merge.
    pub findings: LangFindings,
    /// Per-function attribution (top-level named functions only — see the
    /// module docs for the heuristic's honest limits).
    pub functions: Vec<FunctionFacts>,
}

/// Scan `project` for JS/TS effect seams.
pub fn scan(project: &Path) -> JsScan {
    let mut files = Vec::new();
    collect(project, &mut files);
    files.sort();

    let mut result = JsScan::default();
    for path in &files {
        let Ok(src) = std::fs::read_to_string(path) else {
            continue;
        };
        result.files_scanned += 1;
        let rel = relative(project, path);
        scan_source(&src, &rel, &mut result.findings);
        result.functions.extend(scan_functions(&src, &rel));
    }
    result
}

/// Scan one file's text. Split out so it is unit-testable without touching the
/// filesystem.
fn scan_source(src: &str, rel: &str, findings: &mut LangFindings) {
    for (idx, line) in src.lines().enumerate() {
        let lineno = u32::try_from(idx + 1).unwrap_or(u32::MAX);
        if line_uses_http(line) {
            findings.http_in_use = true;
            record_http_libs(line, findings);
        }
        if let Some(provider) = provider_import(line) {
            findings.libs.insert(provider.to_owned());
            findings.llm.push((
                provider.to_owned(),
                Sighting {
                    file: rel.to_owned(),
                    line: lineno,
                },
            ));
        }
        for host in hosts_in_line(line) {
            findings.hosts.push((
                host,
                Sighting {
                    file: rel.to_owned(),
                    line: lineno,
                },
            ));
        }
    }
}

/// Does this line evidence an HTTP client? (`fetch(`, or an `undici`/`node:http`
/// import/require.)
fn line_uses_http(line: &str) -> bool {
    if contains_call(line, "fetch") {
        return true;
    }
    [
        "undici",
        "node:http",
        "node:https",
        "\"http\"",
        "'http'",
        "\"https\"",
        "'https'",
    ]
    .iter()
    .any(|needle| line.contains(needle) && (line.contains("import") || line.contains("require")))
}

/// Record which HTTP library names a line references, for `keel doctor`.
fn record_http_libs(line: &str, findings: &mut LangFindings) {
    if contains_call(line, "fetch") {
        findings.libs.insert("fetch".to_owned());
    }
    if line.contains("undici") && (line.contains("import") || line.contains("require")) {
        findings.libs.insert("undici".to_owned());
    }
    let http_specifiers = [
        "node:http",
        "node:https",
        "\"http\"",
        "'http'",
        "\"https\"",
        "'https'",
    ];
    if http_specifiers.iter().any(|n| line.contains(n))
        && (line.contains("import") || line.contains("require"))
    {
        findings.libs.insert("http".to_owned());
    }
}

/// If this line imports/requires a known provider SDK, return the provider key.
fn provider_import(line: &str) -> Option<&'static str> {
    if !(line.contains("import") || line.contains("require")) {
        return None;
    }
    // Match the module specifier inside quotes to avoid matching a variable
    // named `openai`.
    let openai = ["\"openai\"", "'openai'"];
    let anthropic = [
        "\"@anthropic-ai/sdk\"",
        "'@anthropic-ai/sdk'",
        "\"anthropic\"",
        "'anthropic'",
    ];
    if openai.iter().any(|q| line.contains(q)) {
        Some("openai")
    } else if anthropic.iter().any(|q| line.contains(q)) {
        Some("anthropic")
    } else {
        None
    }
}

/// Every `scheme://host` host named in a string literal on this line.
fn hosts_in_line(line: &str) -> Vec<String> {
    let mut hosts = Vec::new();
    let bytes = line.as_bytes();
    let mut i = 0;
    while let Some(rel) = line[i..].find("://") {
        let scheme_end = i + rel;
        // walk back over the scheme to its start.
        let mut start = scheme_end;
        while start > 0 {
            let c = bytes[start - 1];
            if c.is_ascii_alphanumeric() || matches!(c, b'+' | b'.' | b'-') {
                start -= 1;
            } else {
                break;
            }
        }
        // the literal runs until a quote/backtick/whitespace/closing paren.
        let tail_start = scheme_end + 3;
        let end = line[tail_start..]
            .find(|c: char| c == '"' || c == '\'' || c == '`' || c.is_whitespace() || c == ')')
            .map_or(line.len(), |off| tail_start + off);
        let candidate = &line[start..end];
        if let Some(host) = host_from_url(candidate) {
            hosts.push(host);
        }
        i = end.max(scheme_end + 3);
    }
    hosts
}

/// A crude "identifier `(`" check: `name` immediately followed (after optional
/// spaces) by `(`, not preceded by an identifier char (so `myfetch(` and
/// `.fetch(` on an unrelated object still count only as a `fetch(` call, which
/// is acceptable for this heuristic).
fn contains_call(line: &str, name: &str) -> bool {
    let mut from = 0;
    while let Some(rel) = line[from..].find(name) {
        let at = from + rel;
        let after = at + name.len();
        let before_ok = at == 0
            || !line.as_bytes()[at - 1].is_ascii_alphanumeric() && line.as_bytes()[at - 1] != b'_';
        let after_ok = line[after..].trim_start().starts_with('(');
        if before_ok && after_ok {
            return true;
        }
        from = after;
    }
    false
}

// ---- per-function attribution (line heuristic; limits in the module docs) ----

/// A top-level function whose body is currently being attributed.
struct OpenFunction {
    facts: FunctionFacts,
    /// Brace depth before the declaration line; the function closes when the
    /// running depth returns to this value.
    base_depth: i32,
}

/// Attribute effects / time / random / unsafe lines to top-level named
/// functions by brace-depth tracking. Split out so it is unit-testable.
fn scan_functions(src: &str, rel: &str) -> Vec<FunctionFacts> {
    let mut out = Vec::new();
    let mut open: Option<OpenFunction> = None;
    let mut depth: i32 = 0;
    for (idx, line) in src.lines().enumerate() {
        let lineno = u32::try_from(idx + 1).unwrap_or(u32::MAX);
        let (opens, closes) = brace_deltas(line);
        if open.is_none()
            && depth == 0
            && let Some(name) = function_decl_name(line)
        {
            let mut facts = FunctionFacts {
                entrypoint: format!("ts:{rel}#{name}"),
                file: rel.to_owned(),
                line: lineno,
                ..FunctionFacts::default()
            };
            // The declaration line itself may carry the whole body
            // (`const f = () => fetch(u);`).
            attribute_line(line, rel, lineno, &mut facts);
            if opens > closes {
                open = Some(OpenFunction {
                    facts,
                    base_depth: depth,
                });
            } else {
                out.push(facts);
            }
            depth += opens - closes;
            continue;
        }
        if let Some(of) = open.as_mut() {
            attribute_line(line, rel, lineno, &mut of.facts);
        }
        depth += opens - closes;
        if let Some(of) = open.as_ref()
            && depth <= of.base_depth
        {
            out.push(open.take().expect("checked Some above").facts);
        }
    }
    if let Some(of) = open {
        out.push(of.facts); // EOF closed the file mid-function; keep the facts.
    }
    out
}

/// If `line` declares a top-level named function, return its name. Recognized:
/// `[export] [default] [async] function[*] NAME(` and
/// `[export] const|let|var NAME = …` where the initializer is a function or
/// arrow (`function` / `=>` on the same line).
fn function_decl_name(line: &str) -> Option<String> {
    let mut t = line.trim_start();
    for prefix in ["export ", "default ", "async "] {
        if let Some(rest) = t.strip_prefix(prefix) {
            t = rest.trim_start();
        }
    }
    if let Some(rest) = t.strip_prefix("function") {
        // Require a real keyword boundary (`functional(…)` is not a decl).
        if !rest.starts_with([' ', '\t', '*']) {
            return None;
        }
        let rest = rest
            .trim_start()
            .strip_prefix('*')
            .unwrap_or(rest)
            .trim_start();
        let name = leading_identifier(rest)?;
        if rest[name.len()..].trim_start().starts_with('(') {
            return Some(name.to_owned());
        }
        return None;
    }
    for kw in ["const ", "let ", "var "] {
        if let Some(rest) = t.strip_prefix(kw) {
            let rest = rest.trim_start();
            let name = leading_identifier(rest)?;
            let after = rest[name.len()..].trim_start();
            let assigns =
                after.starts_with('=') && !after.starts_with("==") && !after.starts_with("=>");
            if assigns && (line.contains("=>") || line.contains("function")) {
                return Some(name.to_owned());
            }
            return None;
        }
    }
    None
}

/// The leading JS identifier of `s`, if any.
fn leading_identifier(s: &str) -> Option<&str> {
    let first = s.chars().next()?;
    if !(first.is_ascii_alphabetic() || first == '_' || first == '$') {
        return None;
    }
    let end = s
        .find(|c: char| !(c.is_ascii_alphanumeric() || c == '_' || c == '$'))
        .unwrap_or(s.len());
    Some(&s[..end])
}

/// `(opens, closes)` braces on this line, skipping quoted strings and the tail
/// of a `//` comment. Template-literal `${…}` interpolation is not tracked —
/// a documented limit of the line heuristic.
fn brace_deltas(line: &str) -> (i32, i32) {
    let mut opens = 0;
    let mut closes = 0;
    let mut quote: Option<char> = None;
    let mut prev = '\0';
    for c in line.chars() {
        if let Some(q) = quote {
            if c == q && prev != '\\' {
                quote = None;
            }
        } else {
            match c {
                '"' | '\'' | '`' => quote = Some(c),
                '{' => opens += 1,
                '}' => closes += 1,
                '/' if prev == '/' => break,
                _ => {}
            }
        }
        prev = c;
    }
    (opens, closes)
}

/// Fold one body line into the open function's facts.
fn attribute_line(line: &str, rel: &str, lineno: u32, f: &mut FunctionFacts) {
    let fetches = count_calls(line, "fetch");
    f.effects += fetches;
    // Same-line method literal only: a POST/PATCH in a multi-line options
    // object is missed (documented limit of the line heuristic).
    if fetches > 0
        && ["\"POST\"", "'POST'", "\"PATCH\"", "'PATCH'"]
            .iter()
            .any(|m| line.contains(m))
    {
        f.idempotent_unsafe += 1;
    }
    for needle in ["Date.now(", "new Date(", "performance.now("] {
        f.time_reads += count_substr(line, needle);
    }
    for needle in [
        "Math.random(",
        "crypto.randomUUID(",
        "crypto.getRandomValues(",
    ] {
        f.random_reads += count_substr(line, needle);
    }
    for host in hosts_in_line(line) {
        f.targets.insert(host);
    }
    for needle in ["child_process", "worker_threads"] {
        if line.contains(needle) {
            f.unsafe_reasons
                .push(format!("{needle} use at {rel}:{lineno}"));
        }
    }
}

/// How many times `name(` occurs as a call on this line (the counting twin of
/// [`contains_call`]).
fn count_calls(line: &str, name: &str) -> u32 {
    let mut count = 0;
    let mut from = 0;
    while let Some(rel) = line[from..].find(name) {
        let at = from + rel;
        let after = at + name.len();
        let before_ok = at == 0
            || !line.as_bytes()[at - 1].is_ascii_alphanumeric() && line.as_bytes()[at - 1] != b'_';
        let after_ok = line[after..].trim_start().starts_with('(');
        if before_ok && after_ok {
            count += 1;
        }
        from = after;
    }
    count
}

/// Non-overlapping occurrences of `needle` in `line`.
fn count_substr(line: &str, needle: &str) -> u32 {
    u32::try_from(line.matches(needle).count()).unwrap_or(u32::MAX)
}

/// Recursively collect scannable files, skipping [`SKIP_DIRS`] and dotdirs.
fn collect(dir: &Path, out: &mut Vec<std::path::PathBuf>) {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return;
    };
    for entry in entries.flatten() {
        let path = entry.path();
        let name = entry.file_name();
        let name = name.to_string_lossy();
        if path.is_dir() {
            if SKIP_DIRS.contains(&name.as_ref()) || name.starts_with('.') {
                continue;
            }
            collect(&path, out);
        } else if path
            .extension()
            .and_then(|e| e.to_str())
            .is_some_and(|e| JS_EXTS.contains(&e))
        {
            out.push(path);
        }
    }
}

/// Project-relative path with `/` separators.
fn relative(project: &Path, path: &Path) -> String {
    path.strip_prefix(project)
        .unwrap_or(path)
        .to_string_lossy()
        .replace('\\', "/")
}

#[cfg(test)]
mod tests {
    use super::*;

    fn findings(src: &str) -> LangFindings {
        let mut f = LangFindings::default();
        scan_source(src, "app.ts", &mut f);
        f
    }

    #[test]
    fn fetch_and_url_literal_are_found() {
        let f = findings("const r = await fetch(\"https://api.example.com/v1/x\");\n");
        assert!(f.http_in_use);
        assert_eq!(f.hosts.len(), 1);
        assert_eq!(f.hosts[0].0, "api.example.com");
        assert_eq!(f.hosts[0].1.line, 1);
    }

    #[test]
    fn provider_imports_map_to_llm_targets() {
        let f = findings(
            "import OpenAI from \"openai\";\nimport Anthropic from '@anthropic-ai/sdk';\n",
        );
        let providers: Vec<_> = f.llm.iter().map(|(p, _)| p.as_str()).collect();
        assert!(providers.contains(&"openai"));
        assert!(providers.contains(&"anthropic"));
    }

    #[test]
    fn undici_import_marks_http_in_use() {
        let f = findings("import { request } from \"undici\";\n");
        assert!(f.http_in_use);
    }

    #[test]
    fn word_named_openai_variable_is_not_an_import() {
        let f = findings("const openai = 3;\n");
        assert!(f.llm.is_empty());
    }

    #[test]
    fn multiple_hosts_on_one_line() {
        let f = findings("x(\"https://a.example.com\", \"https://b.example.com/p\")\n");
        let hosts: Vec<_> = f.hosts.iter().map(|(h, _)| h.as_str()).collect();
        assert_eq!(hosts, ["a.example.com", "b.example.com"]);
    }

    // ---- function attribution (line heuristic) ----

    #[test]
    fn attributes_fetch_time_random_to_top_level_functions() {
        let src = "\
export async function ingest(rows) {
  const started = Date.now();
  const res = await fetch(\"https://api.example.com/v1/x\", { method: \"POST\" });
  const id = crypto.randomUUID();
  return { started, id, body: await res.json() };
}

function pure(a, b) {
  return a + b;
}
";
        let fns = scan_functions(src, "app.mjs");
        assert_eq!(fns.len(), 2);
        let ingest = &fns[0];
        assert_eq!(ingest.entrypoint, "ts:app.mjs#ingest");
        assert_eq!((ingest.file.as_str(), ingest.line), ("app.mjs", 1));
        assert_eq!(ingest.effects, 1);
        assert_eq!(ingest.idempotent_unsafe, 1, "same-line POST literal");
        assert_eq!(ingest.time_reads, 1);
        assert_eq!(ingest.random_reads, 1);
        assert!(ingest.targets.contains("api.example.com"));
        assert!(ingest.unsafe_reasons.is_empty());
        assert_eq!(fns[1].entrypoint, "ts:app.mjs#pure");
        assert_eq!(fns[1].effects, 0);
    }

    #[test]
    fn single_line_arrow_and_nested_callback_attribution() {
        let src = "\
const ping = () => fetch(\"https://a.example.com/health\");
const nightly = async () => {
  const results = await Promise.all(urls.map((u) => fetch(u)));
  return results;
};
";
        let fns = scan_functions(src, "jobs.ts");
        assert_eq!(fns.len(), 2);
        assert_eq!(fns[0].entrypoint, "ts:jobs.ts#ping");
        assert_eq!(fns[0].effects, 1);
        // The nested map callback's fetch attributes to the enclosing
        // top-level function — containment-by-lines, per the module docs.
        assert_eq!(fns[1].entrypoint, "ts:jobs.ts#nightly");
        assert_eq!(fns[1].effects, 1);
    }

    #[test]
    fn child_process_defeats_the_replay_safe_estimate() {
        let src = "\
export function shellOut() {
  const { execSync } = require(\"child_process\");
  execSync(\"ls\");
  return fetch(\"https://api.example.com/v1/x\");
}
";
        let fns = scan_functions(src, "run.js");
        assert_eq!(fns.len(), 1);
        assert_eq!(fns[0].effects, 1);
        assert_eq!(
            fns[0].unsafe_reasons,
            vec!["child_process use at run.js:2".to_owned()]
        );
    }

    #[test]
    fn class_methods_and_plain_calls_are_not_tracked_as_functions() {
        let src = "\
class Api {
  async load() {
    return fetch(\"https://a.example.com\");
  }
}
functional(1, 2);
const url = \"https://b.example.com\";
";
        assert!(scan_functions(src, "app.ts").is_empty());
    }

    #[test]
    fn braces_in_strings_and_comments_do_not_desync_depth() {
        let src = "\
function outer() {
  const s = \"{ not a brace }\";
  // } neither is this
  return fetch(\"https://a.example.com\");
}
function after() {
  return Date.now();
}
";
        let fns = scan_functions(src, "app.ts");
        assert_eq!(fns.len(), 2);
        assert_eq!(fns[0].effects, 1);
        assert_eq!(fns[1].entrypoint, "ts:app.ts#after");
        assert_eq!(fns[1].time_reads, 1);
    }
}
