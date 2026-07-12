/**
 * `keel/ai-sdk` — the Vercel AI SDK middleware seam.
 *
 *     import { wrapLanguageModel } from "ai";
 *     import { keelMiddleware } from "keel/ai-sdk";
 *     const model = wrapLanguageModel({ model: base, middleware: keelMiddleware() });
 *
 * Zero other code changes: every generate/stream on the wrapped model gets
 * Keel's `llm:<provider>` resilience (retry on 429/5xx/timeout with Retry-After
 * backoff, breaker, dev cache). See src/packs/ai-sdk.mjs for semantics.
 */

export { keelMiddleware } from "./src/packs/ai-sdk.mjs";
