// The report page's pure view logic (crates/keel-cli/assets/report/report.js),
// loaded straight from the asset — no DOM, no build step. `render` is the
// only DOM-touching function and is exercised in a real browser by hand.

import test from "node:test";
import assert from "node:assert/strict";

await import(new URL("../../../crates/keel-cli/assets/report/report.js", import.meta.url));
const { viewModel, eventLine, mergePoll } = globalThis.KeelReport;

const T0 = 1_783_728_000_000; // day 20645
const state = {
  v: 1, generated_at_ms: T0, mode: "static", watch_interval_ms: 2000,
  status: {
    calls: 140, retries: 12, throttled: 3, breaker_opens: 1, cache_hits: 30, cache_hit_rate: 0.2143,
    unwrapped_calls: 5, not_retried: 1, week: { retries: 12 },
    flows: { total: 3, running: 1, completed: 1, failed: 0, resumable: 1, dead: 1 },
    targets: [
      { target: "api.example.com", calls: 100, unwrapped_calls: 0, retries: 12, not_retried: 1, throttled: 3, breaker_opens: 1, cache_hits: 10 },
      { target: "llm:openai", calls: 40, unwrapped_calls: 5, retries: 0, not_retried: 0, throttled: 0, breaker_opens: 0, cache_hits: 20 },
    ],
  },
  daily: [
    { target: "api.example.com", day: 20645, calls: 7, retries: 1, failures: 0 },
    { target: "llm:openai", day: 20645, calls: 3, retries: 0, failures: 1 },
    { target: "api.example.com", day: 20640, calls: 2, retries: 0, failures: 0 },
  ],
  run: { id: "0000000f00d-0001" },
  events: [
    { v: 1, seq: 0, ms: 0, event: "run_start", run: "0000000f00d-0001" },
    { v: 1, seq: 1, ms: 12, event: "call_start", call: "t-000001", target: "api.example.com", op: "GET api.example.com/items" },
    { v: 1, seq: 2, ms: 40, event: "mystery", call: "t-000001", target: "api.example.com" },
  ],
  events_seq: 2,
};

test("headline carries the value numbers and flags what is unprotected", () => {
  const vm = viewModel(state);
  const byLabel = Object.fromEntries(vm.headline.map((h) => [h.label, h]));
  assert.equal(byLabel.calls.value, 140);
  assert.equal(byLabel["absorbed rate limits"].value, 3);
  assert.equal(byLabel["retries"].value, 12);
  assert.equal(byLabel["cache hit rate"].value, "21.4%");
  assert.equal(byLabel["unprotected calls"].value, 5);
  assert.equal(byLabel["unprotected calls"].flag, true);
  assert.equal(byLabel["not retried"].flag, true);
});

test("rows are keyed by target with flagged cells", () => {
  const vm = viewModel(state);
  assert.deepEqual(vm.rows.map((r) => r.key), ["api.example.com", "llm:openai"]);
  assert.equal(vm.rows[1].flags.unwrapped, true);
  assert.equal(vm.rows[0].flags.notRetried, true);
  assert.equal(vm.rows[0].cells[7], "10.0%");
});

test("trend is exactly seven days ending today, summed across targets", () => {
  const vm = viewModel(state);
  assert.equal(vm.days.length, 7);
  assert.equal(vm.days[6].day, 20645);
  assert.deepEqual(vm.days[6], { day: 20645, calls: 10, retries: 1, failures: 1 });
  assert.deepEqual(vm.days[1], { day: 20640, calls: 2, retries: 0, failures: 0 });
  assert.deepEqual(vm.days[0], { day: 20639, calls: 0, retries: 0, failures: 0 });
});

test("flows are hidden when the journal has none", () => {
  assert.equal(viewModel(state).flows.total, 3);
  const none = { ...state, status: { ...state.status, flows: { total: 0 } } };
  assert.equal(viewModel(none).flows, null);
});

