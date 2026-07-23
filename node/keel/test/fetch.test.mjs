// Global fetch interception against a scripted local http server (no external
// network). Uses a virtual-clock backend so backoff waits are instant while
// still recording waits_ms and driving the real fetch → server round-trip.

import test from "node:test";
import assert from "node:assert/strict";
import http from "node:http";
import { AsyncEngine, virtualClock } from "../src/engine.mjs";
import { installFetch } from "../src/fetch.mjs";
import { level0Defaults } from "../src/defaults.mjs";
import { resetLlmBudgets, recordSpend, spentCents } from "../src/llm-policy.mjs";

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

// --- idempotency-key injection (contracts/adapter-pack.md "Idempotency-key
// injection") -----------------------------------------------------------

async function withInjection(server, fn, { mintIdempotencyKey } = {}) {
  const backend = new AsyncEngine(virtualClock());
  backend.configure({
    ...level0Defaults(),
    target: { "127.0.0.1": { idempotency: { header: "Idempotency-Key" } } },
  });
  const uninstall = installFetch(backend, null, mintIdempotencyKey ? { mintIdempotencyKey } : {});
  try {
    return await fn(backend);
  } finally {
    uninstall();
    await server.close();
  }
}

test("POST with no caller key is injected one and retried (judgment flip)", async () => {
  const seen = [];
  const server = await startServer((req, res, hit) => {
    seen.push(req.headers["idempotency-key"]);
    if (hit === 1) {
      res.writeHead(503);
      res.end("down");
    } else {
      res.writeHead(200);
      res.end("done");
    }
  });
  await withInjection(server, async () => {
    const resp = await fetch(server.url(), { method: "POST", body: "payload" });
    assert.equal(resp.status, 200);
    assert.equal(server.hits(), 2);
    assert.equal(resp.keelOutcome.attempts, 2);
    assert.equal(seen.length, 2);
    assert.ok(seen[0], "a key was injected");
    assert.equal(seen[0], seen[1], "rule 2: the same minted key on every attempt");
  });
});

test("a caller-supplied key is never overwritten by injection", async () => {
  const seen = [];
  const server = await startServer((req, res, hit) => {
    seen.push(req.headers["idempotency-key"]);
    res.writeHead(hit === 1 ? 503 : 200);
    res.end(hit === 1 ? "down" : "done");
  });
  await withInjection(server, async () => {
    const resp = await fetch(server.url(), {
      method: "POST",
      headers: { "Idempotency-Key": "caller-key" },
      body: "payload",
    });
    assert.equal(resp.status, 200);
    assert.deepEqual(seen, ["caller-key", "caller-key"]);
  });
});

test("two logical calls mint distinct keys", async () => {
  const seen = [];
  const server = await startServer((req, res) => {
    seen.push(req.headers["idempotency-key"]);
    res.writeHead(200);
    res.end("ok");
  });
  await withInjection(server, async () => {
    await fetch(server.url(), { method: "POST", body: "a" });
    await fetch(server.url(), { method: "POST", body: "b" });
    assert.equal(seen.length, 2);
    assert.notEqual(seen[0], seen[1]);
  });
});

test("the mint source is injectable for deterministic tests", async () => {
  const seen = [];
  const server = await startServer((req, res) => {
    seen.push(req.headers["idempotency-key"]);
    res.writeHead(200);
    res.end("ok");
  });
  let n = 0;
  await withInjection(
    server,
    async () => {
      await fetch(server.url(), { method: "POST", body: "a" });
      assert.deepEqual(seen, ["fixed-1"]);
    },
    { mintIdempotencyKey: () => `fixed-${++n}` },
  );
});

