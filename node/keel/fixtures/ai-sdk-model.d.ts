/**
 * PINNED FIXTURE — mirrors the Vercel AI SDK `LanguageModelV2` +
 * `LanguageModelV2Middleware` shapes as of **ai@5.0.0**
 * (content/docs/07-reference/01-ai-sdk-core/{60-wrap-language-model,
 * 65-language-model-v2-middleware}). The real `ai` package is NOT a dependency
 * of Keel; this is the frozen interface our middleware is contract-tested
 * against. If ai@ bumps the middleware shape, update this fixture + the version
 * in this comment and re-certify (adapter-pack contract: version-pinned tests).
 *
 * Only the members Keel relies on are declared. `test/ai-sdk.test.mjs` builds a
 * plain JS fake conforming to these shapes and drives keelMiddleware exactly as
 * `wrapLanguageModel` would.
 */

/** A generation/stream call's options; opaque to Keel except as a cache key. */
export type LanguageModelV2CallOptions = Record<string, unknown>;

export interface LanguageModelV2 {
  readonly specificationVersion: "v2";
  /** Provider id, e.g. "openai.chat", "anthropic.messages". */
  readonly provider: string;
  readonly modelId: string;
  doGenerate(options: LanguageModelV2CallOptions): Promise<Record<string, unknown>>;
  doStream(options: LanguageModelV2CallOptions): Promise<Record<string, unknown>>;
}

export interface LanguageModelV2Middleware {
  middlewareVersion?: "v2";
  wrapGenerate?(options: {
    doGenerate: () => ReturnType<LanguageModelV2["doGenerate"]>;
    doStream?: () => ReturnType<LanguageModelV2["doStream"]>;
    params: LanguageModelV2CallOptions;
    model: LanguageModelV2;
  }): Promise<Awaited<ReturnType<LanguageModelV2["doGenerate"]>>>;
  wrapStream?(options: {
    doGenerate?: () => ReturnType<LanguageModelV2["doGenerate"]>;
    doStream: () => ReturnType<LanguageModelV2["doStream"]>;
    params: LanguageModelV2CallOptions;
    model: LanguageModelV2;
  }): Promise<Awaited<ReturnType<LanguageModelV2["doStream"]>>>;
}
