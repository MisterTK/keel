"""Outbound host/URL-pattern target selection (`keel._targets`), tested
directly against the grammar and precedence rules normalized in
`docs/targeting.md`. These are a cross-language parity contract with the Node
twin (`node/keel/src/judge.mjs`'s `compileOutboundMatchers`/
`resolvePolicyTarget`) — assertions here pin the exact rules: exact > pattern >
class default, most-specific-pattern-wins, and the deterministic tie-break."""

from __future__ import annotations

import unittest

from keel import _targets


class CompileOutboundTargetsTest(unittest.TestCase):
    def test_bare_host_is_exact(self) -> None:
        compiled = _targets.compile_outbound_targets({"target": {"api.example.com": {}}})
        self.assertEqual(compiled.exact, frozenset({"api.example.com"}))
        self.assertEqual(compiled.patterns, ())

    def test_class_prefixed_keys_are_never_outbound(self) -> None:
        compiled = _targets.compile_outbound_targets(
            {
                "target": {
                    "py:pipeline.enrich.*": {},
                    "ts:jobs/nightly.ts#run": {},
                    "rs:pkg::mod": {},
                    "llm:openai": {},
                    "tool:search": {},
                    "mcp:fs": {},
                }
            }
        )
        self.assertEqual(compiled.exact, frozenset())
        self.assertEqual(compiled.patterns, ())

    def test_host_wildcard_method_port_or_path_makes_a_pattern(self) -> None:
        compiled = _targets.compile_outbound_targets(
            {
                "target": {
                    "*.internal.corp": {},
                    "GET api.catalog.internal/*": {},
                    "api.stripe.com:443": {},
                    "api.partner.com/v1/*": {},
                }
            }
        )
        self.assertEqual(compiled.exact, frozenset())
        self.assertEqual({p.key for p in compiled.patterns}, {
            "*.internal.corp",
            "GET api.catalog.internal/*",
            "api.stripe.com:443",
            "api.partner.com/v1/*",
        })

    def test_absent_or_malformed_target_table_compiles_empty(self) -> None:
        empty = _targets.CompiledTargets(frozenset(), ())
        self.assertEqual(_targets.compile_outbound_targets({}), empty)
        self.assertEqual(_targets.compile_outbound_targets({"target": "nope"}), empty)
        self.assertEqual(_targets.compile_outbound_targets("not-a-dict"), empty)


