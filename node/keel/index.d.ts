/** Public type surface for the `keel` Node package. */

export interface InstallOptions {
  cwd?: string;
  env?: Record<string, string | undefined>;
}

export interface InstallResult {
  enabled: boolean;
  reason?: string;
  backend?: Backend;
  discovery?: Discovery;
  functionTargets?: FunctionTarget[];
  uninstallFetch?: () => void;
  mcp?: McpInstallResult;
}

/** One adapter/framework pack (contracts/adapter-pack.md). */
export interface AdapterPack {
  detect(): { matched: boolean; name?: string; version?: string; confidence?: "pinned" | "best_effort" };
  seams(): { patchPoint: string; upstreamApi: string; whyStable: string }[];
  targets(): { pattern: string; kind: string; idempotencyRule: string; argsHashRule: string }[];
  defaults(): Record<string, unknown>;
}

export interface McpInstallResult {
  active: boolean;
  name?: string;
  uninstall?: () => void;
}

export interface FunctionTarget {
  target: string;
  glob: string;
  fn: string | null;
  skipped?: string;
}

/** One intercepted call submitted to the backend (mirrors core_api.rs Request). */
export interface Request {
  v: number;
  target: string;
  op?: string;
  idempotent: boolean;
  args_hash?: string | null;
}

export type AttemptResult =
  | { status: "ok"; payload: unknown }
  | {
      status: "error";
      class: "conn" | "timeout" | "http" | "cancelled" | "other";
      http_status?: number;
      retry_after_ms?: number;
      message?: string;
      original?: unknown;
    };

export interface Outcome {
  v: number;
  result: "ok" | "error";
  payload?: unknown;
  error?: {
    code: string;
    class: string;
    http_status?: number;
    message: string;
    original?: unknown;
  } | null;
  attempts: number;
  from_cache: boolean;
  waits_ms: number[];
  throttled: boolean;
  throttle_wait_ms: number;
  breaker: "closed" | "open" | "half_open";
  trace_id: string;
}

export interface Backend {
  kind: string;
  configure(policy: Record<string, unknown>): void;
  execute(request: Request, effect: (attempt: number) => Promise<AttemptResult>): Promise<Outcome>;
  layer(target: string, key: string): unknown;
  report(): { v: number; clock_ms: number; targets: Record<string, Record<string, unknown>> };
}

export interface Discovery {
  dbPath: string;
  /** Fold one intercepted call's Outcome into its target aggregate. */
  observe(target: string, outcome: Outcome, latencyMs?: number): void;
  /** Write accumulated aggregates to `.keel/discovery.db` (canonical schema). */
  flushSync(): boolean;
}

export declare class KeelError extends Error {
  code: string;
}

export declare function installKeel(options?: InstallOptions): Promise<InstallResult>;
export declare function isDisabled(env?: Record<string, string | undefined>): boolean;
export declare function loadBackend(options?: {
  preferred?: string;
  clock?: unknown;
}): Promise<Backend>;
export declare function loadPolicy(cwd?: string): {
  policy: Record<string, unknown>;
  source: string;
};
export declare function parseToml(text: string): Record<string, unknown>;
export declare function extractFunctionTargets(policy: Record<string, unknown>): FunctionTarget[];
export declare function level0Defaults(): Record<string, unknown>;
export declare function applyPackDefaults(policy: Record<string, unknown>): Record<string, unknown>;
export declare const LLM_HOST_PROVIDERS: Readonly<Record<string, string>>;

/** The `llm:` provider defaults pack (adapter-pack contract). */
export declare const llmPack: AdapterPack;
/** Dev-loop cache lifetime used when resolving `cache = { mode = "dev" }`. */
export declare const DEV_CACHE_TTL: string;
/** Resolve LLM dev caches: `mode:"dev"` → concrete ttl off-prod, inert in prod. */
export declare function resolveDevCache(
  policy: Record<string, unknown>,
  env?: Record<string, string | undefined>
): Record<string, unknown>;

/** The `tool:` semantic pack (adapter-pack contract). */
export declare const toolPack: AdapterPack;
/**
 * Wrap a tool callable as a `tool:<name>` target — the API framework packs
 * build on. A tool call is NON-idempotent by default (observed, never retried
 * — KEEL-E014); `idempotent: true` is the wrap site's opt-in for tools that
 * are safe to re-invoke. The wrapper is always async.
 */
export declare function wrapTool<A extends unknown[], R>(
  name: string,
  fn: (...args: A) => R | PromiseLike<R>,
  options?: { idempotent?: boolean; backend?: Backend; discovery?: Discovery }
): (...args: A) => Promise<R>;
/** Classify a thrown tool error into a core error class. */
export declare function classifyToolError(err: unknown): "timeout" | "cancelled" | "conn" | "other";
/** True iff `name` is a valid `tool:` name per the frozen target grammar. */
export declare function isValidToolName(name: unknown): boolean;
/** `tool:<name>`, validated against the frozen target grammar (throws KEEL-E001). */
export declare function toolTarget(name: string): string;

/** Build the `mcp:` transport pack bound to a project directory. */
export declare function mcpPack(options?: { cwd?: string }): AdapterPack;
/** Auto-detect the MCP client SDK and wrap its transports (best-effort). */
export declare function installMcpPack(options?: {
  cwd?: string;
  clientModule?: { Client?: unknown };
}): Promise<McpInstallResult>;
/** Patch a `Client` class's `request` method; returns an uninstall function. */
export declare function patchClientRequest(
  ClientClass: unknown,
  deps?: { backend?: Backend; discovery?: Discovery }
): () => void;

/** The Keel Vercel AI SDK middleware (also exported from `keel/ai-sdk`). */
export declare function keelMiddleware(options?: {
  backend?: Backend;
  discovery?: Discovery;
}): {
  wrapGenerate(options: { doGenerate: () => PromiseLike<unknown>; params: unknown; model: unknown }): Promise<unknown>;
  wrapStream(options: { doStream: () => PromiseLike<unknown>; params: unknown; model: unknown }): Promise<unknown>;
};

export declare const VERSION: string;
