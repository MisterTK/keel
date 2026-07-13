//! The oxc AST walk behind the JS/TS scan: one visitor pass per file.
//!
//! What the walk extracts (all with exact 1-based lines from byte spans):
//!
//! - **Imports** (static, `require(…)`, dynamic `import(…)`) of known effect
//!   libraries — HTTP clients, provider SDKs, DB clients — excluding
//!   TS `import type` declarations and `type`-only specifiers, which are
//!   erased at runtime and therefore not evidence.
//! - **Call sites**: `fetch(…)` (identifier or member property, matching the
//!   front end's global-fetch wrap), plus calls whose receiver traces back to
//!   an effect-library binding (named/default/namespace import, `require`
//!   destructuring, or `new Client()` of an imported constructor). Each call
//!   site carries its enclosing-function attribution for `keel flows suggest`.
//! - **Hosts**: `scheme://host` literals inside string literals and template
//!   quasis. An interpolated scheme (`` `${scheme}://x` ``) is *not* a host —
//!   the quasi has no scheme — which the old regex scan got wrong.
//! - **Relative imports** (raw specifiers), for the module graph the pass
//!   resolves against the scanned file set.
//! - **Per-function attribution** ([`super::FunctionFacts`], for `keel flows
//!   suggest`): a real enclosing-scope stack (`ScanVisitor::scope`) opens one
//!   entry per function bound directly at module top level and credits it
//!   with effects, idempotent-unsafe (POST/PATCH-shaped) effects, time/random
//!   reads, unsafe constructs (`child_process`/`worker_threads`), and
//!   referenced targets — see [`super`]'s module docs for the exact
//!   scope-open policy (class methods and nested functions roll up into the
//!   enclosing top-level entry rather than opening their own).

use std::collections::BTreeMap;

use oxc_allocator::Allocator;
use oxc_ast::ast::{
    Argument, BindingPattern, CallExpression, Class, Expression, Function, ImportDeclaration,
    ImportDeclarationSpecifier, ImportExpression, MethodDefinition, NewExpression,
    ObjectExpression, ObjectProperty, ObjectPropertyKind, PropertyDefinition, StringLiteral,
    TemplateLiteral, VariableDeclarator,
};
use oxc_ast_visit::{Visit, walk};
use oxc_parser::Parser;
use oxc_span::SourceType;
use oxc_syntax::scope::ScopeFlags;

use super::super::{CallSite, FunctionFacts, LangFindings, Sighting, host_from_url};

/// What importing a module specifier evidences.
#[derive(Debug, Clone, Copy)]
struct LibClass {
    /// Library name recorded for `keel doctor`'s adapter-registry cross-check.
    lib: &'static str,
    /// `llm:<provider>` target evidenced by the import, if any.
    provider: Option<&'static str>,
}

/// Classify a module specifier as a known effect library. Subpath imports
/// (`openai/resources`) classify by package name; scoped packages keep both
/// segments. Every classified library is an outbound network client, so a
/// match sets `http_in_use` (the gate that lets URL literals become host
/// targets — same spirit as the Python pass, where a DSN literal plus a DB
/// client import yields the host).
fn classify(specifier: &str) -> Option<LibClass> {
    let pkg = package_name(specifier);
    let lib = |lib| {
        Some(LibClass {
            lib,
            provider: None,
        })
    };
    match pkg {
        // Generic HTTP clients. `node-fetch`/`got`/`superagent`/`axios` have
        // no adapter yet: recording them makes `keel doctor` report them as
        // invisible instead of silently ignoring them.
        "undici" => lib("undici"),
        "node:http" | "node:https" | "http" | "https" => lib("http"),
        "axios" => lib("axios"),
        "got" => lib("got"),
        "node-fetch" => lib("node-fetch"),
        "superagent" => lib("superagent"),
        // Provider SDKs → `llm:*` targets.
        "openai" => Some(LibClass {
            lib: "openai",
            provider: Some("openai"),
        }),
        "anthropic" | "@anthropic-ai/sdk" => Some(LibClass {
            lib: "anthropic",
            provider: Some("anthropic"),
        }),
        // Vercel AI-SDK: the registry lib is `ai-sdk`; a provider package
        // additionally pins the concrete `llm:*` target.
        "ai" => lib("ai-sdk"),
        "@ai-sdk/openai" => Some(LibClass {
            lib: "ai-sdk",
            provider: Some("openai"),
        }),
        "@ai-sdk/anthropic" => Some(LibClass {
            lib: "ai-sdk",
            provider: Some("anthropic"),
        }),
        p if p.starts_with("@ai-sdk/") => lib("ai-sdk"),
        "@modelcontextprotocol/sdk" => lib("mcp"),
        // DB/network clients: no adapter yet → `keel doctor` invisible
        // findings, and their DSN literals become host targets.
        "pg" => lib("pg"),
        "redis" => lib("redis"),
        "ioredis" => lib("ioredis"),
        "mongodb" => lib("mongodb"),
        "mysql2" => lib("mysql2"),
        _ => None,
    }
}

