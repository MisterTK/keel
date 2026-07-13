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

use std::collections::BTreeMap;

use oxc_allocator::Allocator;
use oxc_ast::ast::{
    Argument, BindingPattern, CallExpression, Class, Expression, Function, ImportDeclaration,
    ImportDeclarationSpecifier, ImportExpression, MethodDefinition, ObjectProperty,
    PropertyDefinition, StringLiteral, TemplateLiteral, VariableDeclarator,
};
use oxc_ast_visit::{Visit, walk};
use oxc_parser::Parser;
use oxc_span::SourceType;
use oxc_syntax::scope::ScopeFlags;

use super::super::{CallSite, LangFindings, Sighting, host_from_url};

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
        bindings: BTreeMap::new(),
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
    /// Local names bound to effect libraries.
    bindings: BTreeMap<String, Binding>,
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
            let sighting = self.sighting(offset);
            self.findings.llm.push((provider.to_owned(), sighting));
        }
    }

    /// Record one effect call site with its enclosing-function attribution.
    fn record_call_site(&mut self, callee: String, offset: u32) {
        let function = if self.scope.is_empty() {
            None
        } else {
            Some(self.scope.join("."))
        };
        self.findings.call_sites.push(CallSite {
            file: self.rel.to_owned(),
            line: self.line_of(offset),
            callee,
            function,
        });
    }

    /// Extract every host from a literal text fragment.
    fn record_hosts(&mut self, text: &str, offset: u32) {
        for host in hosts_in_text(text) {
            let sighting = self.sighting(offset);
            self.findings.hosts.push((host, sighting));
        }
    }

    /// Handle a static/dynamic/`require` import of `specifier`.
    fn record_import(&mut self, specifier: &str, offset: u32) -> Option<LibClass> {
        if specifier.starts_with("./") || specifier.starts_with("../") {
            self.extras.relative_imports.push(specifier.to_owned());
            return None;
        }
        let class = classify(specifier)?;
        self.record_lib(class, offset);
        Some(class)
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
            } else if chain.last().is_some_and(|last| last == "fetch") {
                // Global `fetch(…)` — or any `x.fetch(…)`, matching the old
                // scan's accepted looseness (`globalThis.fetch`, `this.fetch`).
                self.findings.http_in_use = true;
                self.findings.libs.insert("fetch".to_owned());
                self.record_call_site("fetch".to_owned(), it.span.start);
            }
        }
        walk::walk_call_expression(self, it);
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
            Some(init) if names_a_function(init) => {
                self.pending_name = it
                    .id
                    .get_binding_identifier()
                    .map(|ident| ident.name.as_str().to_owned());
            }
            _ => {}
        }
        walk::walk_variable_declarator(self, it);
    }

    fn visit_object_property(&mut self, it: &ObjectProperty<'a>) {
        if names_a_function(&it.value) {
            self.pending_name = it.key.static_name().map(std::borrow::Cow::into_owned);
        }
        walk::walk_object_property(self, it);
    }

    fn visit_property_definition(&mut self, it: &PropertyDefinition<'a>) {
        if it.value.as_ref().is_some_and(names_a_function) {
            self.pending_name = it.key.static_name().map(std::borrow::Cow::into_owned);
        }
        walk::walk_property_definition(self, it);
    }

    fn visit_function(&mut self, it: &Function<'a>, flags: ScopeFlags) {
        let name = it
            .name()
            .map(|n| n.as_str().to_owned())
            .or_else(|| self.pending_name.take());
        self.scoped(name, |v| walk::walk_function(v, it, flags));
    }

    fn visit_arrow_function_expression(&mut self, it: &oxc_ast::ast::ArrowFunctionExpression<'a>) {
        let name = self.pending_name.take();
        self.scoped(name, |v| walk::walk_arrow_function_expression(v, it));
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
