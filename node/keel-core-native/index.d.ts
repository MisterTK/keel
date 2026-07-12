/**
 * Types for the keel-core-native addon. Envelope shapes mirror
 * contracts/core_api.rs (serde field names), identical to node/keel-core-stub.
 */

export type ErrorClass = "conn" | "timeout" | "http" | "cancelled" | "other";

export interface Request {
  v: number;
  target: string;
  op?: string;
  idempotent?: boolean;
  args_hash?: string | null;
}

export type AttemptResult =
  | { status: "ok"; payload: unknown }
  | {
      status: "error";
      class: ErrorClass;
      http_status?: number;
      retry_after_ms?: number;
      message?: string;
      original?: unknown;
    };

export interface OutcomeError {
  code: string; // "KEEL-E0NN"
  class: ErrorClass;
  http_status?: number;
  message: string;
  original?: unknown;
}

export interface Outcome {
  v: number;
  result: "ok" | "error";
  payload?: unknown;
  error?: OutcomeError | null;
  attempts: number;
  from_cache: boolean;
  waits_ms: number[];
  throttled: boolean;
  throttle_wait_ms: number;
  breaker: "closed" | "open" | "half_open";
  trace_id: string;
}

export interface KeelCoreOptions {
  /** Harness-only: run on a paused virtual clock (enables deterministic waits). */
  paused?: boolean;
}

/**
 * Native keel-core. On a policy error, methods throw a standard JS `Error`
 * whose `.code` is the stable `"KEEL-E0NN"`.
 */
export declare class KeelCore {
  constructor(options?: KeelCoreOptions);
  configure(policy: Record<string, unknown>): void;
  /** Synchronous single call; `effect(attempt)` returns an attempt result. */
  execute(request: Request, effect: (attempt: number) => AttemptResult): Outcome;
  /** Async single call; `effect(attempt)` is awaited on the caller's loop. */
  executeAsync(
    request: Request,
    effect: (attempt: number) => Promise<AttemptResult>
  ): Promise<Outcome>;
  report(): Record<string, unknown>;
  /** Harness-only: advance the paused virtual clock by `ms` milliseconds. */
  advanceClock(ms: number): void;
}

/** True when the native addon binary loaded. */
export declare const loaded: boolean;
/** Alias of {@link KeelCore}, or `undefined` if the addon is not built. */
export declare const KeelCoreNative: typeof KeelCore | undefined;