/// The package name of a specifier: both segments for a scoped package, the
/// first segment otherwise. `node:` specifiers pass through unchanged.
fn package_name(specifier: &str) -> &str {
    let seg_end = |from: usize| {
        specifier[from..]
            .find('/')
            .map_or(specifier.len(), |i| from + i)
    };
    if specifier.starts_with('@') {
        let scope_end = seg_end(0);
        if scope_end == specifier.len() {
            return specifier;
        }
        &specifier[..seg_end(scope_end + 1)]
    } else {
        &specifier[..seg_end(0)]
    }
}

/// A local name bound to an effect library.
#[derive(Debug, Clone)]
struct Binding {
    lib: &'static str,
    /// The imported member for a named import (`request` in
    /// `import { request } from "undici"`); `None` for default/namespace
    /// imports and client instances.
    member: Option<String>,
}

/// Per-file scan output that is not part of [`LangFindings`].
#[derive(Debug, Default)]
pub(super) struct FileExtras {
    /// Raw relative import specifiers (`./x`, `../y`), value imports only.
    pub relative_imports: Vec<String>,
    /// Per-top-level-function attribution — see the module docs for the
    /// scope-open policy (which functions get an entry).
    pub functions: Vec<FunctionFacts>,
}

/// Parse and walk one file. Returns `None` when the parser cannot produce a
/// usable AST (the caller warns and skips — a broken file never crashes
/// `keel init`). Findings are appended to `findings` with `rel` as the file.
pub(super) fn scan_source(src: &str, rel: &str, findings: &mut LangFindings) -> Option<FileExtras> {
    let source_type = SourceType::from_path(rel).unwrap_or_else(|_| SourceType::tsx());
    let allocator = Allocator::default();
    let parsed = Parser::new(&allocator, src, source_type).parse();
    // oxc recovers from many errors, but a file that produced diagnostics is
    // not trustworthy evidence — mirror the Python pass: parse cleanly or skip.
    if parsed.panicked || !parsed.diagnostics.is_empty() {
        return None;
    }
    let mut visitor = ScanVisitor {
        rel,
        line_starts: line_starts(src),
        findings,
        extras: FileExtras::default(),
        scope: Vec::new(),
        pending_name: None,
        pending_top_level: false,
        bindings: BTreeMap::new(),
        open_function: None,
    };
    visitor.visit_program(&parsed.program);
    Some(visitor.extras)
}

/// Byte offsets where each line starts, for span → line conversion.
fn line_starts(src: &str) -> Vec<u32> {
    let mut starts = vec![0u32];
    for (i, b) in src.bytes().enumerate() {
        if b == b'\n' {
            starts.push(u32::try_from(i + 1).unwrap_or(u32::MAX));
        }
    }
    starts
}

