//! `keel doctor --effective-policy` — show the composed policy the core is
//! given (the defaults/E005 CCR: front ends and CLI compose
//! `defaults < packs < user` *before* `keel_configure`; the core layers no
//! pack underneath, `contracts/core-ffi.h`).
//!
//! [`effective_policy`] is the Rust twin of Python's
//! `keel._defaults.apply_pack_defaults` and Node's
//! `defaults.mjs::applyPackDefaults`: merge granularity is per KEY of
//! `defaults.outbound` / `defaults.llm` (a higher layer replaces a key it sets
//! wholesale, fills in the rest), and target tables pass through untouched —
//! the engine resolves target → defaults precedence per key at execute time.
//! The three implementations must agree byte-for-byte; the cross-language
//! parity test in `tests/cli.rs` runs all three over the same fixture.
//!
//! The Level 0 layer is the frozen smart-defaults pack itself,
//! `contracts/defaults.toml`, embedded with `include_str!` so the binary and
//! the contract can never drift ("this document ships compiled into the
//! binary"). The printed policy is the PRE-resolution merge — `cache = { mode
//! = "dev" }` stays symbolic (the front ends resolve it against `KEEL_ENV` at
//! run time), keeping the `--json` twin byte-deterministic.

use std::collections::{BTreeMap, BTreeSet};
use std::path::Path;
use std::sync::OnceLock;

use keel_core_api::policy::Policy;
use serde::Serialize;
use serde_json::{Map, Value};

use crate::render::to_json;
use crate::{EXIT_OK, EXIT_USAGE, Rendered, evidence, scan};

/// The frozen smart-defaults pack, compiled in (contracts/defaults.toml).
const DEFAULTS_TOML: &str = include_str!("../../../contracts/defaults.toml");

/// Canonical key order inside a policy layer table (the order the contract
/// file and `keel init` write); keys outside this list follow, sorted.
const LAYER_KEY_ORDER: [&str; 5] = ["timeout", "retry", "breaker", "rate", "cache"];

/// The run-time meaning of a `mode = "dev"` cache, stated so the printed
/// pre-resolution merge cannot mislead (dx-spec honesty).
const DEV_CACHE_NOTE: &str = "cache mode \"dev\" is resolved by the front end at run time \
     (a concrete ttl off-prod; dropped when KEEL_ENV=prod) \u{2014} the policy shown is the \
     pre-resolution merge.";

/// One pack the CLI can detect statically (via the scan's import evidence),
/// with the policy fragment it folds into the `packs` merge layer. Fragments
/// mirror the front ends: the provider packs (`openai`, `anthropic`) and the
/// AI-SDK middleware emit the generic `[defaults.llm]` layer; `mcp:` targets
/// inherit `[defaults.outbound]` and contribute no fragment of their own.
struct Pack {
    lib: &'static str,
    fragment: fn() -> Value,
}

/// Registration order = report order (matches the front ends' provider packs
/// first, then the Node-only packs) — stable, deterministic.
const PACKS: &[Pack] = &[
    Pack {
        lib: "openai",
        fragment: llm_pack_fragment,
    },
    Pack {
        lib: "anthropic",
        fragment: llm_pack_fragment,
    },
    Pack {
        lib: "ai-sdk",
        fragment: llm_pack_fragment,
    },
    Pack {
        lib: "mcp",
        fragment: empty_fragment,
    },
];

/// The `[defaults]` table of the embedded contract, parsed once.
fn level0() -> &'static Map<String, Value> {
    static LEVEL0: OnceLock<Map<String, Value>> = OnceLock::new();
    LEVEL0.get_or_init(|| {
        let toml_value: toml::Value = DEFAULTS_TOML
            .parse()
            .expect("contracts/defaults.toml is valid TOML");
        let json = serde_json::to_value(&toml_value).expect("defaults normalize to JSON");
        json.get("defaults")
            .and_then(Value::as_object)
            .cloned()
            .expect("contracts/defaults.toml has a [defaults] table")
    })
}

/// One embedded Level 0 layer (`outbound` or `llm`) as an owned table.
fn level0_layer(layer: &str) -> Map<String, Value> {
    level0()
        .get(layer)
        .and_then(Value::as_object)
        .cloned()
        .unwrap_or_default()
}

