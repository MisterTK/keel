"""Prints what a spawned Python CHILD sees: KEEL_ENABLE and KEEL_CWD."""
import os
import subprocess
import sys

out = subprocess.run(
    [sys.executable, "-c", "import os; print(os.environ.get('KEEL_ENABLE', '') + '|' + os.environ.get('KEEL_CWD', ''))"],
    capture_output=True,
    text=True,
    check=True,
)
sys.stdout.write(out.stdout)
