"""`KEEL_LOG_FORMAT=json`: Keel's five deployment-evidence lines — the
activation banner, the exit summary, the activation refusal, the
ephemeral-journal warning (`journal-ephemeral-storage`), and the cache-poll
warning (`cache-poll-suspect`) — each become one JSON object (sorted keys, no
spaces) so container log pipelines index their fields (field report
2026-09-15, F10). Nothing else converts: other `keel ▸ …` prose Keel can
write at runtime (a pack's unwrapped-tool warning, a `KEEL_SIM_PLAN` read
failure, …) stays prose. Text is the default; there is no auto-detect
(byte-identity tests and `keel run` piping depend on the text form).

Severity coverage is UNIVERSAL across all five, not a subset (#130): `INFO`
on the activation and summary, `ERROR` on the refusal, `WARNING` on both
warning kinds — Cloud Run fills an entry's severity only from a `severity`
field in the payload, so any kind left out is indistinguishable from a
healthy line in a severity-filtered view. A sixth kind introduced later must
carry one too, or this docstring is lying again.

The Node front end's `src/log.mjs` is the byte-for-byte twin: same recognized
value, same separators, same key ordering. Keep every emitted value a string,
an int or null — `json.dumps(ensure_ascii=False)` and `JSON.stringify` agree
on exactly those, and nothing here needs more.
"""

from __future__ import annotations

import json
import sys
from typing import Any, Mapping

_JSON_FORMATS = {"json"}


def json_logs(env: Mapping[str, str]) -> bool:
    """Whether this process emits JSON lines. Exactly one recognized value;
    anything else (including `1`/`true`) silently keeps the text form."""
    return env.get("KEEL_LOG_FORMAT", "").strip().lower() in _JSON_FORMATS


def dumps_line(obj: Mapping[str, Any]) -> str:
    return json.dumps(obj, sort_keys=True, separators=(",", ":"), ensure_ascii=False) + "\n"


def emit(env: Mapping[str, str], text: str, obj: Mapping[str, Any]) -> None:
    """Write `text` (already newline-terminated) or the JSON form of `obj`."""
    sys.stderr.write(dumps_line(obj) if json_logs(env) else text)
