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
  observe(target: string, host: string | null): void;
  flushSync(report: ReturnType<Backend["report"]>): boolean;
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
export declare const LLM_HOST_PROVIDERS: Readonly<Record<string, string>>;
export declare const VERSION: string;
