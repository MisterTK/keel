// Policy `journal` selection through the front end (architecture spec §4.2).
//
// Two seams, each with its own leg:
//
// * `applyJournalEnvOverride` — the pure KEEL_JOURNAL escape hatch (env wins
//   over keel.toml's `journal` key by dropping the key from the effective
//   policy). No native addon needed.
// * The native core honoring `journal` at configure time: a `file:` location
//   attaches SQLite there (dirs created, `persistent` flips live), and a
//   `postgres://` location attaches the real Postgres/fleet backend
//   (Level 3) — both through the SAME `configure` error path the front end
//   already surfaces. Auto-skips when the addon is absent. The postgres leg
//   here exercises only the failure path (a malformed URL, so it stays fast
//   and network-free); the backend's own conformance/integration coverage
//   lives in `crates/keel-journal`.

import test from "node:test";
import assert from "node:assert/strict";
import { existsSync, mkdtempSync, rmSync } from "node:fs";
import { tmpdir } from "node:os";
import { join } from "node:path";
import { applyJournalEnvOverride } from "../src/bootstrap.mjs";
import { loadBackend } from "../src/backend.mjs";
import { loaded as nativeLoaded } from "../../keel-core-native/index.mjs";

const gate = nativeLoaded
  ? {}
  : { skip: "keel-core-native binary absent — build with `cargo build -p keel-node --release`" };

test("KEEL_JOURNAL set → the policy journal key is dropped (env escape hatch)", () => {
  const policy = { journal: "file:custom/j.db", target: {} };
  const out = applyJournalEnvOverride(policy, { KEEL_JOURNAL: "/tmp/other.db" });
  assert.equal("journal" in out, false);
  assert.deepEqual(out.target, {});
  assert.equal("journal" in policy, true, "the input policy is not mutated");
});

test("KEEL_JOURNAL empty string still wins (disables the journal)", () => {
  const out = applyJournalEnvOverride({ journal: "file:j.db" }, { KEEL_JOURNAL: "" });
  assert.equal("journal" in out, false);
});

test("KEEL_JOURNAL absent → the policy journal stays in force", () => {
  const policy = { journal: "file:custom/j.db" };
  assert.equal(applyJournalEnvOverride(policy, {}), policy);
});

test("no journal key → the policy is untouched", () => {
  const policy = { target: {} };
  assert.equal(applyJournalEnvOverride(policy, { KEEL_JOURNAL: "x" }), policy);
});

test("file: journal location attaches at configure (native)", gate, async () => {
  const dir = mkdtempSync(join(tmpdir(), "keel-journal-policy-"));
  try {
    // KEEL_JOURNAL="" → an in-memory native core (no construction journal).
    const backend = await loadBackend({ preferred: "native", cwd: dir, env: { KEEL_JOURNAL: "" } });
    assert.equal(backend.kind, "native");
    assert.equal(backend.persistent, false, "in-memory before configure");
    const path = join(dir, "custom", "nested", "j.db");
    backend.configure({ journal: `file:${path}` });
    assert.equal(backend.persistent, true, "policy journal attached live");
    assert.ok(existsSync(path), "store created at the policy path, directories included");
  } finally {
    rmSync(dir, { recursive: true, force: true });
  }
});

test("malformed postgres:// journal fails configure without leaking credentials (native)", gate, async () => {
  const dir = mkdtempSync(join(tmpdir(), "keel-journal-pg-"));
  try {
    const backend = await loadBackend({ preferred: "native", cwd: dir, env: { KEEL_JOURNAL: "" } });
    let err;
    try {
      // `postgres://` is a real backend now (Level 3/fleet); this exercises
      // only the failure path — through the same error path a bad `file:`
      // path uses (KEEL-E040) — with a malformed URL so the test stays fast
      // and network-free.
      backend.configure({ journal: "postgres://keel:sekrit@[not-a-valid-host/keel" });
    } catch (e) {
      err = e;
    }
    assert.ok(err, "configure must throw for a malformed journal location");
    assert.equal(err.code, "KEEL-E040");
    assert.ok(!String(err).includes("sekrit"), "credentials never printed");
    assert.equal(backend.persistent, false, "the rejected location attaches nothing");
  } finally {
    rmSync(dir, { recursive: true, force: true });
  }
});