/// The generic `[defaults.llm]` pack fragment — what the provider packs and
/// the AI-SDK middleware contribute (Python `provider_defaults()`, Node
/// `llmPack.defaults()`). Folding it is the identity over the embedded
/// defaults, by construction.
pub fn llm_pack_fragment() -> Value {
    let mut defaults = Map::new();
    defaults.insert("llm".to_owned(), Value::Object(level0_layer("llm")));
    let mut frag = Map::new();
    frag.insert("defaults".to_owned(), Value::Object(defaults));
    Value::Object(frag)
}

/// The empty fragment (`mcp:` targets inherit `[defaults.outbound]`).
fn empty_fragment() -> Value {
    Value::Object(Map::new())
}

/// A JSON value's table view, or empty — the twin of Python's `_table` /
/// Node's `isTable` guards.
fn table(v: Option<&Value>) -> Map<String, Value> {
    v.and_then(Value::as_object).cloned().unwrap_or_default()
}

/// Fold the pack fragments' `defaults.outbound` / `defaults.llm` tables, later
/// fragments overriding earlier per key (Python's `pack_outbound.update(...)`).
fn pack_layers(fragments: &[Value]) -> (Map<String, Value>, Map<String, Value>) {
    let mut outbound = Map::new();
    let mut llm = Map::new();
    for frag in fragments {
        let frag_defaults = table(frag.get("defaults"));
        outbound.extend(table(frag_defaults.get("outbound")));
        llm.extend(table(frag_defaults.get("llm")));
    }
    (outbound, llm)
}

/// Compose the effective policy: `defaults < packs < user`, per-key wholesale
/// on the `defaults.outbound` / `defaults.llm` tables, target tables untouched.
/// Returns a NEW value; the input is never borrowed mutably. Idempotent on the
/// embedded Level 0 policy. Byte-identical (as sorted JSON) to Python
/// `apply_pack_defaults` and Node `applyPackDefaults` over the same inputs.
pub fn effective_policy(user: &Value, fragments: &[Value]) -> Value {
    let mut out = user.as_object().cloned().unwrap_or_default();
    let user_defaults = table(out.get("defaults"));
    let (pack_outbound, pack_llm) = pack_layers(fragments);

    let mut outbound = level0_layer("outbound");
    outbound.extend(pack_outbound);
    outbound.extend(table(user_defaults.get("outbound")));

    let mut llm = level0_layer("llm");
    llm.extend(pack_llm);
    llm.extend(table(user_defaults.get("llm")));

    let mut defaults = user_defaults;
    defaults.insert("outbound".to_owned(), Value::Object(outbound));
    defaults.insert("llm".to_owned(), Value::Object(llm));
    out.insert("defaults".to_owned(), Value::Object(defaults));
    Value::Object(out)
}

/// Per-key provenance for the two merged layers: which layer WON each
/// `defaults.outbound.*` / `defaults.llm.*` key — `"user"` if the user set it,
/// else `"pack"` if a detected pack fragment set it, else `"defaults"`.
/// (A pack that re-affirms a default still wins the key, exactly as the merge
/// itself resolves it.)
fn layer_sources(user: &Value, fragments: &[Value]) -> BTreeMap<String, &'static str> {
    let user_defaults = table(user.get("defaults"));
    let (pack_outbound, pack_llm) = pack_layers(fragments);
    let mut sources = BTreeMap::new();
    for (layer, pack_keys) in [("outbound", &pack_outbound), ("llm", &pack_llm)] {
        let user_keys = table(user_defaults.get(layer));
        let mut merged = level0_layer(layer);
        merged.extend(pack_keys.clone());
        merged.extend(user_keys.clone());
        for key in merged.keys() {
            let source = if user_keys.contains_key(key) {
                "user"
            } else if pack_keys.contains_key(key) {
                "pack"
            } else {
                "defaults"
            };
            sources.insert(format!("defaults.{layer}.{key}"), source);
        }
    }
    sources
}

/// Whether any layer of the composed policy still carries the symbolic
/// dev-loop cache (`cache = { mode = "dev" }`) — the layers the front ends'
/// dev-cache resolution walks: `defaults.llm`, `defaults.outbound`, targets.
fn has_dev_cache(policy: &Value) -> bool {
    let defaults = table(policy.get("defaults"));
    if cache_is_dev(defaults.get("llm")) || cache_is_dev(defaults.get("outbound")) {
        return true;
    }
    table(policy.get("target"))
        .values()
        .any(|t| cache_is_dev(Some(t)))
}

