"""Policy loading: keel.toml → dict → configure, the Level 0 defaults path,
loud failure on a broken file, and defaults parity with the frozen contract."""

from __future__ import annotations

import tomllib
import unittest
from pathlib import Path
from tempfile import TemporaryDirectory

from keel._backend import load_backend
from keel._defaults import level0_defaults
from keel._errors import KeelError
from keel._policy import extract_function_targets, load_policy

from . import CONTRACTS


class PolicyLoadTest(unittest.TestCase):
    def test_absent_file_yields_level0_defaults(self) -> None:
        with TemporaryDirectory() as d:
            policy, source = load_policy(d)
        self.assertEqual(source, "defaults")
        self.assertEqual(policy, level0_defaults())

    def test_defaults_configure_cleanly(self) -> None:
        # The Level 0 pack must be accepted by the backend it is handed to.
        backend = load_backend("stub")
        backend.configure(level0_defaults())  # must not raise

    def test_present_file_parses_and_configures(self) -> None:
        toml = (
            '[target."py:pipeline.enrich.run"]\n'
            'retry = { attempts = 4, on = ["other"], schedule = "fixed(1ms)" }\n'
        )
        with TemporaryDirectory() as d:
            (Path(d) / "keel.toml").write_text(toml, encoding="utf-8")
            policy, source = load_policy(d)
        self.assertEqual(source, "keel.toml")
        self.assertEqual(policy["target"]["py:pipeline.enrich.run"]["retry"]["attempts"], 4)
        load_backend("stub").configure(policy)  # must not raise

    def test_invalid_toml_is_loud_e001(self) -> None:
        with TemporaryDirectory() as d:
            (Path(d) / "keel.toml").write_text("this is = = not toml", encoding="utf-8")
            with self.assertRaises(KeelError) as ctx:
                load_policy(d)
        self.assertEqual(ctx.exception.code, "KEEL-E001")

    def test_unreadable_file_is_loud_e001(self) -> None:
        # A keel.toml that is present but cannot be read (here: it is a
        # directory) is a loud KEEL-E001, never a silent fall-back.
        with TemporaryDirectory() as d:
            (Path(d) / "keel.toml").mkdir()
            with self.assertRaises(KeelError) as ctx:
                load_policy(d)
        self.assertEqual(ctx.exception.code, "KEEL-E001")

    def test_defaults_mirror_contract_verbatim(self) -> None:
        contract = CONTRACTS / "defaults.toml"
        if not contract.exists():
            self.skipTest("contracts/defaults.toml not present in this checkout")
        parsed = tomllib.loads(contract.read_text(encoding="utf-8"))
        self.assertEqual(level0_defaults(), parsed)


class FunctionTargetExtractionTest(unittest.TestCase):
    def test_extracts_py_targets_with_module_and_glob(self) -> None:
        policy = {
            "target": {
                "py:pipeline.enrich.*": {},
                "py:jobs.nightly.run": {},
                "api.stripe.com": {},  # host target: ignored here
                "py:bad": {},  # no module.func split: ignored
                "py:pkg.*.run": {},  # mid-path glob: out of v0.1 scope
            }
        }
        got = {(t.key, t.module, t.func_glob) for t in extract_function_targets(policy)}
        self.assertEqual(
            got,
            {
                ("py:pipeline.enrich.*", "pipeline.enrich", "*"),
                ("py:jobs.nightly.run", "jobs.nightly", "run"),
            },
        )

    def test_no_targets_when_none_declared(self) -> None:
        self.assertEqual(extract_function_targets(level0_defaults()), [])


if __name__ == "__main__":
    unittest.main()
