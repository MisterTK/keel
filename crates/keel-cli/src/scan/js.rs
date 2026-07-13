//! The JS/TS static scan — a real parse on [oxc](https://oxc.rs).
//!
//! oxc is pure Rust, so `keel init` still needs no Node toolchain (the design
//! constraint the old line-oriented scan existed to satisfy — this replaces
//! that documented simplification with an actual AST walk, dx-spec §2). Per
//! file, [`ast`] extracts:
//!
//! - HTTP in use: a `fetch(…)` call, or an import/require/dynamic-import of a
//!   known outbound client (`undici`, `node:http`/`https`, `axios`, `got`,
//!   `node-fetch`, `superagent`, DB clients like `pg`/`redis`) — including
//!   multi-line and aliased forms the old regex missed.
//! - provider SDKs: `openai`, `@anthropic-ai/sdk` (also bare `anthropic`),
//!   AI-SDK provider packages → `llm:*` targets. TS `import type` is excluded:
//!   type-only imports are erased at runtime and are not evidence.
//! - URL literals: hosts from string literals *and* template-literal quasis
//!   (`` `https://api.x.com/${id}` `` resolves; `` `${scheme}://x` `` no
//!   longer false-positives).
//! - effect call sites with enclosing-function attribution
//!   ([`super::CallSite`]) for `keel flows suggest`, and a relative-import
//!   module graph ([`JsScan::imports`]).
//!
//! The tradeoff (accepted here): a URL built by concatenation, or an exotic
//! import form, is missed — exactly the ~20% the import-time and observed-run
//! evidence sources exist to catch (dx-spec §2). Line numbers are exact.
//!
//! ## Function attribution (for `keel flows suggest`)
//!
//! [`ast::ScanVisitor`] tracks a real enclosing-scope stack, so attribution
//! is exact scope containment, not a line heuristic. A [`super::FunctionFacts`]
//! entry opens for a **function bound directly at module top level**: a
//! function declaration (`function f(…) {`, `export async function f(…) {`),
//! a named function expression, or an arrow assigned directly to a top-level
//! `const`/`let`/`var` (`const f = () => {…}`, `const f = function () {…}`).
//! Everything lexically nested inside it — inner arrows, callbacks, helper
//! closures — attributes to that top-level entry, exactly like the Python
//! pass's real `ast` containment (`scan::python`).
//!
//! Class methods and object-literal methods do **not** open their own entry
//! — even though the scope walk names them (`Class.method` shows up in
//! [`super::CallSite::function`] for call-site evidence), a class body is not
//! a flow entrypoint any more than a Python `class` body is. A `fetch` inside
//! `class Api { async load() {…} }` is evidenced in the file-level scan (host
//! literals, call sites) but not credited to a function. The same holds for
//! anonymous top-level values (`export default function () {…}`): still
//! evidenced, never a flow candidate. This is strictly more precise than the
//! old line-heuristic scan it replaces, which additionally missed Allman-brace
//! functions and desynced on template-literal interpolation.
//!
//! A file that fails to parse is warned about on stderr and skipped — never a
//! crash, and never silent narrowing (mirrors the Python pass's
//! parse-or-skip). `files_scanned` counts parsed files only.

mod ast;

use std::collections::{BTreeMap, BTreeSet};
use std::path::Path;

use super::{FunctionFacts, LangFindings, SKIP_DIRS};

/// Extensions the scan reads.
const JS_EXTS: &[&str] = &["js", "mjs", "cjs", "ts", "mts", "cts", "jsx", "tsx"];

/// Extensions tried, in order, when resolving an extensionless relative
/// import (`./x` → `x.ts`, …, then `x/index.ts`, …).
const RESOLVE_EXTS: &[&str] = &["ts", "tsx", "mts", "cts", "js", "jsx", "mjs", "cjs"];