fn cache_is_dev(layer: Option<&Value>) -> bool {
    layer
        .and_then(|l| l.get("cache"))
        .and_then(|c| c.get("mode"))
        .and_then(Value::as_str)
        == Some("dev")
}

/// The whole `--effective-policy` report. `policy` is THE effective policy —
/// the object handed to `keel_configure`, and the object the cross-language
/// parity golden pins.
#[derive(Debug, Serialize)]
struct EffectiveReport {
    notes: Vec<&'static str>,
    packs: Vec<&'static str>,
    policy: Value,
    sources: BTreeMap<String, &'static str>,
    user_policy_present: bool,
}

/// Run `keel doctor --effective-policy` for `project`.
pub fn run(project: &Path) -> Rendered {
    let user = match load_user_policy(&evidence::keel_toml(project)) {
        UserPolicy::Invalid { field, message } => {
            return invalid_rendered(field.as_deref(), &message);
        }
        UserPolicy::Absent => None,
        UserPolicy::Valid(v) => Some(v),
    };
    let scanned = scan::scan(project);
    let report = build_report(&scanned.libs, user);
    let human = human(&report);
    Rendered::ok(human, to_json(&report)).with_exit(EXIT_OK)
}

/// Assemble the report from the detected import evidence and the (validated)
/// user policy. Pure, so tests pin it without a filesystem.
fn build_report(libs: &BTreeSet<String>, user: Option<Value>) -> EffectiveReport {
    let detected: Vec<&Pack> = PACKS.iter().filter(|p| libs.contains(p.lib)).collect();
    let fragments: Vec<Value> = detected.iter().map(|p| (p.fragment)()).collect();
    let user_policy_present = user.is_some();
    let user = user.unwrap_or(Value::Object(Map::new()));

    let policy = effective_policy(&user, &fragments);
    let sources = layer_sources(&user, &fragments);
    let notes = if has_dev_cache(&policy) {
        vec![DEV_CACHE_NOTE]
    } else {
        Vec::new()
    };
    EffectiveReport {
        notes,
        packs: detected.iter().map(|p| p.lib).collect(),
        policy,
        sources,
        user_policy_present,
    }
}

/// The user's `keel.toml`, loaded and validated against the typed
/// [`Policy`] model (exact field path on error, via `serde_path_to_error`).
enum UserPolicy {
    Absent,
    Valid(Value),
    Invalid {
        field: Option<String>,
        message: String,
    },
}

fn load_user_policy(path: &Path) -> UserPolicy {
    if !path.exists() {
        return UserPolicy::Absent;
    }
    let Ok(text) = std::fs::read_to_string(path) else {
        return UserPolicy::Invalid {
            field: None,
            message: "keel.toml exists but could not be read".to_owned(),
        };
    };
    let toml_value: toml::Value = match text.parse() {
        Ok(v) => v,
        Err(e) => {
            return UserPolicy::Invalid {
                field: None,
                message: format!("keel.toml is not valid TOML: {e}"),
            };
        }
    };
    let json_value = match serde_json::to_value(&toml_value) {
        Ok(v) => v,
        Err(e) => {
            return UserPolicy::Invalid {
                field: None,
                message: format!("keel.toml could not be normalized: {e}"),
            };
        }
    };
    match serde_path_to_error::deserialize::<_, Policy>(&json_value) {
        Ok(_) => UserPolicy::Valid(json_value),
        Err(e) => UserPolicy::Invalid {
            field: Some(e.path().to_string()),
            message: e.inner().to_string(),
        },
    }
}

/// The error result for an invalid `keel.toml`: no effective policy exists to
/// compose, so this is a usage error (KEEL-E001, the frozen policy-invalid
/// code) on stderr.
fn invalid_rendered(field: Option<&str>, message: &str) -> Rendered {
    let at = field.unwrap_or("(document)");
    let human = format!(
        "keel \u{25b8} doctor --effective-policy: keel.toml is invalid (KEEL-E001), so there is \
         no effective policy to compose.\n  at `{at}`: {message}\n  \u{2192} Fix the field and \
         re-run; `keel explain KEEL-E001` has the contract, `keel doctor` the full report.\n"
    );
    let json = to_json(&serde_json::json!({
        "code": "KEEL-E001",
        "error": message,
        "field": field,
    }));
    Rendered::ok(human, json).with_exit(EXIT_USAGE).on_stderr()
}

