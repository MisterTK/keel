/**
 * Public programmatic surface for the `keel` Node package (published as
 * `keelrun`). The primary entry is the preload hook (`node --import
 * keelrun/hook app.mjs`); this module exposes the
 * same bootstrap for embedding and the pieces useful to tooling/tests.
 */

export { installKeel, isDisabled } from "./src/bootstrap.mjs";
export { loadBackend } from "./src/backend.mjs";
export { loadPolicy, parseToml, extractFunctionTargets } from "./src/policy.mjs";
export { level0Defaults, applyPackDefaults } from "./src/defaults.mjs";
export { LLM_HOST_PROVIDERS } from "./src/judge.mjs";
export { KeelError } from "./src/engine.mjs";

// Framework/provider packs (adapter-pack contract).
export { keelMiddleware } from "./src/packs/ai-sdk.mjs";
export { llmPack, resolveDevCache, DEV_CACHE_TTL } from "./src/packs/llm.mjs";
export { mcpPack, installMcpPack, patchClientRequest } from "./src/packs/mcp.mjs";
export { toolPack, wrapTool, classifyToolError, isValidToolName, toolTarget } from "./src/packs/tool.mjs";

export const VERSION = "0.1.0";