/// The JS pass result.
#[derive(Debug, Clone, Default)]
pub struct JsScan {
    /// Files parsed (a file that failed to parse is not counted).
    pub files_scanned: usize,
    /// Project-relative paths that failed to parse (warned on stderr, skipped).
    pub parse_failures: Vec<String>,
    /// Module graph: file → project-local files it imports. Relative
    /// specifiers only, resolved against the scanned file set (exact path,
    /// then per-extension, then `…/index.<ext>`).
    pub imports: BTreeMap<String, BTreeSet<String>>,
    /// Findings, ready to merge.
    pub findings: LangFindings,
    /// Per-function attribution (top-level named functions only — real AST
    /// scope containment, see the module docs for the exact policy).
    pub functions: Vec<FunctionFacts>,
}

/// Scan `project` for JS/TS effect seams.
pub fn scan(project: &Path) -> JsScan {
    let mut files = Vec::new();
    collect(project, &mut files);
    files.sort();

    let rels: Vec<String> = files.iter().map(|p| relative(project, p)).collect();
    let known: BTreeSet<&str> = rels.iter().map(String::as_str).collect();

    let mut result = JsScan::default();
    for (path, rel) in files.iter().zip(&rels) {
        let Ok(src) = std::fs::read_to_string(path) else {
            continue;
        };
        if let Some(extras) = ast::scan_source(&src, rel, &mut result.findings) {
            result.files_scanned += 1;
            result.functions.extend(extras.functions);
            let resolved: BTreeSet<String> = extras
                .relative_imports
                .iter()
                .filter_map(|spec| resolve_relative(rel, spec, &known))
                .collect();
            if !resolved.is_empty() {
                result.imports.insert(rel.clone(), resolved);
            }
        } else {
            eprintln!("keel: warning: skipped {rel}: JS/TS parse failed");
            result.parse_failures.push(rel.clone());
        }
    }
    result
}

/// Resolve a relative import specifier against the scanned file set:
/// exact path, then `<spec>.<ext>`, then `<spec>/index.<ext>`.
fn resolve_relative(importer: &str, spec: &str, known: &BTreeSet<&str>) -> Option<String> {
    let dir = importer.rsplit_once('/').map_or("", |(d, _)| d);
    let joined = normalize(dir, spec)?;
    if known.contains(joined.as_str()) {
        return Some(joined);
    }
    for ext in RESOLVE_EXTS {
        let candidate = format!("{joined}.{ext}");
        if known.contains(candidate.as_str()) {
            return Some(candidate);
        }
    }
    for ext in RESOLVE_EXTS {
        let candidate = format!("{joined}/index.{ext}");
        if known.contains(candidate.as_str()) {
            return Some(candidate);
        }
    }
    None
}

