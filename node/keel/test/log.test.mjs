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

test("only the exact value json switches the log format", () => {
  for (const v of ["json", "JSON", "  json  ", "Json"]) assert.equal(jsonLogs({ KEEL_LOG_FORMAT: v }), true, v);
  for (const v of ["", "1", "true", "ndjson", "text", "jsonl"]) assert.equal(jsonLogs({ KEEL_LOG_FORMAT: v }), false, v);
  assert.equal(jsonLogs({}), false);
});
