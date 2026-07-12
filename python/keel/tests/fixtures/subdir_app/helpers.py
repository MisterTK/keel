"""A sibling module of app.py — importable only if the script's own directory
is on sys.path (as CPython does for `python subdir/app.py`)."""


def value() -> int:
    return 99
