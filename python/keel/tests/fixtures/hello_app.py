"""A tiny, deterministic app: fixed stdout + stderr lines, then exit 7.

Used by the byte-identity and banner tests, so it must have no import-time
Keel surface and a stable, non-zero exit code.
"""

import sys

print("stdout-line-1")
print("computed", 6 * 7)
print("stderr-line-1", file=sys.stderr)
sys.exit(7)
