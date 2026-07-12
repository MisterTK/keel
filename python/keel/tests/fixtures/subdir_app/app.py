"""Lives in a SUBDIRECTORY and imports a sibling module by bare name. This
resolves under plain `python subdir_app/app.py` (CPython puts the script's dir
on sys.path) and must resolve identically under `keel run`.
"""

import sys

import helpers

print("helper says", helpers.value())
print("subdir stderr", file=sys.stderr)
sys.exit(5)