struct ScanVisitor<'s> {
    rel: &'s str,
    line_starts: Vec<u32>,
    findings: &'s mut LangFindings,
    extras: FileExtras,
    /// Named enclosing scopes (functions, classes, methods).
    scope: Vec<String>,
    /// A name from a declarator/property, waiting for the function or class
    /// expression it names. Only set when the value is directly a
    /// function/arrow/class, so it is always consumed immediately.
    pending_name: Option<String>,
    /// Whether `pending_name` came from a direct top-level `const`/`let`/`var
    /// NAME = function/arrow` binding (as opposed to an object-literal
    /// property or class member value) — the only shape eligible to open a
    /// top-level [`FunctionFacts`] entry. Consumed alongside `pending_name`.
    pending_top_level: bool,
    /// Local names bound to effect libraries.
    bindings: BTreeMap<String, Binding>,
    /// Index into `extras.functions` of the top-level function currently
    /// being attributed, if any — set when scope depth goes 0→1 via a
    /// function/arrow (not class/method) push, cleared when scope unwinds
    /// back to empty. Nested scopes (inner functions, class methods) keep
    /// crediting whichever top-level entry is already open.
    open_function: Option<usize>,
}

impl ScanVisitor<'_> {
    /// 1-based line of a byte offset.
    fn line_of(&self, offset: u32) -> u32 {
        let idx = self.line_starts.partition_point(|&s| s <= offset);
        u32::try_from(idx).unwrap_or(u32::MAX)
    }

    fn sighting(&self, offset: u32) -> Sighting {
        Sighting {
            file: self.rel.to_owned(),
            line: self.line_of(offset),
        }
    }

    /// Record the evidence an effect-library import carries.
    fn record_lib(&mut self, class: LibClass, offset: u32) {
        self.findings.libs.insert(class.lib.to_owned());
        // Every classified library is an outbound network client, so its
        // import gates URL literals into host targets.
        self.findings.http_in_use = true;
        if let Some(provider) = class.provider {
            if let Some(idx) = self.open_function {
                self.extras.functions[idx]
                    .targets
                    .insert(format!("llm:{provider}"));
            }
            let sighting = self.sighting(offset);
            self.findings.llm.push((provider.to_owned(), sighting));
        }
    }

    /// Record one effect call site with its enclosing-function attribution,
    /// crediting the open top-level [`FunctionFacts`] entry (if any) with one
    /// effect.
    fn record_call_site(&mut self, callee: String, offset: u32) {
        let function = if self.scope.is_empty() {
            None
        } else {
            Some(self.scope.join("."))
        };
        if let Some(idx) = self.open_function {
            self.extras.functions[idx].effects += 1;
        }
        self.findings.call_sites.push(CallSite {
            file: self.rel.to_owned(),
            line: self.line_of(offset),
            callee,
            function,
        });
    }

    /// Extract every host from a literal text fragment, crediting the open
    /// top-level function's targets.
    fn record_hosts(&mut self, text: &str, offset: u32) {
        for host in hosts_in_text(text) {
            if let Some(idx) = self.open_function {
                self.extras.functions[idx].targets.insert(host.clone());
            }
            let sighting = self.sighting(offset);
            self.findings.hosts.push((host, sighting));
        }
    }

    /// Handle a static/dynamic/`require` import of `specifier`. `child_process`
    /// and `worker_threads` are not effect libraries (no `classify` match —
    /// they are not network clients) but they defeat replay outright, so they
    /// are checked here as a separate, additional signal that does not affect
    /// `http_in_use`/`libs`.
    fn record_import(&mut self, specifier: &str, offset: u32) -> Option<LibClass> {
        if specifier.starts_with("./") || specifier.starts_with("../") {
            self.extras.relative_imports.push(specifier.to_owned());
            return None;
        }
        if matches!(specifier, "child_process" | "worker_threads")
            && let Some(idx) = self.open_function
        {
            let line = self.line_of(offset);
            self.extras.functions[idx]
                .unsafe_reasons
                .push(format!("{specifier} use at {}:{line}", self.rel));
        }
        let class = classify(specifier)?;
        self.record_lib(class, offset);
        Some(class)
    }

    /// Credit the open top-level function (if any) with one idempotent-unsafe
    /// effect.
    fn mark_idempotent_unsafe(&mut self) {
        if let Some(idx) = self.open_function {
            self.extras.functions[idx].idempotent_unsafe += 1;
        }
    }

    /// Credit the open top-level function (if any) with one wall-clock read.
    fn mark_time_read(&mut self) {
        if let Some(idx) = self.open_function {
            self.extras.functions[idx].time_reads += 1;
        }
    }

    /// Credit the open top-level function (if any) with one randomness read.
    fn mark_random_read(&mut self) {
        if let Some(idx) = self.open_function {
            self.extras.functions[idx].random_reads += 1;
        }
    }

    /// Open a new top-level [`FunctionFacts`] entry for `name`, run `inner`
    /// with it as the attribution target, then close it. Only called when a
    /// function/arrow is bound directly at module top level (scope empty
    /// before this push) — see the module docs for the full policy.
    fn with_top_level_function(
        &mut self,
        name: String,
        offset: u32,
        inner: impl FnOnce(&mut Self),
    ) {
        let line = self.line_of(offset);
        self.extras.functions.push(FunctionFacts {
            entrypoint: format!("ts:{}#{name}", self.rel),
            file: self.rel.to_owned(),
            line,
            ..FunctionFacts::default()
        });
        self.open_function = Some(self.extras.functions.len() - 1);
        self.scope.push(name);
        inner(self);
        self.scope.pop();
        self.open_function = None;
    }

    /// Bind the names a binding pattern introduces for a value of `lib`
    /// (a `require(…)` result or a client instance): a plain identifier is a
    /// whole-module/instance binding, an object pattern binds each property
    /// as a member.
    fn bind_pattern(&mut self, pattern: &BindingPattern<'_>, lib: &'static str) {
        match pattern {
            BindingPattern::BindingIdentifier(ident) => {
                self.bindings.insert(
                    ident.name.as_str().to_owned(),
                    Binding { lib, member: None },
                );
            }
            BindingPattern::ObjectPattern(obj) => {
                for prop in &obj.properties {
                    let (Some(name), Some(local)) =
                        (prop.key.static_name(), prop.value.get_binding_identifier())
                    else {
                        continue;
                    };
                    self.bindings.insert(
                        local.name.as_str().to_owned(),
                        Binding {
                            lib,
                            member: Some(name.into_owned()),
                        },
                    );
                }
            }
            _ => {}
        }
    }

    /// The effect-library callee string for a call, if its receiver traces to
    /// a binding: `request(…)` from `import { request } from "undici"` →
    /// `undici.request`; `client.chat.completions.create(…)` where `client`
    /// is an OpenAI instance → `openai.chat.completions.create`.
    fn bound_callee(&self, chain: &[String]) -> Option<String> {
        let binding = self.bindings.get(chain.first()?)?;
        let mut parts = vec![binding.lib.to_owned()];
        parts.extend(binding.member.clone());
        parts.extend(chain.iter().skip(1).cloned());
        Some(parts.join("."))
    }

    /// Push a scope name (when known), walk `inner`, pop.
    fn scoped(&mut self, name: Option<String>, inner: impl FnOnce(&mut Self)) {
        let pushed = name.is_some();
        if let Some(name) = name {
            self.scope.push(name);
        }
        inner(self);
        if pushed {
            self.scope.pop();
        }
    }
}

