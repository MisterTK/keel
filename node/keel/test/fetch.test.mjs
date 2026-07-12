// Global fetch interception against a scripted local http server (no external
// network). Uses a virtual-clock backend so backoff waits are instant while
// still recording waits_ms and driving the real fetch → server round-trip.

import test from "node:test";
import assert from "node:assert/strict";
import http from "node:http";
import { AsyncEngine, virtualClock } from "../src/engine.mjs";
import { installFetch } from "../src/fetch.mjs";
import { level0Defaults } from "../src/defaults.mjs";

/** Start a server whose handler is a scripted function of (req, hitCount). */
async function startServer(handler) {
  let hits = 0;
  const srv = http.createServer((req, res) => handler(req, res, ++hits));
  await new Promise((r) => srv.listen(0, "127.0.0.1", r));
  const port = srv.address().port;
  return {
    url: (path = "/") => `http://127.0.0.1:${port}${path}`,
    hits: () => hits,
    close: () => new Promise((r) => srv.close(r)),
  };
}

async function withKeel(server, fn) {
  const backend = new AsyncEngine(virtualClock());
  backend.configure(level0Defaults());
  const uninstall = installFetch(backend, null);
  try {
    return await fn(backend);
  } finally {
    uninstall();
    await server.close();
  }
}

test("idempotent GET survives 503-then-ok", async () => {
  const server = await startServer((_req, res, hit) => {
    if (hit === 1) {
      res.writeHead(503);
      res.end("upstream-down");
    } else {
      res.writeHead(200);
      res.end("ok");
    }
  });
  await withKeel(server, async () => {
    const resp = await fetch(server.url());
    assert.equal(resp.status, 200);
    assert.equal(await resp.text(), "ok");
    assert.equal(server.hits(), 2, "should have retried exactly once");
    assert.equal(resp.keelOutcome.attempts, 2);
    assert.equal(resp.keelOutcome.result, "ok");
  });
});

test("429 with Retry-After is respected", async () => {
  const server = await startServer((_req, res, hit) => {
    if (hit === 1) {
      res.writeHead(429, { "retry-after": "1" });
      res.end("slow down");
    } else {
      res.writeHead(200);
      res.end("ok");
    }
  });
  await withKeel(server, async () => {
    const resp = await fetch(server.url());
    assert.equal(resp.status, 200);
    assert.equal(resp.keelOutcome.attempts, 2);
    // wait = max(schedule 200ms, Retry-After 1000ms) = 1000ms
    assert.deepEqual(resp.keelOutcome.waits_ms, [1000]);
  });
});

test("POST is observed, not retried (KEEL-E014), original response returned", async () => {
  const server = await startServer((_req, res) => {
    res.writeHead(503);
    res.end("still-down");
  });
  await withKeel(server, async () => {
    const resp = await fetch(server.url(), { method: "POST", body: "payload" });
    assert.equal(resp.status, 503);
    assert.equal(await resp.text(), "still-down");
    assert.equal(server.hits(), 1, "non-idempotent call must not be retried");
    assert.equal(resp.keelOutcome.error.code, "KEEL-E014");
    assert.equal(resp.keelOutcome.attempts, 1);
  });
});

test("POST with an idempotency key is retried", async () => {
  const server = await startServer((_req, res, hit) => {
    if (hit === 1) {
      res.writeHead(503);
      res.end("down");
    } else {
      res.writeHead(200);
      res.end("done");
    }
  });
  await withKeel(server, async () => {
    const resp = await fetch(server.url(), {
      method: "POST",
      headers: { "Idempotency-Key": "abc-123" },
      body: "payload",
    });
    assert.equal(resp.status, 200);
    assert.equal(server.hits(), 2, "idempotency key makes the POST retryable");
    assert.equal(resp.keelOutcome.attempts, 2);
  });
});

test("args_hash is derived only for idempotent GET requests", async () => {
  const captured = [];
  const backend = {
    kind: "fake",
    configure() {},
    layer() {
      return undefined;
    },
    report() {
      return { v: 1, clock_ms: 0, targets: {} };
    },
    async execute(request, effect) {
      captured.push(request);
      const r = await effect(1);
      return {
        v: 1,
        result: "ok",
        payload: r.payload,
        attempts: 1,
        from_cache: false,
        waits_ms: [],
        throttled: false,
        throttle_wait_ms: 0,
        breaker: "closed",
        trace_id: "t-1",
      };
    },
  };
  const globalObj = { fetch: async () => new Response("ok", { status: 200 }) };
  installFetch(backend, null, { globalObj });

  await globalObj.fetch("http://api.example.com/x");
  await globalObj.fetch("http://api.example.com/x", { method: "POST", body: "b" });

  assert.match(captured[0].args_hash, /^[0-9a-f]{64}$/, "GET has a sha256 args_hash");
  assert.equal(captured[1].args_hash, null, "POST has no args_hash");
});

test("non-transient 4xx passes through unchanged (success path)", async () => {
  const server = await startServer((_req, res) => {
    res.writeHead(404);
    res.end("nope");
  });
  await withKeel(server, async () => {
    const resp = await fetch(server.url());
    assert.equal(resp.status, 404);
    assert.equal(await resp.text(), "nope");
    assert.equal(server.hits(), 1, "4xx is a real response, not a retryable failure");
    assert.equal(resp.keelOutcome.result, "ok");
    assert.equal(resp.keelOutcome.attempts, 1);
  });
});
