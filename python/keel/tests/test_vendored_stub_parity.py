"""The `keel` wheel vendors `keel_core_stub` (the wheel cannot ship files from
python/keel-core-stub, a separate unpublished dist); stub semantics are
conformance-frozen, so the vendored copy must stay byte-identical to its
source. Refresh with scripts/sync-vendored.sh. Skips when the source is absent
(installed package outside the repo checkout)."""

from __future__ import annotations

import unittest
from pathlib import Path

_HERE = Path(__file__).resolve()
VENDORED = _HERE.parents[1] / "src" / "keel_core_stub" / "__init__.py"
SOURCE = _HERE.parents[2] / "keel-core-stub" / "keel_core_stub" / "__init__.py"


class VendoredStubParityTest(unittest.TestCase):
    def test_vendored_stub_is_byte_identical_to_its_source(self) -> None:
        if not SOURCE.exists():
            self.skipTest("python/keel-core-stub not present (not a repo checkout)")
        self.assertEqual(
            VENDORED.read_bytes(),
            SOURCE.read_bytes(),
            "python/keel/src/keel_core_stub/__init__.py drifted from "
            "python/keel-core-stub; run scripts/sync-vendored.sh",
        )


if __name__ == "__main__":
    unittest.main()