/// The static identifier chain of an expression: `a.b.c` → `["a","b","c"]`.
/// `None` for anything dynamic (computed members, calls, etc.).
fn static_chain(expr: &Expression<'_>) -> Option<Vec<String>> {
    match expr {
        Expression::Identifier(ident) => Some(vec![ident.name.as_str().to_owned()]),
        Expression::StaticMemberExpression(member) => {
            let mut chain = static_chain(&member.object)?;
            chain.push(member.property.name.as_str().to_owned());
            Some(chain)
        }
        _ => None,
    }
}

/// Is this expression a function-valued thing a declarator/property can name?
fn names_a_function(expr: &Expression<'_>) -> bool {
    matches!(
        expr,
        Expression::FunctionExpression(_) | Expression::ArrowFunctionExpression(_)
    )
}

/// `require("<specifier>")` → the specifier.
fn require_specifier<'a>(call: &'a CallExpression<'_>) -> Option<&'a str> {
    if !matches!(&call.callee, Expression::Identifier(id) if id.name == "require") {
        return None;
    }
    match call.arguments.as_slice() {
        [Argument::StringLiteral(s)] => Some(s.value.as_str()),
        _ => None,
    }
}

/// Does `call`'s argument list contain an object literal with a static
/// `method` property whose value is the string literal `"POST"` or
/// `"PATCH"`? The AST-proper version of the old line heuristic's same-line
/// method-literal check.
fn has_unsafe_method_argument(call: &CallExpression<'_>) -> bool {
    call.arguments.iter().any(|arg| {
        let Argument::ObjectExpression(obj) = arg else {
            return false;
        };
        object_has_unsafe_method(obj)
    })
}