// ---- human rendering: the merged policy as annotated TOML ----------------

/// A TOML basic string.
fn toml_string(s: &str) -> String {
    format!("\"{}\"", s.replace('\\', "\\\\").replace('"', "\\\""))
}

/// A TOML key: bare when possible, quoted otherwise.
fn toml_key(k: &str) -> String {
    let bare = !k.is_empty()
        && k.bytes()
            .all(|b| b.is_ascii_alphanumeric() || b == b'_' || b == b'-');
    if bare { k.to_owned() } else { toml_string(k) }
}

/// Render a JSON value as a TOML value (inline tables, sorted keys — the JSON
/// map is a `BTreeMap`, so the output is deterministic). `Null` cannot reach
/// here: every merge input is TOML-parsed or contract-built.
fn toml_value(v: &Value) -> String {
    match v {
        Value::String(s) => toml_string(s),
        Value::Bool(b) => b.to_string(),
        Value::Number(n) => n.to_string(),
        Value::Null => "null".to_owned(),
        Value::Array(items) => {
            let inner: Vec<String> = items.iter().map(toml_value).collect();
            format!("[{}]", inner.join(", "))
        }
        Value::Object(m) => {
            if m.is_empty() {
                "{}".to_owned()
            } else {
                let inner: Vec<String> = m
                    .iter()
                    .map(|(k, v)| format!("{} = {}", toml_key(k), toml_value(v)))
                    .collect();
                format!("{{ {} }}", inner.join(", "))
            }
        }
    }
}

/// Layer keys in canonical order (timeout, retry, breaker, rate, cache), then
/// anything else sorted.
fn ordered_keys(m: &Map<String, Value>) -> Vec<&String> {
    let mut keys: Vec<&String> = LAYER_KEY_ORDER
        .iter()
        .filter_map(|k| m.get_key_value(*k).map(|(k, _)| k))
        .collect();
    keys.extend(m.keys().filter(|k| !LAYER_KEY_ORDER.contains(&k.as_str())));
    keys
}

/// One `[section]` of the annotated TOML: `header_line` is printed verbatim,
/// each key gets the annotation `annotate` returns (aligned on the key column).
fn render_table(
    out: &mut String,
    header_line: &str,
    m: &Map<String, Value>,
    annotate: &dyn Fn(&str) -> Option<&'static str>,
) {
    let header = format!("\n{header_line}\n");
    out.push_str(&header);
    let width = m.keys().map(String::len).max().unwrap_or(0);
    for key in ordered_keys(m) {
        let value = toml_value(&m[key.as_str()]);
        let line = match annotate(key) {
            Some(source) => format!("{key:<width$} = {value}  # {source}\n"),
            None => format!("{key:<width$} = {value}\n"),
        };
        out.push_str(&line);
    }
}

