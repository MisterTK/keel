/** Public types for the `keel/ai-sdk` export. */

import type { Backend, Discovery } from "./index.js";

export interface KeelMiddlewareOptions {
  /** Backend to route through; defaults to the one installed by the hook. */
  backend?: Backend;
  /** Discovery recorder; defaults to the one installed by the hook. */
  discovery?: Discovery;
}

/**
 * A structural subset of the Vercel AI SDK `LanguageModelV2Middleware`
 * (mirrors ai@5.0.0). Only the two operations Keel implements are declared;
 * this is intentionally not a dependency on the `ai` package's types.
 */
export interface KeelLanguageModelMiddleware {
  wrapGenerate(options: {
    doGenerate: () => PromiseLike<unknown>;
    params: unknown;
    model: { provider?: string; modelId?: string };
  }): Promise<unknown>;
  wrapStream(options: {
    doStream: () => PromiseLike<unknown>;
    params: unknown;
    model: { provider?: string; modelId?: string };
  }): Promise<unknown>;
}

/**
 * Build the Keel `LanguageModelV2` middleware. Plug into `wrapLanguageModel`
 * from `ai`. When Keel is disabled/not installed, the middleware is a
 * transparent pass-through.
 */
export declare function keelMiddleware(options?: KeelMiddlewareOptions): KeelLanguageModelMiddleware;
