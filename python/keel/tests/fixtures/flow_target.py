"""Fixture flow entrypoint (WS2 double-activation regression, test_auto.py).

Deliberately does nothing at import time: it must only run when something
actually DISPATCHES it as a flow (`keel._flow.run_as_flow` imports the module
and calls `main()`). Plain `python flow_target.py` / an un-dispatched
`keel run flow_target.py` never calls `main()`, so "FLOW-RAN" on stdout is a
reliable signal that flow dispatch happened.
"""

from __future__ import annotations


def main() -> None:
    print("FLOW-RAN")
