"""Keel — the Python front end (Tier 1: resilience, zero code changes).

`keel run app.py` installs, before user code loads: (1) a `sys.meta_path`
import hook that wraps functions matching `py:` policy targets, and (2)
per-call discovery recording to `.keel/discovery.db`. The resilience
semantics live in the backend (`keel_core` native module when importable,
else the in-repo pure-Python `keel_core_stub`); this package is the thin,
stdlib-only front end that drives it.

DX invariants (docs/dx-spec.md §3) this package must uphold:
  1. zero code changes in user code;
  2. uninstall = remove the package (no lock-in): see `uninstall_keel`;
  4. silence on success (one stderr banner at startup, then quiet);
  5. never swallow errors — the original exception propagates unchanged,
     with a `keel_outcome` attachment for those who look;
  and KEEL_DISABLE=1 makes a run byte-identical to one without Keel.

The public entry points are `keel.bootstrap.install_keel` /
`uninstall_keel` and the `keel._run` runner used by `python -m keel run`.
"""

from __future__ import annotations

__all__ = ["__version__"]

__version__ = "0.3.0"
