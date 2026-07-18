//! Static scanning — the first of the three evidence sources behind `keel init`
//! and `keel doctor` (dx-spec §2). No code runs: we read the project's source
//! and find where effects enter.
//!
//! Two scanners, one merged result:
//! - [`python`] shells an `ast`-walker out to `python3 -` for precise Python
//!   parsing (imports of known effect libraries, URL/DSN string literals).
//! - [`js`] parses JS/TS/JSX in-process with oxc (no Node toolchain needed)
//!   for `fetch`/`undici`/`node:http` usage, provider-SDK imports, effect-lib
//!   call sites, and URL literals.
//!
//! Both label every finding with `file:line`, so the generated `keel.toml` can
//! cite where each target was found and trust stays inspectable.

pub mod js;
pub mod python;

use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};

/// What kind of target a sighting resolves to. Governs the policy block
/// `keel init` writes (an `llm:*` target gets the LLM pack; a host gets the
/// outbound pack).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TargetClass {
    /// A network host, e.g. `api.stripe.com` — from a URL/DSN literal.
    Host,
    /// A semantic `llm:<provider>` target — from a provider SDK import.
    Llm,
}

/// How a sighted host's traffic is dispatched, best-known across sightings.
/// Ordering is meaningful: `Tracked < UntrackedKnown < Unknown`, so merging
/// (`min`) always keeps the most favorable class seen for a host across every
/// file/language that sighted it. This is what `keel doctor` (a later
/// program task) uses to say honestly what Keel can and cannot see: a host
/// is `Tracked` if some sighting reached it through a registry-adapted
/// library, `UntrackedKnown` if the best reach was a known-but-unadapted
/// transport (stdlib `urllib`/`http.client`), and `Unknown` if no transport
/// evidence was found near any sighting at all.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum TransportClass {
    /// A registry-adapted library is in reach — Keel can wrap this.
    Tracked,
    /// A known transport Keel does not adapt (stdlib urllib/http.client).
    UntrackedKnown,
    /// A URL literal with no recognizable transport nearby.
    Unknown,
}

/// One effect call site with enclosing-function attribution — an internal
/// detail of the JS/TS pass ([`js`]), which uses it to verify its real
/// scope-chain tracking (dotted paths like `Class.method`) independently of
/// the coarser top-level-only [`FunctionFacts`] attribution `keel flows
/// suggest` consumes. Not exposed on [`ScanResult`]. Field order is the sort
/// order (file, then line).
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord)]
pub struct CallSite {
    /// Project-relative path with `/` separators.
    pub file: String,
    /// 1-based line of the call expression.
    pub line: u32,
    /// What is called, rooted at the effect library where the receiver is
    /// known (`fetch`, `undici.request`, `openai.chat.completions.create`).
    pub callee: String,
    /// Dotted enclosing-scope path (`Class.method`, `outer.inner`), or `None`
    /// at module top level. Anonymous scopes inherit the nearest named scope.
    pub function: Option<String>,
}

/// One externally-launched process the scan saw — traffic inside it is
/// outside Keel's visibility regardless of policy. Field order is the sort
/// order (file, then line).
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord)]
pub struct SubprocessSighting {
    /// Project-relative path with `/` separators.
    pub file: String,
    /// 1-based line of the launching call.
    pub line: u32,
    /// The launching call, e.g. `subprocess.run`, `os.system`,
    /// `child_process.spawn`.
    pub launcher: String,
    /// The literal argv/command line when statically extractable
    /// (a bare string, or a list/tuple of string-literal elements); otherwise
    /// `"<dynamic>"`.
    pub command: String,
}