class ResolveOutboundTest(unittest.TestCase):
    def test_no_compiled_table_returns_bare_host(self) -> None:
        self.assertEqual(_targets.resolve_outbound(None, "GET", "api.example.com"), "api.example.com")

    def test_exact_beats_pattern(self) -> None:
        compiled = _targets.compile_outbound_targets(
            {"target": {"api.example.com": {}, "*.example.com": {}}}
        )
        self.assertEqual(
            _targets.resolve_outbound(compiled, "GET", "api.example.com"), "api.example.com"
        )

    def test_host_wildcard_crosses_dots(self) -> None:
        compiled = _targets.compile_outbound_targets({"target": {"*.internal.corp": {}}})
        self.assertEqual(
            _targets.resolve_outbound(compiled, "GET", "a.b.internal.corp"), "*.internal.corp"
        )
        self.assertEqual(
            _targets.resolve_outbound(compiled, "GET", "internal.corp"), "internal.corp",
            "the wildcard segment must consume at least the leading dot's host part",
        )

    def test_host_comparison_is_case_insensitive(self) -> None:
        compiled = _targets.compile_outbound_targets({"target": {"*.Internal.Corp": {}}})
        self.assertEqual(
            _targets.resolve_outbound(compiled, "GET", "DB.INTERNAL.CORP"), "*.Internal.Corp"
        )

    def test_path_glob_crosses_slashes_and_is_case_sensitive(self) -> None:
        compiled = _targets.compile_outbound_targets({"target": {"api.catalog.internal/*": {}}})
        self.assertEqual(
            _targets.resolve_outbound(
                compiled, "GET", "api.catalog.internal", path="/a/b/c"
            ),
            "api.catalog.internal/*",
        )
        # No matching pattern (path case differs and the key has no wildcard
        # host) falls through to the bare host — paths are case-sensitive.
        compiled_cs = _targets.compile_outbound_targets({"target": {"api.x/A/*": {}}})
        self.assertEqual(_targets.resolve_outbound(compiled_cs, "GET", "api.x", path="/a/y"), "api.x")

    def test_missing_path_normalizes_to_slash(self) -> None:
        compiled = _targets.compile_outbound_targets({"target": {"api.x/*": {}}})
        self.assertEqual(_targets.resolve_outbound(compiled, "GET", "api.x", path=None), "api.x/*")
        self.assertEqual(_targets.resolve_outbound(compiled, "GET", "api.x", path=""), "api.x/*")

    def test_method_prefix_must_match_exactly(self) -> None:
        compiled = _targets.compile_outbound_targets({"target": {"POST api.example.com": {}}})
        self.assertEqual(
            _targets.resolve_outbound(compiled, "GET", "api.example.com"), "api.example.com"
        )
        self.assertEqual(
            _targets.resolve_outbound(compiled, "POST", "api.example.com"),
            "POST api.example.com",
        )
        # Lower-case method input is still honored (normalized upper before compare).
        self.assertEqual(
            _targets.resolve_outbound(compiled, "post", "api.example.com"),
            "POST api.example.com",
        )

    def test_port_must_equal_the_effective_port(self) -> None:
        compiled = _targets.compile_outbound_targets({"target": {"api.example.com:443": {}}})
        self.assertEqual(
            _targets.resolve_outbound(compiled, "GET", "api.example.com", scheme="https"),
            "api.example.com:443",
            "no explicit port -> the https default (443) is the effective port",
        )
        self.assertEqual(
            _targets.resolve_outbound(compiled, "GET", "api.example.com", scheme="http"),
            "api.example.com",
            "http default (80) != 443 -> falls through to the bare host",
        )

    def test_explicit_port_overrides_scheme_default(self) -> None:
        compiled = _targets.compile_outbound_targets({"target": {"api.example.com:8443": {}}})
        self.assertEqual(
            _targets.resolve_outbound(
                compiled, "GET", "api.example.com", scheme="https", port=8443
            ),
            "api.example.com:8443",
        )
        self.assertEqual(
            _targets.resolve_outbound(
                compiled, "GET", "api.example.com", scheme="https", port=443
            ),
            "api.example.com",
        )

    def test_no_pattern_matches_falls_back_to_bare_host(self) -> None:
        compiled = _targets.compile_outbound_targets({"target": {"other.example.com": {}}})
        self.assertEqual(
            _targets.resolve_outbound(compiled, "GET", "api.example.com"), "api.example.com"
        )

    def test_most_specific_pattern_wins_by_literal_length(self) -> None:
        # Both patterns carry exactly one wildcard; the method+path-qualified
        # key has more literal characters and a method prefix, so it wins over
        # the bare host wildcard even though both match the request.
        compiled = _targets.compile_outbound_targets(
            {
                "target": {
                    "*.example.com": {},
                    "GET api.example.com/*": {},
                }
            }
        )
        self.assertEqual(
            _targets.resolve_outbound(compiled, "GET", "api.example.com", path="/v1/x"),
            "GET api.example.com/*",
        )

    def test_tie_break_is_lexicographic_and_deterministic(self) -> None:
        # Both patterns have one wildcard, the same literal-character count,
        # and no method prefix; both also match the same request. Selection
        # must still be total and repeatable — the lexicographically smaller
        # key wins ('*' < 'x' as a code point).
        p1, p2 = "api.example.com/x/*", "api.example.com/*/y"
        compiled = _targets.compile_outbound_targets({"target": {p1: {}, p2: {}}})
        self.assertEqual(
            _targets.resolve_outbound(compiled, "GET", "api.example.com", path="/x/y"), p2
        )
        # Order of declaration in the policy table must not matter.
        compiled_reordered = _targets.compile_outbound_targets({"target": {p2: {}, p1: {}}})
        self.assertEqual(
            _targets.resolve_outbound(compiled_reordered, "GET", "api.example.com", path="/x/y"),
            p2,
        )

    def test_llm_host_map_is_not_this_modules_concern(self) -> None:
        # `_targets` only resolves the outbound `[target]` table; the LLM host
        # map lives in `keel.adapters._http.resolve_policy_target`, which
        # checks it BEFORE ever consulting `_targets` (see test_adapters_http.py).
        compiled = _targets.compile_outbound_targets({"target": {"*.openai.com": {}}})
        self.assertEqual(
            _targets.resolve_outbound(compiled, "POST", "api.openai.com"), "*.openai.com"
        )


class InstallationStateTest(unittest.TestCase):
    def tearDown(self) -> None:
        _targets.clear_outbound_targets()

    def test_install_compiles_and_current_reflects_it(self) -> None:
        self.assertIsNone(_targets.current_outbound_targets())
        installed = _targets.install_outbound_targets({"target": {"api.example.com": {}}})
        self.assertIs(_targets.current_outbound_targets(), installed)
        self.assertEqual(installed.exact, frozenset({"api.example.com"}))

    def test_clear_resets_to_uninstalled(self) -> None:
        _targets.install_outbound_targets({"target": {"api.example.com": {}}})
        _targets.clear_outbound_targets()
        self.assertIsNone(_targets.current_outbound_targets())


if __name__ == "__main__":
    unittest.main()