/// Does this object literal have a static `method: "POST" | "PATCH"` property?
fn object_has_unsafe_method(obj: &ObjectExpression<'_>) -> bool {
    obj.properties.iter().any(|prop| {
        let ObjectPropertyKind::ObjectProperty(p) = prop else {
            return false;
        };
        p.key.static_name().as_deref() == Some("method")
            && matches!(&p.value, Expression::StringLiteral(s) if matches!(s.value.as_str(), "POST" | "PATCH"))
    })
}

/// Is this static callee chain a wall-clock read Tier 2 virtualizes on
/// replay: `Date.now`, `performance.now`? (`new Date()` is handled separately
/// in `visit_new_expression`, since it is not a call.)
fn is_time_chain(chain: &[String]) -> bool {
    match chain {
        [a, b] => (a == "Date" || a == "performance") && b == "now",
        _ => false,
    }
}

/// Is this static callee chain a randomness read Tier 2 virtualizes on
/// replay: `Math.random`, `crypto.randomUUID`, `crypto.getRandomValues`?
fn is_random_chain(chain: &[String]) -> bool {
    match chain {
        [a, b] => matches!(
            (a.as_str(), b.as_str()),
            ("Math", "random") | ("crypto", "randomUUID" | "getRandomValues")
        ),
        _ => false,
    }
}

