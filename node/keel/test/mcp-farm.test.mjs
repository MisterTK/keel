// Adapter CI farm leg (sprint-plan.md "permanent roles": adapter CI farm —
// contract tests against pinned framework/library versions). Runs the mcp:
// pack's `patchClientRequest` against the REAL `@modelcontextprotocol/sdk`
// package (not the structural fake `mcp.test.mjs` uses), wired end-to-end over
// the SDK's own `InMemoryTransport` to a real `McpServer` — no network, no
// stdio subprocess, just the real client/server/transport/error classes.
//
// Opt-in via KEEL_ADAPTER_FARM=1: the real SDK is NOT a repo dependency (the
// zero-runtime-deps invariant, engineering-manifesto rule 12), so this file's
// tests are skipped in the default offline `node --test` run (the fast path
// mcp.test.mjs covers) and only run when
// .github/workflows/adapter-farm.yml has installed the pinned SDK version
// into an isolated node_modules (see that workflow for the exact version).
//
// The `{ skip }` option below is evaluated BEFORE the test body runs, so the
// dynamic imports of the real SDK never execute (and never throw
// MODULE_NOT_FOUND) unless the farm env var is set.

import test from "node:test";
import assert from "node:assert/strict";
import { patchClientRequest, classifyMcpError } from "../src/packs/mcp.mjs";
import { AsyncEngine, virtualClock } from "../src/engine.mjs";

const FARM = process.env.KEEL_ADAPTER_FARM === "1";
const skip = FARM ? false : "KEEL_ADAPTER_FARM=1 not set (offline fast path — see mcp.test.mjs)";

test("farm: real @modelcontextprotocol/sdk Client.request is patched and reversible", { skip }, async () => {
  const { Client } = await import("@modelcontextprotocol/sdk/client/index.js");
  assert.equal(typeof Client.prototype.request, "function", "real SDK still exposes Client.prototype.request");
  const uninstall = patchClientRequest(Client, {});
  assert.equal(Client.prototype.request.__keelWrapped, true, "the REAL Client.prototype.request is wrapped");
  uninstall();
  assert.equal(Client.prototype.request.__keelWrapped, undefined, "uninstall restores the real original");
});

test("farm: a tools/call round trip survives the real SDK by identity (echo)", { skip }, async () => {
  const { Client } = await import("@modelcontextprotocol/sdk/client/index.js");
  const { McpServer } = await import("@modelcontextprotocol/sdk/server/mcp.js");
  const { InMemoryTransport } = await import("@modelcontextprotocol/sdk/inMemory.js");
  const { z } = await import("zod");

  const server = new McpServer({ name: "farm-server", version: "1.0.0" });
  server.registerTool(
    "echo",
    { description: "echo", inputSchema: { text: z.string() } },
    async ({ text }) => ({ content: [{ type: "text", text }] })
  );

  const backend = new AsyncEngine(virtualClock());
  backend.configure({ target: { "mcp:farm-server": { retry: { attempts: 1 } } } });
  const uninstall = patchClientRequest(Client, { backend });
  try {
    const [clientTransport, serverTransport] = InMemoryTransport.createLinkedPair();
    const client = new Client({ name: "farm-client", version: "1.0.0" });
    await Promise.all([client.connect(clientTransport), server.connect(serverTransport)]);

    const res = await client.callTool({ name: "echo", arguments: { text: "hi" } });
    assert.deepEqual(res.content, [{ type: "text", text: "hi" }]);
    assert.equal(res.keelOutcome.result, "ok", "a successful real round trip is not turned into an error");
    assert.equal(res.keelOutcome.attempts, 1);
  } finally {
    uninstall();
  }
});

test("farm: a real protocol-level McpError (unknown method/resource) retries then surfaces (KEEL-E010)", { skip }, async () => {
  const { Client } = await import("@modelcontextprotocol/sdk/client/index.js");
  const { McpServer } = await import("@modelcontextprotocol/sdk/server/mcp.js");
  const { InMemoryTransport } = await import("@modelcontextprotocol/sdk/inMemory.js");
  const { McpError } = await import("@modelcontextprotocol/sdk/types.js");

  const server = new McpServer({ name: "farm-server", version: "1.0.0" });

  const backend = new AsyncEngine(virtualClock());
  backend.configure({
    target: { "mcp:farm-server": { retry: { attempts: 3, schedule: "fixed(1ms)", on: ["other"] } } },
  });
  const uninstall = patchClientRequest(Client, { backend });
  try {
    const [clientTransport, serverTransport] = InMemoryTransport.createLinkedPair();
    const client = new Client({ name: "farm-client", version: "1.0.0" });
    await Promise.all([client.connect(clientTransport), server.connect(serverTransport)]);

    let caught;
    try {
      // No resources are registered on this server — a real protocol-level
      // "method not found" McpError, not a tool-level `isError` result.
      await client.readResource({ uri: "farm://does-not-exist" });
    } catch (e) {
      caught = e;
    }
    assert.ok(caught instanceof McpError, `expected a real McpError, got ${caught?.constructor?.name}`);
    assert.equal(classifyMcpError(caught), "other");
    assert.equal(caught.keelOutcome.attempts, 3, "retried per policy against the REAL transport before giving up");
    assert.equal(caught.keelOutcome.error.code, "KEEL-E010", "retries exhausted against a real transport ends terminal");
    // `caught` being the real McpError instance (not a synthesized KeelError)
    // already proves identity is preserved — Keel never wraps the original.
  } finally {
    uninstall();
  }
});
