import test from "node:test";
import assert from "node:assert/strict";
import { KeelCoreStub, KeelError } from "../index.mjs";

const VERTEX = "us-central1-aiplatform.googleapis.com";
const FETCH = "/v1/projects/p/locations/us-central1/publishers/google/models/veo-3.1:fetchPredictOperation";
const SUBMIT = "/v1/projects/p/locations/us-central1/publishers/google/models/veo-3.1:predictLongRunning";
const ROUTE = "POST *-aiplatform.googleapis.com/*:fetchPredictOperation";
const poll = (until) => ({ interval: "10s", deadline: "90s", until });
const run = (core, op, idempotent, bodies) => {
  const script = [...bodies];
  return core.execute(
    { v: 1, target: "ops.internal", op, idempotent, args_hash: "h" },
    () => ({ status: "ok", payload: script.shift() }),
  );
};

test("route key beats the host map for its route only", () => {
  const core = new KeelCoreStub();
  core.configure({ target: { [ROUTE]: {}, "*.googleapis.com": {}, "*-aiplatform.googleapis.com/*": {} } });
  assert.equal(core.resolveTarget("POST", VERTEX, "https", null, FETCH), ROUTE);
  assert.equal(core.resolveTarget("GET", VERTEX, "https", null, FETCH), "*-aiplatform.googleapis.com/*"); // the method-less route key matches a GET; only the POST-prefixed key is method-gated
  assert.equal(core.resolveTarget("POST", VERTEX, "https", null, SUBMIT), "*-aiplatform.googleapis.com/*");
  assert.equal(core.resolveTarget("POST", "generativelanguage.googleapis.com", null, null, "/v1beta/m:generateContent"), "llm:google-genai");
  assert.equal(core.resolveTarget("GET", "storage.googleapis.com", null, null, "/b/x"), "*.googleapis.com");
  const plain = new KeelCoreStub();
  plain.configure({ target: { "api.example.com": {}, "GET api.example.com/*": {} } });
  assert.equal(plain.resolveTarget("GET", "api.example.com", null, null, "/v1/x"), "api.example.com");
});

test("boolean and numeric terminals match by JSON type and value", () => {
  const b = new KeelCoreStub();
  b.configure({ target: { "ops.internal": { poll: poll({ field: "done", terminal: [true] }) } } });
  const out = run(b, "POST ops.internal/op:fetchOperation", true, [{ done: false }, { done: "true" }, { done: 1 }, { done: true }]);
  assert.equal(out.attempts, 4);
  assert.deepEqual(out.payload, { done: true });
  const n = new KeelCoreStub();
  n.configure({ target: { "ops.internal": { poll: poll({ field: "progress", terminal: [100] }) } } });
  assert.equal(run(n, "GET ops.internal/op", true, [{ progress: 99 }, { progress: "100" }, { progress: true }, { progress: 100.0 }]).attempts, 4);
});

test("dotted field walks objects, fails open, and is printed verbatim in E016", () => {
  const core = new KeelCoreStub();
  core.configure({ target: { "ops.internal": { poll: { interval: "10s", deadline: "25s", until: { field: "response.state", terminal: ["SUCCEEDED"] } } } } });
  assert.equal(run(core, "GET ops.internal/op", true, [{ response: { state: "RUNNING" } }, { response: { state: "SUCCEEDED" } }]).attempts, 2);
  for (const body of [{ metadata: {} }, { response: "flat" }, { "response.state": "SUCCEEDED" }])
    assert.equal(run(core, "GET ops.internal/op", true, [body]).attempts, 1, JSON.stringify(body));
  const dead = run(core, "GET ops.internal/op", true, [{ response: { state: "R" } }, { response: { state: "R" } }, { response: { state: "R" } }]);
  assert.equal(dead.error.code, "KEEL-E016");
  assert.equal(dead.error.message, "GET ops.internal/op poll deadline exceeded: 'response.state' not terminal after 25000ms");
});

test("the gate is idempotency, not method", () => {
  const core = new KeelCoreStub();
  core.configure({ target: { "ops.internal": { poll: poll({ field: "status", terminal: ["done"] }) } } });
  assert.equal(run(core, "POST ops.internal/op:fetchOperation", true, [{ status: "running" }, { status: "done" }]).attempts, 2);
  assert.equal(run(core, "POST ops.internal/op:fetchOperation", false, [{ status: "running" }]).attempts, 1);
  assert.equal(run(core, "GET ops.internal/op", false, [{ status: "running" }]).attempts, 1);
});

test("validator: terminal items are strings, booleans, or numbers", () => {
  for (const good of [["a"], [true], [1], [1.5], ["a", false, 2]])
    new KeelCoreStub().configure({ target: { x: { poll: poll({ field: "f", terminal: good }) } } });
  for (const bad of [[], [null], [{}], [[1]], "done"]) {
    assert.throws(
      () => new KeelCoreStub().configure({ target: { x: { poll: poll({ field: "f", terminal: bad }) } } }),
      (e) => e instanceof KeelError && e.code === "KEEL-E001" &&
        e.message.includes("poll.until.terminal must be a non-empty array of strings, booleans, or numbers"),
    );
  }
});
