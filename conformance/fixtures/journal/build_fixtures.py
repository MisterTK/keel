#!/usr/bin/env python3
"""Build golden journal fixture databases from the checked-in .sql files.

Creates .gen/<name>.db (gitignored) for each fixture, applying the frozen
schema (contracts/journal.sql) first. Also sanity-checks each database
against the stories the fixtures promise (statuses, step counts).
"""

from __future__ import annotations

import sqlite3
import sys
from pathlib import Path

HERE = Path(__file__).resolve().parent
ROOT = HERE.parent.parent.parent
SCHEMA = ROOT / "contracts" / "journal.sql"

EXPECT = {
    "completed-flow": ("completed", 5),
    "interrupted-flow": ("running", 4),
    "dead-flow": ("dead", 2),
}


def main() -> int:
    out_dir = HERE / ".gen"
    out_dir.mkdir(exist_ok=True)
    schema_sql = SCHEMA.read_text()
    failures = []
    for name, (status, steps) in EXPECT.items():
        db_path = out_dir / f"{name}.db"
        db_path.unlink(missing_ok=True)
        con = sqlite3.connect(db_path)
        try:
            con.executescript(schema_sql)
            con.executescript((HERE / f"{name}.sql").read_text())
            con.commit()
            got_status, got_steps = con.execute(
                "SELECT f.status, (SELECT COUNT(*) FROM steps s WHERE s.flow_id = f.flow_id)"
                " FROM flows f"
            ).fetchone()
            if (got_status, got_steps) != (status, steps):
                failures.append(
                    f"{name}: expected ({status}, {steps} steps), got ({got_status}, {got_steps})"
                )
            else:
                print(f"ok    {db_path.relative_to(ROOT)}  ({status}, {steps} steps)")
        finally:
            con.close()
    for f in failures:
        print(f"FAIL  {f}", file=sys.stderr)
    return 1 if failures else 0


if __name__ == "__main__":
    sys.exit(main())
