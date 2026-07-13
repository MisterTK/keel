#!/usr/bin/env python3
"""Assert an adapter pack's `detect()` reports the expected confidence grade
against WHATEVER version of the library is actually installed in the current
interpreter.

This is the "exercise the pinned -> best_effort flip against a real
out-of-range install" leg the adapter CI farm needs (see
.github/workflows/adapter-farm.yml `python-out-of-range` job): the packs'
`_is_pinned` prefix-match logic (python/keel/src/keel/adapters/httpx_pack.py,
requests_pack.py) is otherwise only ever exercised by unit tests that
monkeypatch `importlib.metadata.version` — never against a real install
outside the pinned range.

Usage: check-adapter-grading.py <pack> <pinned|best_effort>
  <pack>  one of: httpx, requests
Exit 0 if detect().confidence matches, 1 (with a diagnostic) otherwise.
"""

from __future__ import annotations

import importlib
import sys
from pathlib import Path

REPO = Path(__file__).resolve().parent.parent
sys.path.insert(0, str(REPO / "python" / "keel" / "src"))

PACKS = {
    "httpx": "keel.adapters.httpx_pack",
    "requests": "keel.adapters.requests_pack",
}


def main(argv: list[str]) -> int:
    if len(argv) != 3 or argv[1] not in PACKS or argv[2] not in ("pinned", "best_effort"):
        print(__doc__, file=sys.stderr)
        return 2
    pack_name, expected = argv[1], argv[2]
    module = importlib.import_module(PACKS[pack_name])
    detection = module.detect()
    if not detection.matched:
        print(f"check-adapter-grading: FAILED — {pack_name} is not importable in this interpreter", file=sys.stderr)
        return 1
    if detection.confidence != expected:
        print(
            f"check-adapter-grading: FAILED — {pack_name} {detection.version} "
            f"graded '{detection.confidence}', expected '{expected}' "
            f"(pack pinned range: {module._PINNED!r})",
            file=sys.stderr,
        )
        return 1
    print(f"check-adapter-grading: OK — {pack_name} {detection.version} graded '{detection.confidence}' as expected")
    return 0


if __name__ == "__main__":
    raise SystemExit(main(sys.argv))
