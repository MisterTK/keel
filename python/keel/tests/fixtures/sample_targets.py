"""Fixture module whose module-level functions the import hook wraps.

Deliberately trivial and side-effect-free: the tests assert on wrapping,
metadata preservation, and routing, not on what these functions compute.
"""

from __future__ import annotations


def enrich_a(x: int) -> int:
    """doc-a"""
    return x + 1


def enrich_b(x: int) -> int:
    """doc-b"""
    return x * 2


def other(x: int) -> int:
    """not selected by an enrich_* glob"""
    return -x