/// The human report: the merged policy as TOML, every `defaults` key annotated
/// with the layer that won it — so no fact escapes the JSON twin.
fn human(r: &EffectiveReport) -> String {
    let mut out = String::from("keel \u{25b8} doctor --effective-policy\n\n");
    out.push_str(
        "The composed policy `keel_configure` receives \u{2014} defaults < packs < user,\n\
         merged per key; the core layers no pack underneath.\n",
    );
    out.push_str("  defaults  embedded contracts/defaults.toml (Level 0)\n");
    let packs = if r.packs.is_empty() {
        "(none detected)".to_owned()
    } else {
        r.packs.join(", ")
    };
    let packs_line = format!("  packs     {packs}\n");
    out.push_str(&packs_line);
    let user_line = if r.user_policy_present {
        "keel.toml"
    } else {
        "(no keel.toml \u{2014} Level 0 applies)"
    };
    let user_row = format!("  user      {user_line}\n");
    out.push_str(&user_row);

    let policy = table(Some(&r.policy));
    let defaults = table(policy.get("defaults"));

    // The two merged layers, each key annotated with its winning layer.
    for layer in ["outbound", "llm"] {
        if let Some(Value::Object(m)) = defaults.get(layer) {
            render_table(&mut out, &format!("[defaults.{layer}]"), m, &|key| {
                r.sources.get(&format!("defaults.{layer}.{key}")).copied()
            });
        }
    }
    // Any other `defaults` table is user-only (the typed model admits none, so
    // this renders nothing after validation; kept for merge-shape honesty).
    for (key, v) in &defaults {
        if key != "outbound"
            && key != "llm"
            && let Value::Object(m) = v
        {
            render_table(
                &mut out,
                &format!("[defaults.{}]  # user", toml_key(key)),
                m,
                &|_| None,
            );
        }
    }
    // Remaining top-level tables (flows / journal / telemetry), pure user.
    for (key, v) in &policy {
        if key == "defaults" || key == "target" {
            continue;
        }
        if let Value::Object(m) = v {
            render_table(
                &mut out,
                &format!("[{}]  # user", toml_key(key)),
                m,
                &|_| None,
            );
        } else {
            let line = format!("\n{} = {}  # user\n", toml_key(key), toml_value(v));
            out.push_str(&line);
        }
    }
    // Target tables pass through the merge untouched.
    if let Some(Value::Object(targets)) = policy.get("target") {
        for (name, t) in targets {
            let header = format!(
                "[target.{}]  # user \u{2014} pass-through (engine resolves target \u{2192} \
                 defaults per key)",
                toml_string(name)
            );
            if let Value::Object(m) = t {
                render_table(&mut out, &header, m, &|_| None);
            }
        }
    }
    for note in &r.notes {
        let line = format!("\nnote: {note}\n");
        out.push_str(&line);
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    /// The embedded contract parses to the documented Level 0 values (the
    /// same sync-test the Python/Node front ends run against the repo file).
    #[test]
    fn embedded_defaults_match_contract_values() {
        let outbound = level0_layer("outbound");
        assert_eq!(outbound["timeout"], json!("30s"));
        assert_eq!(
            outbound["retry"],
            json!({
                "attempts": 3,
                "schedule": "exp(200ms, x2, max 30s, jitter)",
                "on": ["conn", "timeout", "429", "5xx"],
            })
        );
        assert_eq!(
            outbound["breaker"],
            json!({ "failures": 5, "cooldown": "15s" })
        );

        let llm = level0_layer("llm");
        assert_eq!(llm["timeout"], json!("120s"));
        assert_eq!(llm["retry"]["attempts"], json!(6));
        assert_eq!(llm["breaker"], json!({ "failures": 5, "cooldown": "30s" }));
        assert_eq!(llm["cache"], json!({ "mode": "dev" }));
    }

    /// Mirror of Python `test_empty_policy_gets_full_pack_layers` / the Node
    /// twin in llm-pack.test.mjs.
    #[test]
    fn empty_policy_gets_full_pack_layers() {
        let merged = effective_policy(&json!({}), &[]);
        assert_eq!(
            merged["defaults"]["outbound"],
            Value::Object(level0_layer("outbound"))
        );
        assert_eq!(
            merged["defaults"]["llm"],
            Value::Object(level0_layer("llm"))
        );
    }

    /// Mirror of Python `test_user_key_replaces_pack_key_wholesale`.
    #[test]
    fn user_key_replaces_layer_key_wholesale_and_targets_pass_through() {
        let user = json!({
            "defaults": { "llm": { "retry": { "attempts": 2 } } },
            "target": { "x": {} },
        });
        let merged = effective_policy(&user, &[]);
        assert_eq!(
            merged["defaults"]["llm"]["retry"],
            json!({ "attempts": 2 }),
            "user retry wins wholesale"
        );
        assert_eq!(
            merged["defaults"]["llm"]["cache"],
            json!({ "mode": "dev" }),
            "pack cache kept"
        );
        assert_eq!(
            merged["defaults"]["llm"]["breaker"],
            json!({ "failures": 5, "cooldown": "30s" }),
            "pack breaker kept"
        );
        assert_eq!(
            merged["target"],
            json!({ "x": {} }),
            "target tables untouched"
        );
    }

    /// Mirror of Python `test_idempotent_on_level0`.
    #[test]
    fn idempotent_on_level0() {
        let level0_policy = Value::Object(
            [("defaults".to_owned(), Value::Object(level0().clone()))]
                .into_iter()
                .collect(),
        );
        assert_eq!(effective_policy(&level0_policy, &[]), level0_policy);
    }

    /// Mirror of Python `test_provider_fragments_fold_as_identity`.
    #[test]
    fn provider_fragments_fold_as_identity() {
        let with = effective_policy(&json!({}), &[llm_pack_fragment(), llm_pack_fragment()]);
        let without = effective_policy(&json!({}), &[]);
        assert_eq!(with, without);
    }

    #[test]
    fn sources_classify_user_pack_and_defaults() {
        let user = json!({ "defaults": { "llm": { "retry": { "attempts": 2 } } } });
        let sources = layer_sources(&user, &[llm_pack_fragment()]);
        assert_eq!(sources["defaults.llm.retry"], "user");
        assert_eq!(sources["defaults.llm.cache"], "pack");
        assert_eq!(sources["defaults.llm.timeout"], "pack");
        assert_eq!(sources["defaults.outbound.timeout"], "defaults");
        assert_eq!(sources["defaults.outbound.retry"], "defaults");
    }

    #[test]
    fn dev_cache_note_tracks_the_composed_policy() {
        let with_default_cache = build_report(&BTreeSet::new(), None);
        assert_eq!(with_default_cache.notes, vec![DEV_CACHE_NOTE]);

        // The user replaces the llm cache wholesale with a concrete ttl → the
        // symbolic dev cache is gone and the note with it.
        let user = json!({ "defaults": { "llm": { "cache": { "ttl": "1h" } } } });
        let report = build_report(&BTreeSet::new(), Some(user));
        assert!(report.notes.is_empty());
    }

    #[test]
    fn packs_are_detected_from_import_evidence_in_registration_order() {
        let libs: BTreeSet<String> = ["anthropic", "openai", "requests"]
            .into_iter()
            .map(str::to_owned)
            .collect();
        let report = build_report(&libs, None);
        assert_eq!(report.packs, vec!["openai", "anthropic"]);
        // Fragments are identity over the embedded defaults; provenance says
        // the packs re-affirmed the llm layer.
        assert_eq!(report.sources["defaults.llm.cache"], "pack");
        assert_eq!(report.sources["defaults.outbound.timeout"], "defaults");
    }

    #[test]
    fn toml_rendering_quotes_and_sorts() {
        assert_eq!(toml_value(&json!("a\"b")), "\"a\\\"b\"");
        assert_eq!(toml_value(&json!([1, "x"])), "[1, \"x\"]");
        assert_eq!(
            toml_value(&json!({ "z": 1, "a": { "mode": "dev" } })),
            "{ a = { mode = \"dev\" }, z = 1 }"
        );
        assert_eq!(toml_key("api.example.com"), "\"api.example.com\"");
        assert_eq!(toml_key("timeout"), "timeout");
    }

    #[test]
    fn invalid_policy_is_keel_e001_usage_error() {
        let dir = tempfile::TempDir::new().unwrap();
        std::fs::write(
            dir.path().join("keel.toml"),
            "[target.\"x\"]\nretry = { attempts = 0 }\n",
        )
        .unwrap();
        let r = run(dir.path());
        assert_eq!(r.exit, EXIT_USAGE);
        assert!(r.to_stderr);
        assert_eq!(r.json["code"], json!("KEEL-E001"));
        assert_eq!(r.json["field"], json!("target.x.retry.attempts"));
        assert!(r.human.contains("KEEL-E001"));
        assert!(r.human.contains("target.x.retry.attempts"));
    }

    #[test]
    fn absent_policy_composes_pure_level0() {
        let dir = tempfile::TempDir::new().unwrap();
        let r = run(dir.path());
        assert_eq!(r.exit, EXIT_OK);
        assert_eq!(r.json["user_policy_present"], json!(false));
        assert_eq!(
            r.json["policy"]["defaults"]["llm"]["timeout"],
            json!("120s")
        );
        assert!(r.human.contains("(no keel.toml \u{2014} Level 0 applies)"));
    }

    /// Every fact in the JSON twin surfaces in the human report: packs, the
    /// winning-layer annotations, the note.
    #[test]
    fn human_carries_the_json_facts() {
        let libs: BTreeSet<String> = ["openai".to_owned()].into_iter().collect();
        let user = json!({
            "defaults": { "llm": { "retry": { "attempts": 2 } } },
            "target": { "api.example.com": { "retry": { "attempts": 5 } } },
        });
        let report = build_report(&libs, Some(user));
        let text = human(&report);
        assert!(text.contains("openai"));
        assert!(text.contains("# user"));
        assert!(text.contains("# pack"));
        assert!(text.contains("# defaults"));
        assert!(text.contains("[target.\"api.example.com\"]"));
        assert!(text.contains("note: "));
    }
}
