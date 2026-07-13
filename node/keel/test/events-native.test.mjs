// Live event feed against the REAL native core (crates/keel-node): a project
// with a `.keel/` dir activates the engine-side NDJSON sink (keel-core's
// `events` module), so the Node front end inherits `keel tail`'s feed — and
// the trace refs on Tier 1 failure messages — with zero front-end plumbing.
//
// This file chdir's into a temp project dir; that is safe because `node
// --test` runs each test FILE in its own child process, and the engine reads
// its cwd once, at native-core construction.
//
// Auto-skips when the native addon is absent (build: `cargo build -p keel-node
// --release`).

import test from "node:test";
import assert from "node:assert/strict";
import { existsSync, mkdirSync, mkdtempSync, readFileSync, readdirSync, rmSync } from "node:fs";
import { tmpdir } from "node:os";
import { join } from "node:path";
import { loadBackend } from "../src/backend.mjs";
import { loaded as nativeLoaded } from "../../keel-core-native/index.mjs";

const gate = nativeLoaded
  ? {}
  : { skip: "keel-core-native binary absent — build with `cargo build -p keel-node --release`" };

/** Read every NDJSON event written for this run (single run file expected). */
function readFeed(eventsDir) {
  const files = existsSync(eventsDir)
    ? readdirSync(eventsDir).filter((f) => f.endsWith(".ndjson"))
    : [];
  if (files.length === 0) return { file: null, events: [] };
  assert.equal(files.length, 1, `one run file expected, got ${files.join(", ")}`);
  const text = readFileSync(join(eventsDir, files[0]), "utf8");
  const events = text
    .split("\n")
    .filter((line) => line !== "")
    .map((line) => JSON.parse(line));
  return { file: files[0], events };
}

/** The background writer flushes when its queue drains; poll briefly for it. */
async function awaitFeed(eventsDir, isComplete) {
  for (let i = 0; i < 300; i++) {
    const feed = readFeed(eventsDir);
    if (feed.events.length > 0 && isComplete(feed.events)) return feed;
    await new Promise((resolve) => setTimeout(resolve, 10));
  }
  return readFeed(eventsDir);
}

test(
  "native: a .keel project activates the live event feed and trace-ref'd failures",
  gate,
  async () => {
    const dir = mkdtempSync(join(tmpdir(), "keel-events-native-"));
    mkdirSync(join(dir, ".keel"));
    const prevCwd = process.cwd();
    delete process.env.KEEL_EVENTS; // exercise the `.keel`-dir activation path
    process.chdir(dir); // the engine resolves ./.keel at construction
    try {
      const backend = await loadBackend({
        preferred: "native",
        cwd: dir,
        env: { KEEL_JOURNAL: "" },
      });
      assert.equal(backend.kind, "native");
      backend.configure({
        target: {
          "api.flaky.internal": {
            retry: { attempts: 2, schedule: "exp(1ms, x2, max 10ms)" },
          },
        },
      });

      const outcome = await backend.execute(
        { v: 1, target: "api.flaky.internal", op: "GET api.flaky.internal", idempotent: true },
        // The napi bridge awaits the effect: it must return a Promise.
        async () => ({ status: "error", class: "timeout", message: "read timeout" })
      );

      // Invariant 4: the terminal failure message carries a resolvable ref.
      assert.equal(outcome.result, "error");
      assert.equal(outcome.error.code, "KEEL-E010");
      const match = outcome.error.message.match(/ trace: keel trace ([^ ]+)$/);
      assert.ok(match, `message must end in a trace ref: ${outcome.error.message}`);
      const [run, seq] = [
        match[1].slice(0, match[1].lastIndexOf("#")),
        Number(match[1].slice(match[1].lastIndexOf("#") + 1)),
      ];

      const eventsDir = join(dir, ".keel", "events");
      const { file, events } = await awaitFeed(eventsDir, (evs) =>
        evs.some((e) => e.event === "call_end")
      );
      assert.equal(file, `${run}.ndjson`, "the ref names the run's event file");

      // Header first, then the call's lifecycle, seq strictly increasing.
      assert.equal(events[0].event, "run_start");
      assert.equal(events[0].run, run);
      assert.ok(events.every((e, i) => e.v === 1 && e.seq === i));
      const anchor = events.find((e) => e.seq === seq);
      assert.equal(anchor?.event, "call_start", "the ref's seq is the call_start line");
      const kinds = events.filter((e) => e.call === anchor.call).map((e) => e.event);
      assert.deepEqual(kinds, [
        "call_start",
        "attempt_start",
        "attempt_error",
        "backoff",
        "attempt_start",
        "attempt_error",
        "call_end",
      ]);
    } finally {
      process.chdir(prevCwd);
      rmSync(dir, { recursive: true, force: true });
    }
  }
);
