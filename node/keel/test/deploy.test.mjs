// Parity twin of `python/keel/tests/test_deploy.py` — same four cases, plus a
// byte-identity assertion against Python's exact text template (built here
// from the same literal pieces, not imported cross-language).

import test from "node:test";
import assert from "node:assert/strict";
import { mkdtempSync, writeFileSync, mkdirSync, chmodSync, rmSync } from "node:fs";
import { tmpdir } from "node:os";
import { join } from "node:path";

import { ephemeralJournalWarning } from "../src/deploy.mjs";

const FLOWS = { flows: { entrypoints: ["py:app:main"] } };

function tmp() {
  return mkdtempSync(join(tmpdir(), "keel-deploy-"));
}

test("serverless marker with flows and sqlite warns", () => {
  const d = tmp();
  try {
    const got = ephemeralJournalWarning(FLOWS, { K_SERVICE: "render" }, d, {
      dockerenv: join(d, "nope"),
    });
    assert.notEqual(got, null);
    const [text, obj] = got;
    assert.ok(
      text.startsWith(
        "keel ▸ warning: durable flows are configured but the journal is SQLite at "
      )
    );
    assert.ok(text.includes("(K_SERVICE)"));
    assert.equal(obj.code, "journal-ephemeral-storage");
    assert.equal(obj.marker, "K_SERVICE");
    assert.ok(obj.journal.endsWith(`${join(".keel", "journal.db")}`));

    // Byte-identity with Python's exact template, built independently here.
    const journal = join(d, ".keel", "journal.db");
    const expected =
      `keel ▸ warning: durable flows are configured but the journal is SQLite at ${journal} ` +
      "on ephemeral storage (K_SERVICE) — flow state will not survive an instance replacement; " +
      "mount a volume for .keel/ or use a Postgres journal\n";
    assert.equal(text, expected);
  } finally {
    rmSync(d, { recursive: true, force: true });
  }
});

test("dockerenv and read-only cwd are markers", () => {
  let d = tmp();
  try {
    const marker = join(d, ".dockerenv");
    writeFileSync(marker, "");
    const [, obj] = ephemeralJournalWarning(FLOWS, {}, d, { dockerenv: marker });
    assert.equal(obj.marker, "/.dockerenv");
  } finally {
    rmSync(d, { recursive: true, force: true });
  }

  d = tmp();
  try {
    const ro = join(d, "ro");
    mkdirSync(ro);
    chmodSync(ro, 0o500);
    try {
      const got = ephemeralJournalWarning(FLOWS, {}, ro, { dockerenv: join(d, "nope") });
      assert.equal(got[1].marker, "read-only cwd");
    } finally {
      chmodSync(ro, 0o700);
    }
  } finally {
    rmSync(d, { recursive: true, force: true });
  }
});

test("no flows, postgres, or no marker is silent", () => {
  const d = tmp();
  try {
    assert.equal(
      ephemeralJournalWarning({}, { K_SERVICE: "x" }, d, { dockerenv: join(d, "nope") }),
      null
    );
    const pg = { ...FLOWS, journal: "postgres://u:p@h/db" };
    assert.equal(
      ephemeralJournalWarning(pg, { K_SERVICE: "x" }, d, { dockerenv: join(d, "nope") }),
      null
    );
    assert.equal(ephemeralJournalWarning(FLOWS, {}, d, { dockerenv: join(d, "nope") }), null);
  } finally {
    rmSync(d, { recursive: true, force: true });
  }
});

test("cmd: flows count as configured", () => {
  const d = tmp();
  try {
    const pol = {
      flows: { entrypoints: ["cmd:etl"], match: { "cmd:etl": { argv: ["run_etl.sh"] } } },
    };
    assert.notEqual(
      ephemeralJournalWarning(pol, { K_SERVICE: "x" }, d, { dockerenv: join(d, "nope") }),
      null
    );
  } finally {
    rmSync(d, { recursive: true, force: true });
  }
});
