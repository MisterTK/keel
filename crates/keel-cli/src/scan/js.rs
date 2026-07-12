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

use std::path::Path;

use super::{LangFindings, SKIP_DIRS, Sighting, host_from_url};

/// Extensions the scan reads.
const JS_EXTS: &[&str] = &["js", "mjs", "cjs", "ts", "mts", "cts", "jsx", "tsx"];

/// The JS pass result.
#[derive(Debug, Clone, Default)]
pub struct JsScan {
    /// Files read.
    pub files_scanned: usize,
    /// Findings, ready to merge.
    pub findings: LangFindings,
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
}
