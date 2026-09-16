// src/log.mjs — `KEEL_LOG_FORMAT=json` line formatting.

import test from "node:test";
import assert from "node:assert/strict";
import { dumpsLine, jsonLogs } from "../src/log.mjs";

test("dumpsLine sorts nested object keys (parity with json.dumps sort_keys=True)", () => {
  assert.equal(dumpsLine({ b: { z: 1, a: 2 }, a: 0 }), '{"a":0,"b":{"a":2,"z":1}}\n');
});

test("dumpsLine leaves arrays and primitives unsorted/untouched", () => {
  assert.equal(dumpsLine({ b: [3, 1, 2], a: "x" }), '{"a":"x","b":[3,1,2]}\n');
  assert.equal(dumpsLine({ b: null, a: true, c: 1 }), '{"a":true,"b":null,"c":1}\n');
});

test("dumpsLine sorts objects nested inside arrays too", () => {
  assert.equal(
    dumpsLine({ items: [{ z: 1, a: 2 }] }),
    '{"items":[{"a":2,"z":1}]}\n'
  );
});

test("sortKeys leaves non-plain objects to JSON.stringify (#103)", () => {
  const d = new Date("2026-09-16T12:00:00.000Z");
  assert.equal(dumpsLine({ when: d, b: 1, a: 2 }), '{"a":2,"b":1,"when":"2026-09-16T12:00:00.000Z"}\n');
  assert.equal(dumpsLine({ m: new Map([["k", 1]]) }), '{"m":{}}\n', "a Map has no toJSON; {} is JSON.stringify's own answer, not ours");
  assert.equal(dumpsLine({ o: Object.create(null, { z: { value: 1, enumerable: true }, a: { value: 2, enumerable: true } }) }), '{"o":{"a":2,"z":1}}\n');
  assert.equal(dumpsLine({ arr: [{ z: 1, a: 2 }] }), '{"arr":[{"a":2,"z":1}]}\n');
});

test("only the exact value json switches the log format", () => {
  for (const v of ["json", "JSON", "  json  ", "Json"]) assert.equal(jsonLogs({ KEEL_LOG_FORMAT: v }), true, v);
  for (const v of ["", "1", "true", "ndjson", "text", "jsonl"]) assert.equal(jsonLogs({ KEEL_LOG_FORMAT: v }), false, v);
  assert.equal(jsonLogs({}), false);
});