impl<'a> Visit<'a> for ScanVisitor<'_> {
    fn visit_import_declaration(&mut self, it: &ImportDeclaration<'a>) {
        // `import type … from "openai"` is erased at runtime: not evidence.
        if it.import_kind.is_type() {
            return;
        }
        let Some(class) = self.record_import(it.source.value.as_str(), it.span.start) else {
            return;
        };
        let Some(specifiers) = &it.specifiers else {
            return;
        };
        for specifier in specifiers {
            match specifier {
                ImportDeclarationSpecifier::ImportSpecifier(s) => {
                    if s.import_kind.is_type() {
                        continue; // `import { type Foo }` — erased.
                    }
                    self.bindings.insert(
                        s.local.name.as_str().to_owned(),
                        Binding {
                            lib: class.lib,
                            member: Some(s.imported.name().as_str().to_owned()),
                        },
                    );
                }
                ImportDeclarationSpecifier::ImportDefaultSpecifier(s) => {
                    self.bindings.insert(
                        s.local.name.as_str().to_owned(),
                        Binding {
                            lib: class.lib,
                            member: None,
                        },
                    );
                }
                ImportDeclarationSpecifier::ImportNamespaceSpecifier(s) => {
                    self.bindings.insert(
                        s.local.name.as_str().to_owned(),
                        Binding {
                            lib: class.lib,
                            member: None,
                        },
                    );
                }
            }
        }
    }

    fn visit_import_expression(&mut self, it: &ImportExpression<'a>) {
        if let Expression::StringLiteral(source) = &it.source {
            self.record_import(source.value.as_str(), it.span.start);
        }
        walk::walk_import_expression(self, it);
    }

    fn visit_call_expression(&mut self, it: &CallExpression<'a>) {
        if let Some(specifier) = require_specifier(it) {
            // A `require` is an import, not an effect call — no call site.
            self.record_import(specifier, it.span.start);
        } else if let Some(chain) = static_chain(&it.callee) {
            if let Some(callee) = self.bound_callee(&chain) {
                self.record_call_site(callee, it.span.start);
                if has_unsafe_method_argument(it) {
                    self.mark_idempotent_unsafe();
                }
            } else if chain.last().is_some_and(|last| last == "fetch") {
                // Global `fetch(…)` — or any `x.fetch(…)`, matching the old
                // scan's accepted looseness (`globalThis.fetch`, `this.fetch`).
                self.findings.http_in_use = true;
                self.findings.libs.insert("fetch".to_owned());
                self.record_call_site("fetch".to_owned(), it.span.start);
                if has_unsafe_method_argument(it) {
                    self.mark_idempotent_unsafe();
                }
            } else if is_time_chain(&chain) {
                self.mark_time_read();
            } else if is_random_chain(&chain) {
                self.mark_random_read();
            }
        }
        walk::walk_call_expression(self, it);
    }

    fn visit_new_expression(&mut self, it: &NewExpression<'a>) {
        // Bare `new Date()` — a wall-clock read Tier 2 virtualizes on replay,
        // same as `Date.now()`.
        if let Some(chain) = static_chain(&it.callee)
            && chain.as_slice() == [String::from("Date")]
        {
            self.mark_time_read();
        }
        walk::walk_new_expression(self, it);
    }

    fn visit_string_literal(&mut self, it: &StringLiteral<'a>) {
        self.record_hosts(it.value.as_str(), it.span.start);
    }

    fn visit_template_literal(&mut self, it: &TemplateLiteral<'a>) {
        for quasi in &it.quasis {
            let text = quasi
                .value
                .cooked
                .as_ref()
                .map_or_else(|| quasi.value.raw.as_str(), |cooked| cooked.as_str());
            self.record_hosts(text, quasi.span.start);
        }
        walk::walk_template_literal(self, it);
    }

    fn visit_variable_declarator(&mut self, it: &VariableDeclarator<'a>) {
        match &it.init {
            // `const u = require("undici")` / `const { request } = require(…)`
            Some(Expression::CallExpression(call)) => {
                if let Some(class) = require_specifier(call).and_then(classify) {
                    self.bind_pattern(&it.id, class.lib);
                }
            }
            // `const client = new OpenAI()` — the instance carries the lib.
            Some(Expression::NewExpression(new_expr)) => {
                if let Some(chain) = static_chain(&new_expr.callee)
                    && let Some(binding) = chain.first().and_then(|n| self.bindings.get(n))
                {
                    let lib = binding.lib;
                    self.bind_pattern(&it.id, lib);
                }
            }
            // `const handler = () => …` — name the function after the binding.
            // Only eligible to open a top-level `FunctionFacts` entry when
            // this declarator itself sits at module top level (empty scope);
            // a nested `const inner = () => {}` inside another function
            // still names `inner` for `CallSite` attribution, but does not
            // open its own entry.
            Some(init) if names_a_function(init) => {
                self.pending_name = it
                    .id
                    .get_binding_identifier()
                    .map(|ident| ident.name.as_str().to_owned());
                self.pending_top_level = self.pending_name.is_some() && self.scope.is_empty();
            }
            _ => {}
        }
        walk::walk_variable_declarator(self, it);
    }

    fn visit_object_property(&mut self, it: &ObjectProperty<'a>) {
        if names_a_function(&it.value) {
            self.pending_name = it.key.static_name().map(std::borrow::Cow::into_owned);
            // An object-literal method/property value is never "bound
            // directly at module top level", even when the enclosing object
            // itself is a top-level const — it does not open its own entry.
            self.pending_top_level = false;
        }
        walk::walk_object_property(self, it);
    }

    fn visit_property_definition(&mut self, it: &PropertyDefinition<'a>) {
        if it.value.as_ref().is_some_and(names_a_function) {
            self.pending_name = it.key.static_name().map(std::borrow::Cow::into_owned);
            // A class field's function value is a class member, never a
            // top-level entry.
            self.pending_top_level = false;
        }
        walk::walk_property_definition(self, it);
    }

    fn visit_function(&mut self, it: &Function<'a>, flags: ScopeFlags) {
        let own_name = it.name().map(|n| n.as_str().to_owned());
        let top_level = self.scope.is_empty() && (own_name.is_some() || self.pending_top_level);
        let name = own_name.or_else(|| self.pending_name.take());
        self.pending_top_level = false;
        match name {
            Some(name) if top_level => {
                self.with_top_level_function(name, it.span.start, |v| {
                    walk::walk_function(v, it, flags);
                });
            }
            name => self.scoped(name, |v| walk::walk_function(v, it, flags)),
        }
    }

    fn visit_arrow_function_expression(&mut self, it: &oxc_ast::ast::ArrowFunctionExpression<'a>) {
        let top_level = self.scope.is_empty() && self.pending_top_level;
        let name = self.pending_name.take();
        self.pending_top_level = false;
        match name {
            Some(name) if top_level => {
                self.with_top_level_function(name, it.span.start, |v| {
                    walk::walk_arrow_function_expression(v, it);
                });
            }
            name => self.scoped(name, |v| walk::walk_arrow_function_expression(v, it)),
        }
    }

    fn visit_class(&mut self, it: &Class<'a>) {
        let name = it
            .id
            .as_ref()
            .map(|id| id.name.as_str().to_owned())
            .or_else(|| self.pending_name.take());
        self.scoped(name, |v| walk::walk_class(v, it));
    }

    fn visit_method_definition(&mut self, it: &MethodDefinition<'a>) {
        let name = it.key.static_name().map(std::borrow::Cow::into_owned);
        self.scoped(name, |v| walk::walk_method_definition(v, it));
    }
}

