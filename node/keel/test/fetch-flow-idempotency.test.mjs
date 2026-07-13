// End-to-end proof of contracts/adapter-pack.md "Idempotency-key injection"
// rule 3, through the REAL adapter (fetch.mjs + judge.mjs) and REAL native
// core — not a fake backend, and not the raw KeelCore binding test in
// flow.test.mjs, which pins the binding surface in isolation.
//
// A single fetch call's step is left `running` (injectRunningStep) — the
// process died after the (simulated) crashed attempt injected its key and
// sent its request, before the terminal outcome was recorded. On resume, the
// actual retried HTTP request that reaches the wire must carry the SAME
// Idempotency-Key header value the crashed attempt did — proven by
// inspecting the header a real local server actually received, not by
// reading the journal back. (Deliberately one step, not two: a completed
// step's *replay* rebuilding a Response from the journal is a separate,
// already-covered concern and orthogonal to this rule.)

import test from "node:test";
import assert from "node:assert/strict";
import http from "node:http";
import { mkdtempSync, rmSync } from "node:fs";
import { DatabaseSync } from "node:sqlite";
import { tmpdir } from "node:os";
import { join } from "node:path";
import { installFetch } from "../src/fetch.mjs";
import { loadBackend } from "../src/backend.mjs";
import { loaded as nativeLoaded } from "../../keel-core-native/index.mjs";

const gate = nativeLoaded
  ? {}
  : { skip: "keel-core-native binary absent — build with `cargo build -p keel-node --release`" };

/** Bare MessagePack for a small string->string map — see the identical
 *  helper's docstring in flow.test.mjs (parity with the Python twin's
 *  `_pack_bare_str_map`). Fixstr-only. */
function packBareStrMap(pairs) {
  const parts = [Buffer.from([0x80 | pairs.length])];
  for (const [k, v] of pairs) {
    for (const s of [k, v]) {
      const raw = Buffer.from(s, "utf8");
      if (raw.length >= 32) throw new Error("fixstr only; not needed for test-sized values");
      parts.push(Buffer.from([0xa0 | raw.length]), raw);
    }
  }
  return Buffer.concat(parts);
}

/** Directly journal a `running` step record carrying an idempotency key —
 *  models a crash mid-effect (see flow.test.mjs's identical helper for the
 *  full rationale). */
function injectRunningStep(journalPath, flowId, { seq, stepKey, idempotencyKey }) {
  const db = new DatabaseSync(journalPath);
  try {
    const payload = packBareStrMap([["idempotency_key", idempotencyKey]]);
    db.prepare(
      "INSERT INTO steps (flow_id, seq, step_key, kind, attempt, outcome, payload, error_class, started_at, ended_at) " +
        "VALUES (?,?,?,?,?,?,?,?,?,?)"
    ).run(flowId, seq, stepKey, "effect", 0, "running", payload, null, Date.now(), null);
  } finally {
    db.close();
  }
}

/** A tiny local HTTP server recording every request's headers. */
async function startCaptureServer() {
  const captured = [];
  const server = http.createServer((req, res) => {
    captured.push({ ...req.headers });
    res.writeHead(200, { "content-type": "text/plain" });
    res.end("ok");
  });
  await new Promise((r) => server.listen(0, "127.0.0.1", r));
  const port = server.address().port;
  return {
    url: (path = "/") => `http://127.0.0.1:${port}${path}`,
    captured,
    close: () => new Promise((r) => server.close(r)),
  };
}

test("native fetch: a resumed step injects the SAME key the crashed attempt did", gate, async (t) => {
  const dir = mkdtempSync(join(tmpdir(), "keel-fetch-idem-e2e-"));
  t.after(() => rmSync(dir, { recursive: true, force: true }));
  const srv = await startCaptureServer();
  t.after(() => srv.close());

  const entrypoint = "ts:pipeline.mjs#main";
  const argsHash = "ah-idem-e2e";
  const flowId = `${entrypoint}#${argsHash}#`;
  const journalPath = join(dir, ".keel", "journal.db"); // loadBackend's default location
  const policy = { defaults: { outbound: { idempotency: { header: "Idempotency-Key" } } } };

  // Open the flow once just to create the `flows` row (identity), then crash
  // immediately: step 1's `running` record (carrying the key a real adapter
  // would have injected before its request) is journaled directly — never
  // through a live call — modeling the process dying right after that write,
  // before any response was recorded.
  const backend1 = await loadBackend({ preferred: "native", cwd: dir });
  backend1.configure(policy);
  backend1.enterFlow(entrypoint, argsHash, {});
  const stepKey = "127.0.0.1#-"; // POST to a non-llm: target hashes to null -> "-"
  injectRunningStep(journalPath, flowId, { seq: 1, stepKey, idempotencyKey: "ik-crashed-e2e" });

  const backend2 = await loadBackend({ preferred: "native", cwd: dir });
  backend2.configure(policy);
  const info = backend2.enterFlow(entrypoint, argsHash, {});
  assert.equal(info.replay, false, "an uncompleted flow resumes live, not a pure replay");

  const uninstall = installFetch(backend2, null);
  try {
    const resp = await fetch(srv.url("/charge"), {
      method: "POST",
      body: JSON.stringify({ amount: 100 }),
      headers: { "content-type": "application/json" },
    });
    assert.equal(resp.status, 200);
  } finally {
    uninstall();
    backend2.exitFlow("completed");
  }

  assert.equal(srv.captured.length, 1, "the resumed step hit the network exactly once, live");
  // The load-bearing assertion: the header the resumed attempt actually sent
  // on the wire is IDENTICAL to the crashed attempt's key — not merely that
  // some Idempotency-Key header was present.
  assert.equal(srv.captured[0]["idempotency-key"], "ik-crashed-e2e");
});