test("known event kinds render in tail's vocabulary; unknown kinds degrade to a generic line", () => {
  const lines = viewModel(state).events;
  assert.equal(lines.length, 3);
  assert.match(lines[0], /^00:00\.000  run 0000000f00d-0001/);
  assert.match(lines[1], /^00:00\.012  t-000001  api\.example\.com +call +GET api\.example\.com\/items/);
  assert.match(lines[2], /mystery/);
  assert.equal(eventLine({ v: 1, seq: 9 }), null); // no ms/event envelope
});

test("banner names the mode", () => {
  assert.match(viewModel(state).banner, /^Snapshot at /);
  assert.equal(viewModel({ ...state, mode: "serve" }).banner, "Live");
  assert.equal(viewModel({ ...state, mode: "watch" }).banner, "Live via --watch (reload every 2s)");
});

test("mergePoll: first poll adopts the run and keeps seq 0 (run_start)", () => {
  const cursor = { since: null, runId: null, events: [] };
  const resp = {
    run: { id: "0000000f00d-0001" },
    events: [
      { v: 1, seq: 0, ms: 0, event: "run_start", run: "0000000f00d-0001" },
      { v: 1, seq: 1, ms: 12, event: "call_start" },
      { v: 1, seq: 2, ms: 40, event: "mystery" },
    ],
    events_seq: 2,
  };
  const result = mergePoll(cursor, resp);
  assert.equal(result.render, true);
  assert.equal(result.refetchNow, false);
  assert.equal(result.cursor.runId, "0000000f00d-0001");
  assert.equal(result.cursor.since, 2);
  assert.equal(result.cursor.events.length, 3);
  assert.equal(result.cursor.events[0].seq, 0);
});

test("mergePoll: a run change mid-session resets the cursor and asks for an immediate re-poll", () => {
  const cursor = { since: 25, runId: "A", events: [{ seq: 24 }, { seq: 25 }] };
  const resp = { run: { id: "B" }, events: [{ seq: 0 }, { seq: 1 }], events_seq: 1 };
  const result = mergePoll(cursor, resp);
  assert.equal(result.render, false);
  assert.equal(result.refetchNow, true);
  assert.equal(result.cursor.runId, "B");
  assert.equal(result.cursor.since, null);
  assert.equal(result.cursor.events.length, 0);
});

test("mergePoll: same run appends new events and advances the cursor", () => {
  const cursor = { since: 2, runId: "0000000f00d-0001", events: [{ seq: 0 }, { seq: 1 }, { seq: 2 }] };
  const resp = { run: { id: "0000000f00d-0001" }, events: [{ seq: 3 }, { seq: 4 }], events_seq: 4 };
  const result = mergePoll(cursor, resp);
  assert.equal(result.render, true);
  assert.equal(result.refetchNow, false);
  assert.equal(result.cursor.events.length, 5);
  assert.equal(result.cursor.since, 4);
});

test("mergePoll: an events_seq of 0 is a real cursor, not a falsy no-op", () => {
  const cursor = { since: null, runId: null, events: [] };
  const resp = { run: { id: "R" }, events: [{ seq: 0 }], events_seq: 0 };
  const result = mergePoll(cursor, resp);
  assert.equal(result.cursor.since, 0);
  assert.notEqual(result.cursor.since, null);
});

test("mergePoll: a run:null response (evidence, no run yet) does not adopt a cursor; the first run then arrives in full", () => {
  const cursor = { since: null, runId: null, events: [] };
  const empty = { run: null, events: [], events_seq: 0 };
  const first = mergePoll(cursor, empty);
  assert.equal(first.cursor.since, null);
  assert.equal(first.cursor.runId, null);
  assert.equal(first.render, true);
  const runA = {
    run: { id: "A" },
    events: [{ seq: 0, event: "run_start" }, { seq: 1, event: "call_start" }, { seq: 2, event: "call_end" }],
    events_seq: 2,
  };
  const adopted = mergePoll(first.cursor, runA);
  assert.equal(adopted.cursor.runId, "A");
  assert.equal(adopted.cursor.since, 2);
  assert.deepEqual(adopted.cursor.events.map((e) => e.seq), [0, 1, 2]);
});