/// One file the scan judged dependency-averse: stdlib-only imports plus a
/// risk/gate/guard/auth/valid/safety/kill name or docstring signal, or an
/// explicit `# keel: exclude` marker. Markers win in both directions: an
/// exclude marker forces this classification regardless of imports, and an
/// include marker defeats the heuristic even where it would otherwise match.
/// `keel doctor`/`keel init` (a later program task) use this to honestly
/// exclude hosts seen only in such files from proposed policy. Field order
/// is the sort order (by file).
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord)]
pub struct DepAverseFile {
    /// Project-relative path with `/` separators.
    pub file: String,
    /// `"marker"` for an explicit `# keel: exclude`, or
    /// `"stdlib-only + name/docstring signal: <word>"`.
    pub reason: String,
}

/// One place a target was seen: a project-relative path and 1-based line.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord)]
pub struct Sighting {
    /// Project-relative path with `/` separators.
    pub file: String,
    /// 1-based line number.
    pub line: u32,
}

impl Sighting {
    /// Render as the `file:line` token used in evidence comments.
    pub fn label(&self) -> String {
        format!("{}:{}", self.file, self.line)
    }
}

/// A target and everywhere the static scan saw it, deduplicated and ordered.
#[derive(Debug, Clone)]
pub struct TargetEvidence {
    /// The target's class.
    pub class: TargetClass,
    /// Sorted, unique sightings.
    pub sightings: BTreeSet<Sighting>,
}

/// Per-function effect attribution — the evidence behind `keel flows suggest`.
///
/// Each language pass attributes what it finds *inside* a function definition
/// to that function: intercepted-effect call sites, calls that read time or
/// randomness (virtualized under Tier 2 replay), and constructs that defeat
/// replay outright (threads, subprocesses, raw sockets). Both passes
/// attribute by real containment: the Python walker via `ast` module-level
/// def bodies, the JS/TS pass via a real oxc scope walk (see [`js`]) — an
/// entry opens only for a function bound directly at module top level; class
/// methods and nested/inner functions roll up into the enclosing top-level
/// entry rather than opening their own.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct FunctionFacts {
    /// The full flow-entrypoint ref this function would be designated as —
    /// `py:pipeline.ingest:main` or `ts:jobs/nightly.ts#run` (the `ts:`
    /// namespace covers all JS/TS files).
    pub entrypoint: String,
    /// Project-relative path of the defining file.
    pub file: String,
    /// 1-based line of the `def`/`function`.
    pub line: u32,
    /// Intercepted-effect call sites (HTTP / LLM / DSN-bearing libraries).
    pub effects: u32,
    /// Effect calls that are not idempotent-safe to re-send (POST/PATCH-shaped)
    /// and carry no idempotency evidence.
    pub idempotent_unsafe: u32,
    /// Wall-clock reads (`time.time`, `datetime.now`, `Date.now`, …) — these
    /// are virtualized (journaled + replayed) under Tier 2.
    pub time_reads: u32,
    /// Randomness reads (`random.*`, `uuid4`, `Math.random`, …) — also
    /// virtualized under Tier 2.
    pub random_reads: u32,
    /// Why replay would be unsafe (empty = the replay-safe estimate holds).
    /// Each reason cites `what at file:line`; sorted, deterministic.
    pub unsafe_reasons: Vec<String>,
    /// Targets referenced inside the function (hosts from URL literals,
    /// `llm:<provider>` from SDK calls) — the join key into `.keel/discovery.db`.
    pub targets: BTreeSet<String>,
}

