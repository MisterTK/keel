"""Policy `journal` selection through the front end (architecture spec §4.2).

Two seams, each with its own leg:

* `bootstrap.apply_journal_env_override` — the pure KEEL_JOURNAL escape hatch
  (env wins over keel.toml's `journal` key by dropping the key from the
  effective policy). No native module needed.
* The native core honoring `journal` at configure time: a `file:` location
  attaches SQLite there (dirs created, `persistent` flips live), and a
  `postgres://` location fails loudly with KEEL-E005 through the SAME
  `configure` error path the front end already surfaces (skips without the
  built `keel_core`).
"""

from __future__ import annotations

import unittest
from pathlib import Path
from tempfile import TemporaryDirectory

from keel.bootstrap import apply_journal_env_override

try:  # native-only legs: policy journal selection lives in the real core
    import keel_core  # noqa: F401

    _NATIVE = True
except ImportError:
    _NATIVE = False

_POSTGRES_E005 = "Postgres journal not yet available in this build; use file: — see docs"


class JournalEnvOverrideTest(unittest.TestCase):
    """KEEL_JOURNAL (when set) beats the policy `journal` key (escape hatch)."""

    def test_env_set_drops_the_policy_journal_key(self) -> None:
        policy = {"journal": "file:custom/j.db", "target": {}}
        out = apply_journal_env_override(policy, {"KEEL_JOURNAL": "/tmp/other.db"})
        self.assertNotIn("journal", out)
        self.assertEqual(out["target"], {})
        self.assertIn("journal", policy, "the input policy is not mutated")

    def test_env_empty_string_still_wins_and_disables(self) -> None:
        out = apply_journal_env_override({"journal": "file:j.db"}, {"KEEL_JOURNAL": ""})
        self.assertNotIn("journal", out)

    def test_env_absent_leaves_the_policy_journal_in_force(self) -> None:
        policy = {"journal": "file:custom/j.db"}
        self.assertIs(apply_journal_env_override(policy, {}), policy)

    def test_no_journal_key_is_untouched(self) -> None:
        policy = {"target": {}}
        self.assertIs(apply_journal_env_override(policy, {"KEEL_JOURNAL": "x"}), policy)


@unittest.skipUnless(_NATIVE, "keel_core native module not built (maturin develop in crates/keel-py)")
class NativeJournalPolicyTest(unittest.TestCase):
    def test_file_location_attaches_the_journal_at_configure(self) -> None:
        """`journal = "file:<path>"` on an in-memory core: configure attaches
        SQLite there (parent dirs created) and `persistent` flips — read live,
        not snapshotted at construction."""
        with TemporaryDirectory() as d:
            path = Path(d) / "custom" / "nested" / "j.db"
            core = keel_core.KeelCore()  # in-memory: no construction journal
            self.assertFalse(core.persistent)
            core.configure({"journal": f"file:{path}"})
            self.assertTrue(core.persistent, "policy journal attached live")
            self.assertTrue(path.exists(), "store created at the policy path")

    def test_postgres_location_raises_e005_through_configure(self) -> None:
        """The KEEL-E005 contract surfaces through the front end's existing
        configure error path — exact frozen message, credentials never echoed."""
        core = keel_core.KeelCore()
        with self.assertRaises(keel_core.KeelCoreError) as ctx:
            core.configure({"journal": "postgres://keel:sekrit@db.internal/keel"})
        self.assertEqual(ctx.exception.code, "KEEL-E005")
        self.assertEqual(ctx.exception.message, _POSTGRES_E005)
        self.assertNotIn("sekrit", str(ctx.exception), "credentials never printed")
        self.assertFalse(core.persistent, "the rejected location attaches nothing")


if __name__ == "__main__":
    unittest.main()