/// Join `dir` and a `./`/`../` specifier, normalizing `.` and `..` segments.
/// `None` when `..` escapes the project root.
fn normalize(dir: &str, spec: &str) -> Option<String> {
    let mut parts: Vec<&str> = if dir.is_empty() {
        Vec::new()
    } else {
        dir.split('/').collect()
    };
    for seg in spec.split('/') {
        match seg {
            "" | "." => {}
            ".." => {
                parts.pop()?;
            }
            other => parts.push(other),
        }
    }
    Some(parts.join("/"))
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
    use std::fs;
    use tempfile::TempDir;

    /// Parse one in-memory source as `name` and return its findings and
    /// per-top-level-function attribution.
    fn scan_str_named(src: &str, name: &str) -> (LangFindings, Vec<FunctionFacts>) {
        let mut f = LangFindings::default();
        let extras = ast::scan_source(src, name, &mut f).expect("fixture failed to parse");
        (f, extras.functions)
    }

    /// Parse one in-memory source as `name` and return its findings.
    fn findings_named(src: &str, name: &str) -> LangFindings {
        scan_str_named(src, name).0
    }

    fn findings(src: &str) -> LangFindings {
        findings_named(src, "app.ts")
    }

    /// Parse one in-memory source as `name` and return its per-top-level-
    /// function attribution.
    fn functions_named(src: &str, name: &str) -> Vec<FunctionFacts> {
        scan_str_named(src, name).1
    }

    fn functions(src: &str) -> Vec<FunctionFacts> {
        functions_named(src, "app.ts")
    }

    // ---- conformance with the old regex scan (same inputs, same findings) ----

    #[test]
    fn fetch_and_url_literal_are_found() {
        let f = findings("const r = await fetch(\"https://api.example.com/v1/x\");\n");
        assert!(f.http_in_use);
        assert_eq!(f.hosts.len(), 1);
        assert_eq!(f.hosts[0].0, "api.example.com");
        assert_eq!(f.hosts[0].1.line, 1);
        assert!(f.libs.contains("fetch"));
    }

    #[test]
    fn provider_imports_map_to_llm_targets() {
        let f = findings(
            "import OpenAI from \"openai\";\nimport Anthropic from '@anthropic-ai/sdk';\n",
        );
        let providers: Vec<_> = f.llm.iter().map(|(p, _)| p.as_str()).collect();
        assert!(providers.contains(&"openai"));
        assert!(providers.contains(&"anthropic"));
        assert_eq!(f.llm[0].1.line, 1);
        assert_eq!(f.llm[1].1.line, 2);
    }

    #[test]
    fn undici_import_marks_http_in_use() {
        let f = findings("import { request } from \"undici\";\n");
        assert!(f.http_in_use);
        assert!(f.libs.contains("undici"));
    }

    #[test]
    fn word_named_openai_variable_is_not_an_import() {
        let f = findings("const openai = 3;\n");
        assert!(f.llm.is_empty());
    }

    #[test]
    fn multiple_hosts_on_one_line() {
        let f = findings("fetch(1);\nx(\"https://a.example.com\", \"https://b.example.com/p\");\n");
        let hosts: Vec<_> = f.hosts.iter().map(|(h, _)| h.as_str()).collect();
        assert_eq!(hosts, ["a.example.com", "b.example.com"]);
        assert_eq!(f.hosts[0].1.line, 2);
    }

    #[test]
    fn member_fetch_still_counts() {
        // The old scan accepted `.fetch(` — keep that looseness: it is how
        // `globalThis.fetch(…)` and `this.fetch(…)` appear.
        let f = findings("globalThis.fetch(\"https://api.example.com\");\n");
        assert!(f.http_in_use);
        assert!(f.libs.contains("fetch"));
    }

    // ---- cases the regex provably got wrong, now correct ----

    #[test]
    fn multi_line_import_is_found() {
        // The specifier and the `import` keyword sit on different lines: the
        // line-oriented scan missed this entirely.
        let f = findings("import {\n  request,\n} from \"undici\";\n");
        assert!(f.http_in_use);
        assert!(f.libs.contains("undici"));
    }

    #[test]
    fn import_type_is_not_runtime_evidence() {
        // `import type` is erased by tsc: the regex flagged it as an OpenAI
        // dependency; the AST walk knows better.
        let f = findings("import type { ChatModel } from \"openai\";\n");
        assert!(f.llm.is_empty());
        assert!(!f.http_in_use);
    }

    #[test]
    fn type_only_specifier_is_skipped_but_value_binds() {
        let f =
            findings("import { type ClientOptions, request } from \"undici\";\nrequest(\"x\");\n");
        assert!(f.http_in_use);
        let callees: Vec<_> = f.call_sites.iter().map(|c| c.callee.as_str()).collect();
        assert_eq!(callees, ["undici.request"]);
    }

    #[test]
    fn template_literal_host_is_found() {
        let f = findings("const id = 1;\nawait fetch(`https://api.example.com/v1/${id}`);\n");
        assert_eq!(f.hosts.len(), 1);
        assert_eq!(f.hosts[0].0, "api.example.com");
        assert_eq!(f.hosts[0].1.line, 2);
    }

    #[test]
    fn interpolated_scheme_is_not_a_false_positive_host() {
        // The regex walked back from `://` across the `}` and reported
        // `internal` as a host. The quasi has no scheme, so no host.
        let f = findings("const scheme = \"https\";\nconst u = `${scheme}://internal`;\n");
        assert!(f.hosts.is_empty());
    }

    #[test]
    fn require_and_dynamic_import_are_imports() {
        let cjs = findings_named(
            "const { request } = require(\"undici\");\nrequest(\"https://api.example.com\");\n",
            "app.cjs",
        );
        assert!(cjs.http_in_use);
        assert!(cjs.libs.contains("undici"));
        assert_eq!(cjs.call_sites[0].callee, "undici.request");

        let dynamic = findings("const undici = await import(\"undici\");\n");
        assert!(dynamic.http_in_use);
        assert!(dynamic.libs.contains("undici"));
    }

    #[test]
    fn subpath_import_classifies_by_package() {
        let f = findings("import { toFile } from \"openai/uploads\";\n");
        assert_eq!(f.llm.len(), 1);
        assert_eq!(f.llm[0].0, "openai");
    }

    // ---- new evidence the regex never had ----

    #[test]
    fn effect_lib_imports_gate_hosts() {
        // A pg import plus a DSN literal yields the DB host (the Python pass
        // resolves the same shape via psycopg + DSN).
        let f = findings(
            "import { Client } from \"pg\";\nconst DSN = \"postgres://db.internal:5432/app\";\n",
        );
        assert!(f.http_in_use);
        assert!(f.libs.contains("pg"));
        assert_eq!(f.hosts[0].0, "db.internal");
    }

    #[test]
    fn axios_default_import_call_sites() {
        let f = findings(
            "import axios from \"axios\";\nawait axios.get(\"https://api.example.com\");\n",
        );
        assert!(f.http_in_use);
        assert!(f.libs.contains("axios"));
        assert_eq!(f.call_sites[0].callee, "axios.get");
    }

    #[test]
    fn client_instance_traces_back_to_provider() {
        let f = findings(
            "import OpenAI from \"openai\";\nconst client = new OpenAI();\n\
             export async function ask() {\n  return client.chat.completions.create({});\n}\n",
        );
        assert_eq!(f.call_sites.len(), 1);
        let site = &f.call_sites[0];
        assert_eq!(site.callee, "openai.chat.completions.create");
        assert_eq!(site.function.as_deref(), Some("ask"));
        assert_eq!(site.line, 4);
    }

    #[test]
    fn ai_sdk_provider_packages_pin_llm_targets() {
        let f = findings("import { anthropic } from \"@ai-sdk/anthropic\";\n");
        assert!(f.libs.contains("ai-sdk"));
        assert_eq!(f.llm[0].0, "anthropic");
    }

    // ---- attribution ----

    #[test]
    fn attribution_covers_functions_methods_and_arrows() {
        let f = findings(
            "class Api {\n  async load() {\n    return fetch(\"https://a.x\");\n  }\n}\n\
             function outer() {\n  const inner = async () => fetch(\"https://b.x\");\n  return inner;\n}\n\
             const top = fetch(\"https://c.x\");\n",
        );
        let sites: Vec<(&str, Option<&str>)> = f
            .call_sites
            .iter()
            .map(|c| (c.callee.as_str(), c.function.as_deref()))
            .collect();
        assert_eq!(
            sites,
            [
                ("fetch", Some("Api.load")),
                ("fetch", Some("outer.inner")),
                ("fetch", None),
            ]
        );
    }

    #[test]
    fn tsx_parses_with_jsx_and_types() {
        let f = findings_named(
            "type Props = { url: string };\n\
             export function Widget({ url }: Props) {\n\
               const load = () => fetch(\"https://api.example.com\");\n\
               return <button onClick={load}>go</button>;\n\
             }\n",
            "widget.tsx",
        );
        assert!(f.http_in_use);
        assert_eq!(f.hosts[0].0, "api.example.com");
        assert_eq!(
            f.call_sites[0].function.as_deref(),
            Some("Widget.load"),
            "arrow inside a component attributes to Widget.load"
        );
    }

    // ---- pass-level behavior (filesystem) ----

    #[test]
    fn broken_file_is_skipped_never_fatal() {
        let dir = TempDir::new().unwrap();
        fs::write(dir.path().join("broken.ts"), "function (((\n").unwrap();
        fs::write(dir.path().join("ok.ts"), "import \"undici\";\n").unwrap();
        let scan = scan(dir.path());
        assert_eq!(scan.files_scanned, 1, "only the parseable file counts");
        assert_eq!(scan.parse_failures, ["broken.ts"]);
        assert!(scan.findings.http_in_use);
    }

    #[test]
    fn import_graph_resolves_relative_specifiers() {
        let dir = TempDir::new().unwrap();
        fs::create_dir(dir.path().join("lib")).unwrap();
        fs::write(
            dir.path().join("app.ts"),
            "import { helper } from \"./lib/helper\";\nimport { util } from \"./util\";\n\
             import express from \"express\";\n",
        )
        .unwrap();
        fs::write(dir.path().join("util.ts"), "export const util = 1;\n").unwrap();
        fs::write(
            dir.path().join("lib").join("helper.ts"),
            "import { util } from \"../util\";\nexport const helper = util;\n",
        )
        .unwrap();
        let scan = scan(dir.path());
        assert_eq!(scan.files_scanned, 3);
        assert_eq!(
            scan.imports.get("app.ts"),
            Some(&BTreeSet::from([
                "lib/helper.ts".to_owned(),
                "util.ts".to_owned()
            ]))
        );
        assert_eq!(
            scan.imports.get("lib/helper.ts"),
            Some(&BTreeSet::from(["util.ts".to_owned()]))
        );
    }

    #[test]
    fn import_graph_resolves_index_files() {
        let dir = TempDir::new().unwrap();
        fs::create_dir(dir.path().join("api")).unwrap();
        fs::write(dir.path().join("app.js"), "import api from \"./api\";\n").unwrap();
        fs::write(
            dir.path().join("api").join("index.js"),
            "export default 1;\n",
        )
        .unwrap();
        let scan = scan(dir.path());
        assert_eq!(
            scan.imports.get("app.js"),
            Some(&BTreeSet::from(["api/index.js".to_owned()]))
        );
    }

    #[test]
    fn deterministic_across_runs() {
        let dir = TempDir::new().unwrap();
        fs::write(
            dir.path().join("a.ts"),
            "import { request } from \"undici\";\nrequest(\"https://a.example.com\");\n",
        )
        .unwrap();
        fs::write(
            dir.path().join("b.ts"),
            "await fetch(\"https://b.example.com\");\n",
        )
        .unwrap();
        let one = scan(dir.path());
        let two = scan(dir.path());
        assert_eq!(format!("{:?}", one.findings), format!("{:?}", two.findings));
        assert_eq!(one.imports, two.imports);
    }

    // ---- function attribution (real AST scope containment) ----

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
        let fns = functions_named(src, "app.mjs");
        assert_eq!(fns.len(), 2);
        let ingest = &fns[0];
        assert_eq!(ingest.entrypoint, "ts:app.mjs#ingest");
        assert_eq!((ingest.file.as_str(), ingest.line), ("app.mjs", 1));
        assert_eq!(ingest.effects, 1);
        assert_eq!(ingest.idempotent_unsafe, 1, "object-literal POST method");
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
        let fns = functions_named(src, "jobs.ts");
        assert_eq!(fns.len(), 2);
        assert_eq!(fns[0].entrypoint, "ts:jobs.ts#ping");
        assert_eq!(fns[0].effects, 1);
        // The nested map callback's fetch attributes to the enclosing
        // top-level function — real scope containment, not a line heuristic.
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
        let fns = functions_named(src, "run.js");
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
        assert!(functions(src).is_empty());
    }

    #[test]
    fn braces_in_strings_and_comments_do_not_desync_depth() {
        // A regression fixture from the old line-oriented scan, where braces
        // inside string/comment text could desync brace-depth tracking. A
        // real parse never has this problem by construction — kept as a
        // cheap sanity check that nothing about the new pass reintroduces it.
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
        let fns = functions(src);
        assert_eq!(fns.len(), 2);
        assert_eq!(fns[0].effects, 1);
        assert_eq!(fns[1].entrypoint, "ts:app.ts#after");
        assert_eq!(fns[1].time_reads, 1);
    }
}
