/** Envelope shapes mirror contracts/core_api.rs (serde field names). */

export type ErrorClass = "conn" | "timeout" | "http" | "cancelled" | "other";

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

export declare class KeelError extends Error {
  code: string;
}

export declare class KeelCoreStub {
  configure(policy: Record<string, unknown>): void;
  execute(request: Request, effect: (attempt: number) => AttemptResult): Outcome;
  report(): Record<string, unknown>;
  advanceClock(ms: number): void;
}

export declare const ENVELOPE_VERSION: number;
