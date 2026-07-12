"""A 10-step durable pipeline (dx-spec §1 Level 2 — the demo that sells it).

`do_step` is a plain `py:` function target listed in keel.toml, so listing the
entrypoint under [flows] makes the whole run crash-resumable with ZERO code
changes. Each step appends one line to a shared side-effect log — a real,
process-external record that no in-process mock could fake across a `kill -9`.

`KEEL_DEMO_CRASH_AT=n` hard-crashes (SIGKILL) right before step n fires, so the
first run dies mid-pipeline. Re-run and steps already journaled are substituted
(their effects never re-fire — the log gains no duplicate lines) and the rest run
live to completion.
"""

import os
import signal

_LOG = os.environ["KEEL_DEMO_LOG"]
_CRASH_AT = int(os.environ.get("KEEL_DEMO_CRASH_AT", "0"))


def do_step(n):
    with open(_LOG, "a", encoding="utf-8") as f:
        f.write(f"step-{n}\n")
    return {"step": n}


def main():
    for n in range(1, 11):
        if _CRASH_AT and n == _CRASH_AT:
            os.kill(os.getpid(), signal.SIGKILL)  # kill -9 before step n fires
        do_step(n)
    print("PIPELINE_COMPLETE")