/// Every `scheme://host` host named inside a literal text fragment. Same
/// tolerant embedded-URL walk the regex scan used, now applied to real
/// literal values instead of raw source lines.
fn hosts_in_text(text: &str) -> Vec<String> {
    let mut hosts = Vec::new();
    let bytes = text.as_bytes();
    let mut i = 0;
    while let Some(rel) = text[i..].find("://") {
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
        // the URL runs until a quote/backtick/whitespace/closing paren.
        let tail_start = scheme_end + 3;
        let end = text[tail_start..]
            .find(|c: char| c == '"' || c == '\'' || c == '`' || c.is_whitespace() || c == ')')
            .map_or(text.len(), |off| tail_start + off);
        let candidate = &text[start..end];
        if let Some(host) = host_from_url(candidate) {
            hosts.push(host);
        }
        i = end.max(scheme_end + 3);
    }
    hosts
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn package_name_handles_scopes_and_subpaths() {
        assert_eq!(package_name("openai"), "openai");
        assert_eq!(package_name("openai/resources"), "openai");
        assert_eq!(package_name("@anthropic-ai/sdk"), "@anthropic-ai/sdk");
        assert_eq!(
            package_name("@anthropic-ai/sdk/resources"),
            "@anthropic-ai/sdk"
        );
        assert_eq!(package_name("@scope"), "@scope");
        assert_eq!(package_name("node:https"), "node:https");
    }

    #[test]
    fn interpolated_scheme_is_not_a_host() {
        // `${scheme}://internal` — the quasi is "://internal": no scheme.
        assert!(hosts_in_text("://internal").is_empty());
    }

    #[test]
    fn embedded_url_in_text_is_found() {
        assert_eq!(
            hosts_in_text("see https://api.example.com/v1 for details"),
            ["api.example.com"]
        );
    }
}
