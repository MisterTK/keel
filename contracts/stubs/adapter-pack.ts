/**
 * Adapter-pack contract, TypeScript form — contracts-v1.
 *
 * See ../adapter-pack.md for semantics. Real packs live in-tree in keel-node;
 * this interface is the frozen shape they implement.
 */

export interface Detection {
  matched: boolean;
  /** e.g. "undici", "openai", "ai" (Vercel AI SDK) */
  name?: string;
  /** installed version, undefined if unknown */
  version?: string;
  confidence?: "pinned" | "best_effort";
}

export interface Seam {
  /** e.g. "undici.Dispatcher#dispatch" */
  patchPoint: string;
  /** the documented upstream API this relies on */
  upstreamApi: string;
  /** printed verbatim by `keel doctor` */
  whyStable: string;
}

export interface TargetDecl {
  /** target id or pattern, e.g. "llm:openai", "mcp:<server>" */
  pattern: string;
  kind: "host" | "function" | "llm" | "tool" | "mcp";
  /** how `idempotent` is derived at the seam */
  idempotencyRule: string;
  /** how `args_hash` is derived at the seam */
  argsHashRule: string;
}

/**
 * The four operations every pack implements. No retry/backoff/breaker logic
 * lives here — all behavior flows through the core.
 */
export interface AdapterPack {
  detect(): Detection;
  seams(): Seam[];
  targets(): TargetDecl[];
  /**
   * Policy fragment (keel.toml JSON form, per policy.schema.json), merged
   * UNDER user configuration.
   */
  defaults(): Record<string, unknown>;
}