test("no configured header means no injection: a bare POST is still not retried", async () => {
  const server = await startServer((_req, res) => {
    res.writeHead(503);
    res.end("still-down");
  });
  await withKeel(server, async () => {
    const resp = await fetch(server.url(), { method: "POST", body: "payload" });
    assert.equal(resp.keelOutcome.error.code, "KEEL-E014");
    assert.equal(server.hits(), 1);
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
    resolveTarget(method, host) {
      return host;
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

/** A fake backend that records each request envelope and forwards the effect.
 *  `layer`/`resolveTarget` delegate to a real `AsyncEngine` configured with
 *  `policy` (default empty), so target resolution in these tests exercises
 *  the actual backend algorithm — not a re-derived test double — while
 *  `execute` stays a simple capture-and-forward. */
function captureBackend(captured, policy = {}) {
  const engine = new AsyncEngine(virtualClock());
  engine.configure(policy);
  return {
    kind: "fake",
    configure() {},
    layer: (target, key) => engine.layer(target, key),
    resolveTarget: (method, host, scheme, port, path) => engine.resolveTarget(method, host, scheme, port, path),
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

// --- outbound host/URL-pattern targets (docs/targeting.md) -------------------
//
// Target resolution itself (the LLM host map, exact/pattern `[target]` keys,
// precedence, tie-break) is now the BACKEND's job (`backend.resolveTarget`,
// proven identical across native/stub/both front ends by conformance
// scenarios 36–38) — these tests pin only that `fetch.mjs` actually calls
// through to it with the right request fields and uses the result verbatim
// as the call's target.

test("installFetch: a policy pattern key becomes the call's target, verbatim", async () => {
  const captured = [];
  const globalObj = { fetch: async () => new Response("ok", { status: 200 }) };
  const policy = { target: { "GET api.catalog.internal/*": {} } };
  installFetch(captureBackend(captured, policy), null, { globalObj });

  await globalObj.fetch("https://api.catalog.internal/items/42");

  assert.equal(captured[0].target, "GET api.catalog.internal/*");
});

test("installFetch: no matching pattern falls back to the bare host (unchanged behavior)", async () => {
  const captured = [];
  const globalObj = { fetch: async () => new Response("ok", { status: 200 }) };
  const policy = { target: { "other.example.com": {} } };
  installFetch(captureBackend(captured, policy), null, { globalObj });

  await globalObj.fetch("https://api.example.com/x");

  assert.equal(captured[0].target, "api.example.com");
});

test("installFetch: the llm: host map still wins over an installed pattern", async () => {
  const captured = [];
  const globalObj = { fetch: async () => new Response("ok", { status: 200 }) };
  const policy = { target: { "*.openai.com": {} } };
  installFetch(captureBackend(captured, policy), null, { globalObj });

  await globalObj.fetch("https://api.openai.com/v1/chat/completions", { method: "POST" });

  assert.equal(captured[0].target, "llm:openai");
});

test("installFetch: an exact host key wins over a matching pattern", async () => {
  const captured = [];
  const globalObj = { fetch: async () => new Response("ok", { status: 200 }) };
  const policy = { target: { "api.internal": {}, "*.internal": {} } };
  installFetch(captureBackend(captured, policy), null, { globalObj });

  await globalObj.fetch("https://api.internal/anything");

  assert.equal(captured[0].target, "api.internal");
});

test("installFetch: with no [target] patterns configured, resolution is unchanged (bare host)", async () => {
  const captured = [];
  const globalObj = { fetch: async () => new Response("ok", { status: 200 }) };
  installFetch(captureBackend(captured), null, { globalObj });

  await globalObj.fetch("https://api.stripe.com/v1/charges");

  assert.equal(captured[0].target, "api.stripe.com");
});

// --- LLM budget caps + model fallback chains (llm-policy.mjs) ---------------

test("LLM budget: usage-based spend accrues and blocks the NEXT call before dispatch", async () => {
  resetLlmBudgets();
  let hits = 0;
  const globalObj = {
    fetch: async () => {
      hits++;
      return new Response(
        JSON.stringify({ reply: "hi", usage: { prompt_tokens: 1_000_000, completion_tokens: 1_000_000 } }),
        { status: 200, headers: { "content-type": "application/json" } }
      );
    },
  };
  const backend = new AsyncEngine(virtualClock());
  // gpt-4o: $2.50 in + $10.00 out per 1M tokens → one call costs $12.50, well over a $1 cap.
  backend.configure({ target: { "llm:openai": { budget: "$1.00/run", retry: { attempts: 1 } } } });
  installFetch(backend, null, { globalObj });
  const url = "https://api.openai.com/v1/chat/completions";
  const post = () =>
    globalObj.fetch(url, {
      method: "POST",
      headers: { "content-type": "application/json" },
      body: JSON.stringify({ model: "gpt-4o", messages: [] }),
    });

  const r1 = await post();
  assert.equal(r1.status, 200);
  assert.equal(hits, 1, "the first call is always dispatched (nothing spent yet)");
  assert.ok(spentCents("llm:openai") >= 100, "usage from the response was priced and recorded");

  await assert.rejects(post(), (e) => {
    assert.equal(e.code, "KEEL-E012");
    assert.match(e.message, /budget cap/i);
    assert.match(e.message, /llm:openai/);
    return true;
  });
  assert.equal(hits, 1, "the second call was blocked before dispatch — no second API call");
});

test("LLM budget: a pre-exhausted budget blocks the very first call", async () => {
  resetLlmBudgets();
  recordSpend("llm:openai", 5); // already over a 1-cent cap
  let hits = 0;
  const globalObj = { fetch: async () => { hits++; return new Response("{}", { status: 200 }); } };
  const backend = new AsyncEngine(virtualClock());
  backend.configure({ target: { "llm:openai": { budget: "$0.01/run" } } });
  installFetch(backend, null, { globalObj });
  await assert.rejects(
    () =>
      globalObj.fetch("https://api.openai.com/v1/chat/completions", {
        method: "POST",
        headers: { "content-type": "application/json" },
        body: JSON.stringify({ model: "gpt-4o" }),
      }),
    (e) => {
      assert.equal(e.code, "KEEL-E012");
      return true;
    }
  );
  assert.equal(hits, 0);
});

test("LLM budget: a target with no `budget` configured never reads the response body for accounting", async () => {
  resetLlmBudgets();
  const backend = new AsyncEngine(virtualClock());
  backend.configure({ target: { "llm:openai": { retry: { attempts: 1 } } } }); // no budget
  const globalObj = {
    fetch: async () => new Response(JSON.stringify({ usage: { prompt_tokens: 999_999_999 } }), { status: 200 }),
  };
  installFetch(backend, null, { globalObj });
  await globalObj.fetch("https://api.openai.com/v1/chat/completions", {
    method: "POST",
    headers: { "content-type": "application/json" },
    body: JSON.stringify({ model: "gpt-4o" }),
  });
  assert.equal(spentCents("llm:openai"), 0, "no budget configured → no accounting, ever");
});

test("LLM fallback: re-dispatches to the next model after a POST 503 (observed-not-retried, KEEL-E014)", async () => {
  resetLlmBudgets();
  const bodies = [];
  let hits = 0;
  const globalObj = {
    fetch: async (_url, init) => {
      hits++;
      bodies.push(init.body);
      const parsed = JSON.parse(init.body);
      if (parsed.model === "gpt-4o") return new Response("overloaded", { status: 503 });
      return new Response(JSON.stringify({ reply: "fallback-ok" }), {
        status: 200,
        headers: { "content-type": "application/json" },
      });
    },
  };
  const backend = new AsyncEngine(virtualClock());
  // attempts: 3 so a non-idempotent 503 fails as KEEL-E014 (observed, not
  // retried) rather than KEEL-E010 (which fires immediately when no retry
  // policy at all is configured, since the default attempt budget is then 1).
  backend.configure({ target: { "llm:openai": { retry: { attempts: 3 }, fallback: ["gpt-4o-mini"] } } });
  installFetch(backend, null, { globalObj });
  const resp = await globalObj.fetch("https://api.openai.com/v1/chat/completions", {
    method: "POST",
    headers: { "content-type": "application/json" },
    body: JSON.stringify({ model: "gpt-4o", messages: [] }),
  });
  assert.equal(resp.status, 200);
  assert.equal(hits, 2, "primary model failed, the fallback model was dispatched");
  assert.equal(JSON.parse(bodies[0]).model, "gpt-4o");
  assert.equal(JSON.parse(bodies[1]).model, "gpt-4o-mini", "the fallback hop rewrote the model field");
  assert.equal(resp.keelOutcome.result, "ok");
});

test("LLM fallback: chain exhausted delivers the LAST hop's failure", async () => {
  resetLlmBudgets();
  let hits = 0;
  const globalObj = {
    fetch: async () => {
      hits++;
      return new Response("still down", { status: 503 });
    },
  };
  const backend = new AsyncEngine(virtualClock());
  backend.configure({
    target: { "llm:openai": { retry: { attempts: 3 }, fallback: ["gpt-4o-mini", "gpt-4.1-mini"] } },
  });
  installFetch(backend, null, { globalObj });
  const resp = await globalObj.fetch("https://api.openai.com/v1/chat/completions", {
    method: "POST",
    headers: { "content-type": "application/json" },
    body: JSON.stringify({ model: "gpt-4o" }),
  });
  assert.equal(resp.status, 503, "the last hop's real response, unchanged");
  assert.equal(hits, 3, "primary + both chain entries were tried");
  assert.equal(resp.keelOutcome.error.code, "KEEL-E014");
});

test("LLM fallback: does NOT chase a budget-exceeded block (dx-spec: not on budget exhaustion)", async () => {
  resetLlmBudgets();
  recordSpend("llm:openai", 10);
  let hits = 0;
  const globalObj = { fetch: async () => { hits++; return new Response("{}", { status: 200 }); } };
  const backend = new AsyncEngine(virtualClock());
  backend.configure({ target: { "llm:openai": { budget: "$0.01/run", fallback: ["gpt-4o-mini"] } } });
  installFetch(backend, null, { globalObj });
  await assert.rejects(
    () =>
      globalObj.fetch("https://api.openai.com/v1/chat/completions", {
        method: "POST",
        headers: { "content-type": "application/json" },
        body: JSON.stringify({ model: "gpt-4o" }),
      }),
    (e) => {
      assert.equal(e.code, "KEEL-E012");
      return true;
    }
  );
  assert.equal(hits, 0, "the budget block happens before any hop is dispatched");
});

test("LLM fallback: an unrecognized request shape stops the chain and delivers the original failure", async () => {
  resetLlmBudgets();
  let hits = 0;
  const globalObj = {
    fetch: async () => {
      hits++;
      return new Response("still down", { status: 503 });
    },
  };
  const backend = new AsyncEngine(virtualClock());
  backend.configure({ target: { "llm:openai": { fallback: ["gpt-4o-mini"] } } });
  installFetch(backend, null, { globalObj });
  // A non-JSON body (no `model` field to rewrite) — rewriteModel returns null.
  const resp = await globalObj.fetch("https://api.openai.com/v1/chat/completions", {
    method: "POST",
    headers: { "content-type": "text/plain" },
    body: "not json",
  });
  assert.equal(resp.status, 503);
  assert.equal(hits, 1, "fallback could not rewrite the request, so it never re-dispatched");
});
