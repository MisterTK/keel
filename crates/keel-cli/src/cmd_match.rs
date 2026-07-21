//! A Rust-side copy of the `[flows.match."cmd:<name>"]` argv-matching dialect
//! (CCR-5, docs/targeting.md) so `keel doctor` can cross-reference a scanned
//! `SubprocessSighting` against declared rules (issue #41). This is a THIRD
//! home for the same tie-break the two runtime interceptors already carry —
//! Python `subprocess_pack.py`'s `_compile`/`_match`, Node
//! `child-process.mjs`'s `compileCmdMatchers`/`matchArgv` — a maintenance
//! cost the issue calls out explicitly. Keep the three in lock-step: single
//! `*` wildcard (may repeat within one position's pattern), per-position,
//! case-sensitive, anchored full-string match; specificity tie-break sorts
//! ascending by (wildcard count, `-literal char count`, entrypoint name), so
//! fewest wildcards wins, ties broken toward more literal characters, final
//! tie broken lexicographically by the `cmd:<name>` entrypoint string.

use std::collections::BTreeMap;

use keel_core_api::policy::FlowMatchRule;

/// One compiled `[flows.match."cmd:<name>"]` rule, ready to test against an
/// observed argv. `patterns[i]` is position `i`'s pattern pre-split on `*`
/// (an empty split segment either side of consecutive `*`s is expected and
/// handled by `segments_match`).
pub(crate) struct CompiledCmdRule {
    pub(crate) entrypoint: String,
    patterns: Vec<Vec<String>>,
    wildcards: usize,
    literal: usize,
}

/// Compile every rule with at least one argv pattern (an empty-pattern
/// `cmd:` entrypoint matches nothing in-process — same "requires an explicit
/// argv rule to fire" rule the Python/Node interceptors enforce), sorted
/// most-specific-first.
pub(crate) fn compile_cmd_rules(
    match_table: &BTreeMap<String, FlowMatchRule>,
) -> Vec<CompiledCmdRule> {
    let mut out: Vec<CompiledCmdRule> = match_table
        .iter()
        .filter(|(_, rule)| !rule.argv.is_empty())
        .map(|(entrypoint, rule)| {
            let patterns: Vec<Vec<String>> = rule
                .argv
                .iter()
                .map(|p| p.split('*').map(str::to_owned).collect())
                .collect();
            let wildcards: usize = rule.argv.iter().map(|p| p.matches('*').count()).sum();
            let literal: usize = rule
                .argv
                .iter()
                .map(|p| p.chars().count() - p.matches('*').count())
                .sum();
            CompiledCmdRule {
                entrypoint: entrypoint.clone(),
                patterns,
                wildcards,
                literal,
            }
        })
        .collect();
    out.sort_by(|a, b| {
        a.wildcards
            .cmp(&b.wildcards)
            .then(b.literal.cmp(&a.literal))
            .then(a.entrypoint.cmp(&b.entrypoint))
    });
    out
}

/// Whether `segments` (one position's pattern, pre-split on `*`) matches
/// `value` — the anchored, `*`-only glob the runtime interceptors use
/// (`re.escape` + `.*`-join in Python, an equivalent in Node). Case-sensitive,
/// no other glob metacharacters recognized (`?`/`[` stay literal upstream).
fn segments_match(segments: &[String], value: &str) -> bool {
    if segments.len() == 1 {
        return segments[0] == value;
    }
    let Some(mut rest) = value.strip_prefix(segments[0].as_str()) else {
        return false;
    };
    let last = segments.last().expect("len() > 1 checked above");
    let Some(mid) = rest.strip_suffix(last.as_str()) else {
        return false;
    };
    rest = mid;
    for seg in &segments[1..segments.len() - 1] {
        if seg.is_empty() {
            continue;
        }
        match rest.find(seg.as_str()) {
            Some(idx) => rest = &rest[idx + seg.len()..],
            None => return false,
        }
    }
    true
}

