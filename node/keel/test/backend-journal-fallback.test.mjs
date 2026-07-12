// Task 14 review fix 1: journal-open-failure parity with the Python front end.
//
// When the native addon loads but its journal cannot open (unwritable / invalid
// path), `loadBackend` must degrade to an IN-MEMORY native core — NOT silently
// downgrade to the stub (auto) or throw KEEL-E040 (explicit native). Resilience
// still comes from the real engine; only cross-run dev-cache replay is lost.
//
// We force the failure with a journal path whose parent is a regular FILE, so
// the binding's `create_dir_all(parent)` fails deterministically and cross-
// platform. Auto-skips when the addon is absent.

import test from "node:test";
import assert from "node:assert/strict";
import { mkdtempSync, writeFileSync, rmSync } from "node:fs";
import { tmpdir } from "node:os";
import { join } from "node:path";
import { loadBackend } from "../src/backend.mjs";
import { loaded as nativeLoaded } from "../../keel-core-native/index.mjs";

const gate = nativeLoaded
  ? {}
  : { skip: "keel-core-native binary absent — build with `cargo build -p keel-node --release`" };

test("journal-open failure → in-memory native core (not a stub downgrade)", gate, async () => {
  const dir = mkdtempSync(join(tmpdir(), "keel-badjournal-"));
  // Suppress + capture the expected degradation warning so it doesn't clutter
  // test output while still proving it fires.
  const prevWarn = process.listeners("warning");
  process.removeAllListeners("warning");
  const warned = [];
  process.on("warning", (w) => warned.push(w.code));
  try {
    writeFileSync(join(dir, "notadir"), "x"); // a FILE where a dir is needed
    const badJournal = join(dir, "notadir", "journal.db"); // parent is a file → open fails
    const env = { KEEL_JOURNAL: badJournal };

    for (const preferred of ["auto", "native"]) {
      const backend = await loadBackend({ preferred, cwd: dir, env });
      assert.equal(backend.kind, "native", `${preferred}: native core, not a stub downgrade`);
      assert.equal(backend.persistent, false, `${preferred}: in-memory core is not persistent`);
      backend.configure({ target: { x: { retry: { attempts: 1 } } } });
      const out = await backend.execute(
        { v: 1, target: "x", op: "GET x", idempotent: true },
        async () => ({ status: "ok", payload: { ok: 1 } })
      );
      assert.equal(out.result, "ok", `${preferred}: call succeeds on the in-memory native core`);
      assert.deepEqual(out.payload, { ok: 1 });
    }

    await new Promise((r) => setImmediate(r)); // let emitWarning flush
    assert.ok(warned.includes("KEEL_JOURNAL_UNAVAILABLE"), "the degradation is warned, not silent");
  } finally {
    process.removeAllListeners("warning");
    for (const l of prevWarn) process.on("warning", l);
    rmSync(dir, { recursive: true, force: true });
  }
});
