import { test } from "node:test";
import assert from "node:assert/strict";
import { createCachePollDetector, MAX_FIRED, MAX_RUNS } from "../src/cachepoll.mjs";

const HIT = { v: 1, result: "ok", attempts: 0, from_cache: true };
const MISS = { v: 1, result: "ok", attempts: 1, from_cache: false };

function fakeClock() {
  const state = { t: 0 };
  const now = () => state.t;
  return { state, now };
}

function makeDetector() {
  const { state, now } = fakeClock();
  const fired = [];
  const d = createCachePollDetector({
    now,
    onSuspect: (t, h, s) => fired.push([t, h, s]),
  });
  const hits = (n, step, key = "h1") => {
    for (let i = 0; i < n; i++) {
      d.observe("llm:google-genai", key, HIT);
      state.t += step;
    }
  };
  return { d, state, fired, hits };
}

test("five regular hits over twenty seconds fire once", () => {
  const { d, fired, hits } = makeDetector();
  hits(5, 10.0);
  assert.deepEqual(fired, [["llm:google-genai", 5, 40]]);
  hits(20, 10.0);
  assert.equal(fired.length, 1, "once per key per process");
});

test("a burst does not fire", () => {
  const { fired, hits } = makeDetector();
  hits(50, 0.01); // a test suite replaying one prompt
  assert.deepEqual(fired, []);
});

test("a real fetch resets the run", () => {
  const { d, fired, hits } = makeDetector();
  hits(4, 10.0);
  d.observe("llm:google-genai", "h1", MISS);
  hits(4, 10.0);
  assert.deepEqual(fired, [], "never five consecutive");
});

test("keys are independent and null is ignored", () => {
  const { d, state, fired } = makeDetector();
  for (let i = 0; i < 10; i++) {
    d.observe("llm:openai", `h${i}`, HIT);
    state.t += 10.0;
  }
  assert.deepEqual(fired, [], "different prompts, not a poll");
  for (let i = 0; i < 10; i++) {
    d.observe("llm:openai", null, HIT);
    state.t += 10.0;
  }
  assert.deepEqual(fired, []);
});

test("eviction caps runs at MAX_RUNS and an active run near the cap still fires", () => {
  const { state, now } = fakeClock();
  const fired = [];
  const d = createCachePollDetector({ now, onSuspect: (t, h, s) => fired.push([t, h, s]) });
  for (let i = 0; i < MAX_RUNS + 50; i++) {
    d.observe("llm:openai", `h${i}`, HIT);
    state.t += 0.001;
  }
  assert.ok(d._debugSizes().runs <= MAX_RUNS);
  // An active run, touched most recently (so never the oldest-`last`
  // eviction candidate), must survive the churn above and still fire.
  for (let i = 0; i < 5; i++) {
    d.observe("llm:google-genai", "active", HIT);
    state.t += 10.0;
  }
  assert.deepEqual(fired, [["llm:google-genai", 5, 40]]);
});

test("fired set eviction caps growth", () => {
  const { state, now } = fakeClock();
  const d = createCachePollDetector({ now, minHits: 1, minSpanS: 0 });
  for (let i = 0; i < MAX_FIRED + 50; i++) {
    d.observe("llm:openai", `f${i}`, HIT);
    d.observe("llm:openai", `f${i}`, HIT); // second hit fires (minHits=1, span=0)
  }
  assert.ok(d._debugSizes().fired <= MAX_FIRED);
});