/// The merged output of both scanners.
#[derive(Debug, Clone, Default)]
pub struct ScanResult {
    /// Number of source files parsed (Python + JS/TS) — the header's "N static
    /// scans".
    pub files_scanned: usize,
    /// Whether `python3` was available for the Python pass. When false, Python
    /// files could not be scanned; `keel init` notes this on stderr rather than
    /// letting it silently narrow coverage.
    pub python_available: bool,
    /// Discovered targets, keyed by target string, ordered.
    pub targets: BTreeMap<String, TargetEvidence>,
    /// Effect-library names detected across the project (e.g. `httpx`,
    /// `openai`, `boto3`, `fetch`). `keel doctor` cross-references these against
    /// its adapter registry to classify coverage.
    pub libs: BTreeSet<String>,
    /// Per-function attribution (see [`FunctionFacts`]), sorted by
    /// `(file, line)` — deterministic across runs.
    pub functions: Vec<FunctionFacts>,
    /// Known resilience-library names detected (Python-only as of this
    /// build — see [`LangFindings::resilience_libs`]).
    pub resilience_libs: BTreeSet<String>,
    /// Best-known [`TransportClass`] per sighted host, merged across every
    /// file and language that sighted it (minimum = best class wins). Unlike
    /// `targets`, this is never gated on `http_in_use` — it exists precisely
    /// to let `keel doctor` report on hosts Keel cannot see at all.
    pub host_transports: BTreeMap<String, TransportClass>,
    /// Every externally-launched process the scan saw, sorted by
    /// `(file, line)` — deterministic across runs. `keel doctor` uses this to
    /// call out where Keel's visibility ends at a process boundary.
    pub subprocesses: Vec<SubprocessSighting>,
    /// Files judged dependency-averse across the project, sorted by file —
    /// see [`DepAverseFile`]. Python-only as of this build (see
    /// [`LangFindings::dependency_averse`]).
    pub dependency_averse: Vec<DepAverseFile>,
}

impl ScanResult {
    fn add(&mut self, target: String, class: TargetClass, file: String, line: u32) {
        self.targets
            .entry(target)
            .or_insert_with(|| TargetEvidence {
                class,
                sightings: BTreeSet::new(),
            })
            .sightings
            .insert(Sighting { file, line });
    }
}

/// Scan `project` with both scanners and merge. Host targets are only emitted
/// when the language pass also saw an HTTP client in use (a bare URL in a
/// non-networked file is not evidence of an outbound call), keeping the output
/// honest.
pub fn scan(project: &Path) -> ScanResult {
    let mut result = ScanResult::default();

    let py = python::scan(project);
    result.python_available = py.available;
    result.files_scanned += py.files_scanned;
    merge_lang(&mut result, &py.findings);
    result.functions.extend(py.functions);

    let js = js::scan(project);
    result.files_scanned += js.files_scanned;
    merge_lang(&mut result, &js.findings);
    result.functions.extend(js.functions);

    result
        .functions
        .sort_by(|a, b| (&a.file, a.line, &a.entrypoint).cmp(&(&b.file, b.line, &b.entrypoint)));
    result.subprocesses.sort();
    result.dependency_averse.sort();
    result
}

/// One language scanner's raw findings before host-gating.
#[derive(Debug, Clone, Default)]
pub struct LangFindings {
    /// Provider SDK imports → `llm:*` targets.
    pub llm: Vec<(String, Sighting)>,
    /// URL/DSN host literals → host targets (gated on `http_in_use`).
    pub hosts: Vec<(String, Sighting)>,
    /// Whether an HTTP client (http lib / fetch / undici) was seen at all.
    pub http_in_use: bool,
    /// Effect-library names detected (for `keel doctor`'s registry cross-check).
    pub libs: BTreeSet<String>,
    /// Effect call sites with enclosing-function attribution.
    pub call_sites: Vec<CallSite>,
    /// Known resilience-library names detected (e.g. `tenacity`, `backoff`)
    /// — a `keel doctor` signal for pre-existing retry/backoff that might
    /// now silently compound with Keel's own. Deliberately separate from
    /// `libs`: these are libraries Keel never adapts, so merging them in
    /// would misclassify them as an "invisible" coverage gap.
    pub resilience_libs: BTreeSet<String>,
    /// Per-sighting [`TransportClass`] for every host this language pass saw,
    /// keyed by host. Never gated on `http_in_use` — a bare URL literal with
    /// no reachable transport is exactly the `Unknown` case `keel doctor`
    /// needs to report honestly.
    pub host_transports: BTreeMap<String, TransportClass>,
    /// Externally-launched processes this language pass saw (see
    /// [`SubprocessSighting`]).
    pub subprocesses: Vec<SubprocessSighting>,
    /// Files this language pass judged dependency-averse (see
    /// [`DepAverseFile`]).
    pub dependency_averse: Vec<DepAverseFile>,
}

