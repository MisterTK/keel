/**
 * PINNED FIXTURE — mirrors the `Client` request boundary of
 * **@modelcontextprotocol/sdk@1.29.0**
 * (`@modelcontextprotocol/sdk/client/index.js`; `request` is inherited from
 * `Protocol`). The real SDK is NOT a dependency of Keel; this is the frozen
 * shape the `mcp:` pack patches (`Client.prototype.request`) and is
 * contract-tested against. If the SDK bumps this boundary, update this fixture +
 * the version here and re-certify (adapter-pack contract: version-pinned tests).
 *
 * Only the members the pack relies on are declared. `test/mcp.test.mjs` builds a
 * plain JS fake conforming to this shape (its `request` speaks newline JSON-RPC
 * to fixtures/fake-mcp-server.mjs over stdio).
 */

export interface McpRequest {
  method: string;
  params?: Record<string, unknown>;
}

export interface McpRequestOptions {
  /** Abort the in-flight request (SDK sends notifications/cancelled). */
  signal?: AbortSignal;
  /** Per-request timeout in ms (SDK's own; Keel imposes its own on top). */
  timeout?: number;
}

export interface McpImplementation {
  name: string;
  version: string;
}

export interface McpClient {
  /** The JSON-RPC request/response boundary the pack wraps. `resultSchema` is
   *  the caller's Zod schema; opaque to Keel. */
  request(request: McpRequest, resultSchema: unknown, options?: McpRequestOptions): Promise<unknown>;
  /** Connected server identity; the pack derives `mcp:<name>` from `.name`. */
  getServerVersion(): McpImplementation | undefined;
}