/// The most specific rule whose per-position patterns all match `argv`
/// (positional; `argv` may be longer than the pattern — trailing observed
/// args are unconstrained), or `None`. `rules` must already be
/// most-specific-first (see [`compile_cmd_rules`]), so the first match wins.
pub(crate) fn match_argv<'a>(rules: &'a [CompiledCmdRule], argv: &[String]) -> Option<&'a str> {
    rules
        .iter()
        .find(|rule| {
            argv.len() >= rule.patterns.len()
                && rule
                    .patterns
                    .iter()
                    .zip(argv)
                    .all(|(pat, val)| segments_match(pat, val))
        })
        .map(|rule| rule.entrypoint.as_str())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn rules(entries: &[(&str, &[&str])]) -> Vec<CompiledCmdRule> {
        let table: BTreeMap<String, FlowMatchRule> = entries
            .iter()
            .map(|(name, argv)| {
                (
                    (*name).to_owned(),
                    FlowMatchRule {
                        argv: argv.iter().map(|s| (*s).to_owned()).collect(),
                    },
                )
            })
            .collect();
        compile_cmd_rules(&table)
    }

    #[test]
    fn exact_argv_matches() {
        let r = rules(&[("cmd:etl", &["etl", "run"])]);
        let argv: Vec<String> = vec!["etl".into(), "run".into()];
        assert_eq!(match_argv(&r, &argv), Some("cmd:etl"));
    }

    #[test]
    fn mismatched_position_does_not_match() {
        let r = rules(&[("cmd:etl", &["etl", "run"])]);
        let argv: Vec<String> = vec!["etl".into(), "backfill".into()];
        assert_eq!(match_argv(&r, &argv), None);
    }

    #[test]
    fn shorter_argv_than_pattern_does_not_match() {
        let r = rules(&[("cmd:etl", &["etl", "run"])]);
        let argv: Vec<String> = vec!["etl".into()];
        assert_eq!(match_argv(&r, &argv), None);
    }

    #[test]
    fn trailing_unconstrained_args_still_match() {
        let r = rules(&[("cmd:etl", &["etl", "run"])]);
        let argv: Vec<String> = vec!["etl".into(), "run".into(), "--verbose".into()];
        assert_eq!(match_argv(&r, &argv), Some("cmd:etl"));
    }

    #[test]
    fn single_wildcard_position_matches_anything() {
        let r = rules(&[("cmd:etl", &["etl", "*"])]);
        let argv: Vec<String> = vec!["etl".into(), "backfill".into()];
        assert_eq!(match_argv(&r, &argv), Some("cmd:etl"));
    }

    #[test]
    fn wildcard_is_anchored_and_case_sensitive() {
        let r = rules(&[("cmd:etl", &["etl-*"])]);
        assert_eq!(
            match_argv(&r, &["etl-run".to_owned()]),
            Some("cmd:etl"),
            "prefix+wildcard matches"
        );
        assert_eq!(
            match_argv(&r, &["ETL-run".to_owned()]),
            None,
            "case-sensitive: differently-cased prefix does not match"
        );
        assert_eq!(
            match_argv(&r, &["xetl-run".to_owned()]),
            None,
            "anchored: extra leading text before the literal prefix does not match"
        );
    }

    #[test]
    fn fewer_wildcards_wins_specificity_tie_break() {
        let r = rules(&[("cmd:wild", &["*"]), ("cmd:exact", &["etl"])]);
        let argv: Vec<String> = vec!["etl".into()];
        assert_eq!(
            match_argv(&r, &argv),
            Some("cmd:exact"),
            "the zero-wildcard rule is tried first even though both match"
        );
    }

    #[test]
    fn more_literal_chars_wins_when_wildcard_counts_tie() {
        let r = rules(&[("cmd:short", &["e*"]), ("cmd:long", &["etl*"])]);
        let argv: Vec<String> = vec!["etl-run".into()];
        assert_eq!(
            match_argv(&r, &argv),
            Some("cmd:long"),
            "both have one wildcard; the more-literal pattern is tried first"
        );
    }

    #[test]
    fn entrypoint_name_breaks_final_tie() {
        let r = rules(&[("cmd:bbb", &["etl"]), ("cmd:aaa", &["etl"])]);
        let argv: Vec<String> = vec!["etl".into()];
        assert_eq!(match_argv(&r, &argv), Some("cmd:aaa"));
    }

    #[test]
    fn empty_argv_pattern_rule_never_matches() {
        let r = rules(&[("cmd:unset", &[])]);
        assert!(
            r.is_empty(),
            "a rule with no argv patterns compiles to nothing"
        );
        let argv: Vec<String> = vec!["anything".into()];
        assert_eq!(match_argv(&r, &argv), None);
    }
}