fn merge_lang(result: &mut ScanResult, f: &LangFindings) {
    for (provider, s) in &f.llm {
        result.add(
            format!("llm:{provider}"),
            TargetClass::Llm,
            s.file.clone(),
            s.line,
        );
    }
    if f.http_in_use {
        for (host, s) in &f.hosts {
            result.add(host.clone(), TargetClass::Host, s.file.clone(), s.line);
        }
    }
    for lib in &f.libs {
        result.libs.insert(lib.clone());
    }
    for lib in &f.resilience_libs {
        result.resilience_libs.insert(lib.clone());
    }
    for (host, class) in &f.host_transports {
        result
            .host_transports
            .entry(host.clone())
            .and_modify(|c| *c = (*c).min(*class))
            .or_insert(*class);
    }
    result.subprocesses.extend(f.subprocesses.iter().cloned());
    result
        .dependency_averse
        .extend(f.dependency_averse.iter().cloned());
}

/// Directory names never descended into during a filesystem walk — scans,
/// `keel init`'s Python-file check, and `keel flows resume`'s module search
/// all share this one list (previously three drifted copies; see the
/// 2026-07-14 fast-follow that consolidated them).
pub(crate) const SKIP_DIRS: &[&str] = &[
    ".keel",
    ".git",
    ".hg",
    ".svn",
    "__pycache__",
    "node_modules",
    ".venv",
    "venv",
    ".mypy_cache",
    ".pytest_cache",
    "dist",
    "build",
    "target",
];

/// Recursively collect files under `dir` whose extension is one of
/// `extensions`, skipping [`SKIP_DIRS`] and dot-prefixed directories. The
/// one walker for "find source files by extension" in this crate — shared
/// by the JS/TS scanner ([`js`]), `keel init`'s Python-file check, and
/// `keel run`'s directory-entry resolution.
pub(crate) fn collect_files(dir: &Path, extensions: &[&str], out: &mut Vec<PathBuf>) {
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
            collect_files(&path, extensions, out);
        } else if path
            .extension()
            .and_then(|e| e.to_str())
            .is_some_and(|e| extensions.contains(&e))
        {
            out.push(path);
        }
    }
}

/// Extract the host from a `scheme://host[:port][/…]` literal, lowercased and
/// without port/userinfo/path. Returns `None` for non-URL strings. Shared by
/// both scanners so Python and JS agree on what a host is.
pub(crate) fn host_from_url(s: &str) -> Option<String> {
    let s = s.trim();
    let (scheme, rest) = s.split_once("://")?;
    if scheme.is_empty()
        || !scheme
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '+' | '.' | '-'))
        || !scheme
            .chars()
            .next()
            .is_some_and(|c| c.is_ascii_alphabetic())
    {
        return None;
    }
    // authority ends at the first '/', '?', or '#'.
    let authority = rest.split(['/', '?', '#']).next().unwrap_or(rest);
    // strip userinfo, then port.
    let host_port = authority.rsplit('@').next().unwrap_or(authority);
    let host = host_port.split(':').next().unwrap_or(host_port);
    if host.is_empty() || host.contains(|c: char| c.is_whitespace()) {
        return None;
    }
    Some(host.to_ascii_lowercase())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn host_extraction_strips_port_userinfo_and_path() {
        assert_eq!(
            host_from_url("https://api.stripe.com/v1/x").as_deref(),
            Some("api.stripe.com")
        );
        assert_eq!(
            host_from_url("postgres://u:p@db.internal:5432/app").as_deref(),
            Some("db.internal")
        );
        assert_eq!(host_from_url("HTTPS://API.X").as_deref(), Some("api.x"));
        assert_eq!(host_from_url("not a url"), None);
        assert_eq!(host_from_url("://nohost"), None);
        assert_eq!(host_from_url("1bad://x"), None);
    }
}
