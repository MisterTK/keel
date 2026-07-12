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

/** A fake backend that records each request envelope and forwards the effect. */
function captureBackend(captured) {
  return {
    kind: "fake",
    configure() {},
    layer: () => undefined,
    report: () => ({ v: 1, clock_ms: 0, targets: {} }),
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
        trace_id: "t",
      };
    },
  };
}

test("LLM POST dev-cache replays an identical prompt through the fetch seam (0 extra API calls)", async () => {
  let hits = 0;
  const globalObj = {
    fetch: async () => {
      hits++;
      return new Response(JSON.stringify({ reply: "hi", served: hits }), {
        status: 200,
        headers: { "content-type": "application/json" },
      });
    },
  };
  const backend = new AsyncEngine(virtualClock());
  backend.configure({ target: { "llm:openai": { cache: { ttl: "24h" }, retry: { attempts: 1 } } } });
  installFetch(backend, null, { globalObj });
  const url = "https://api.openai.com/v1/chat/completions";
  const post = (body) =>
    globalObj.fetch(url, { method: "POST", headers: { "content-type": "application/json" }, body });

  const r1 = await post(JSON.stringify({ model: "gpt", messages: [{ role: "user", content: "hi" }], temperature: 0 }));
  assert.equal(r1.keelOutcome.from_cache, false);
  assert.equal(hits, 1, "first prompt hits the API once");
  // Key order + spacing differ, but the JSON is equivalent → same canonical hash → cache hit.
  const r2 = await post(JSON.stringify({ temperature: 0, messages: [{ content: "hi", role: "user" }], model: "gpt" }));
  assert.equal(r2.keelOutcome.from_cache, true, "identical prompt replays from the dev cache");
  assert.equal(r2.keelOutcome.attempts, 0, "a cache hit runs zero attempts");
  assert.equal(hits, 1, "the repeated LLM POST makes NO second API call");
  assert.deepEqual(await r2.json(), { reply: "hi", served: 1 });
});

test("LLM POST derives a canonical args_hash yet stays non-idempotent; non-LLM POST gets none", async () => {
  const captured = [];
  const globalObj = { fetch: async () => new Response("{}", { status: 200 }) };
  installFetch(captureBackend(captured), null, { globalObj });

  await globalObj.fetch("https://api.openai.com/v1/chat", { method: "POST", body: JSON.stringify({ a: 1, b: 2 }) });
  await globalObj.fetch("https://api.openai.com/v1/chat", { method: "POST", body: JSON.stringify({ b: 2, a: 1 }) });
  await globalObj.fetch("https://api.example.com/x", { method: "POST", body: JSON.stringify({ a: 1 }) });

  assert.match(captured[0].args_hash, /^[0-9a-f]{64}$/, "an LLM POST gets a dev-cache args_hash");
  assert.equal(captured[0].idempotent, false, "a cacheable LLM POST is still NOT retryable");
  assert.equal(captured[1].args_hash, captured[0].args_hash, "key order does not change the canonical hash");
  assert.equal(captured[2].args_hash, null, "a non-LLM POST has no args_hash");
});

test("idempotent PUT with a string body is retried and re-sends the body unchanged", async () => {
  const bodies = [];
  const server = await startServer((req, res, hit) => {
    let data = "";
    req.on("data", (c) => (data += c));
    req.on("end", () => {
      bodies.push(data);
      if (hit === 1) {
        res.writeHead(503);
        res.end("down");
      } else {
        res.writeHead(200);
        res.end("ok");
      }
    });
  });
  await withKeel(server, async () => {
    const resp = await fetch(server.url(), { method: "PUT", body: "the-payload" });
    assert.equal(resp.status, 200);
    assert.equal(server.hits(), 2, "PUT is idempotent → retried once");
    assert.deepEqual(bodies, ["the-payload", "the-payload"], "the body is re-sent unchanged on the retry");
  });
});

test("a caller abort is 'cancelled' — not retried, propagates the original AbortError at once", async () => {
  // A fake fetch that honors an AbortSignal exactly like the real one (rejects
  // with the signal's reason), but otherwise never resolves — so the ONLY way the
  // call ends is the caller's abort. If 'cancelled' were misclassified as
  // 'timeout' it would be retried through the whole backoff schedule.
  const globalObj = {
    fetch: (_input, init) =>
      new Promise((_resolve, reject) => {
        const sig = init?.signal;
        const fail = () => reject(sig?.reason ?? new DOMException("aborted", "AbortError"));
        if (sig?.aborted) return fail();
        sig?.addEventListener("abort", fail, { once: true });
      }),
  };
  const backend = new AsyncEngine(virtualClock());
  backend.configure(level0Defaults()); // outbound retry on conn/timeout/429/5xx
  installFetch(backend, null, { globalObj });

  const controller = new AbortController();
  const p = globalObj.fetch("http://api.example.com/x", { signal: controller.signal });
  controller.abort();
  let caught;
  try {
    await p;
  } catch (e) {
    caught = e;
  }
  assert.equal(caught?.name, "AbortError", "the original AbortError propagates unchanged");
  assert.equal(caught.keelOutcome.error.class, "cancelled");
  assert.equal(caught.keelOutcome.error.code, "KEEL-E015", "cancelled is terminal, not retried");
  assert.equal(caught.keelOutcome.attempts, 1, "no retries after a user abort");
  assert.deepEqual(caught.keelOutcome.waits_ms, [], "no backoff was waited");
});

test("an idempotent request with an unbuffered stream body is observed, not retried", async () => {
  const captured = [];
  const globalObj = { fetch: async () => new Response("{}", { status: 200 }) };
  installFetch(captureBackend(captured), null, { globalObj });

  const stream = new ReadableStream({
    start(c) {
      c.enqueue(new TextEncoder().encode("x"));
      c.close();
    },
  });
  // PUT is normally idempotent, but a stream body cannot be re-sent on a retry.
  await globalObj.fetch("https://api.example.com/x", { method: "PUT", body: stream, duplex: "half" });
  await globalObj.fetch("https://api.example.com/x", { method: "PUT", body: "buffered" });

  assert.equal(captured[0].idempotent, false, "a stream body → observed, not retried (can't wrap safely)");
  assert.equal(captured[1].idempotent, true, "an in-memory body stays retryable");
});
