/**
 * Vercel eve pack (DX spec §4.2: "eve is TypeScript, filesystem-first (agents
 * are directories of tools/skills/subagents) ... wrap the tool modules eve
 * discovers and the MCP transports it calls").
 *
 * eve's own README (github.com/vercel/eve, as of 2026-06/07) documents a
 * fixed per-agent directory convention:
 *
 *     my-agent/
 *     └── agent/
 *         ├── agent.ts            # model + runtime config
 *         ├── instructions.md     # system prompt
 *         ├── tools/               <- typed functions, discovered by eve
 *         ├── skills/              # on-demand procedures
 *         ├── channels/            # message channels
 *         └── schedules/           # recurring jobs
 *
 * and a fixed tool-module shape:
 *
 *     import { defineTool } from "eve/tools";
 *     export default defineTool({
 *       description: "...",
 *       inputSchema: z.object({ ... }),
 *       async execute(args) { ... },
 *     });
 *
 * This module is the OTHER HALF of dx-spec §4.2's eve line — the MCP half is
 * already covered with zero eve-specific code: eve reaches the world through
 * the same `@modelcontextprotocol/sdk` `Client` that packs/mcp.mjs already
 * patches (`Client.prototype.request`), so eve's MCP round-trips are wrapped
 * the moment `installMcpPack` detects the SDK, regardless of eve being
 * present. What's missing is eve's OWN extension point: the `tools/` module
 * boundary.
 *
 * eve's tools are a `export default defineTool({...})` value, not a named
 * function export, so the existing `ts:` function-target rewrite (loader.mjs
 * `export function NAME` regex) does not apply verbatim. We reuse the SAME
 * mechanism — the ESM loader hook that rewrites a module's source text before
 * it is evaluated (module.register; see loader.mjs) — with a narrower,
 * eve-specific pattern: `transformEveTool` below detects the canonical
 * `import { defineTool } from "eve/tools"` line and rewrites it into a local
 * shim that hands eve back a `defineTool` whose `execute` routes through the
 * Keel backend first. Everything else in the module (description,
 * inputSchema, any other exports) passes through completely untouched — this
 * is a text-level rewrite, exactly like `ts:` targets are (loader.mjs's own
 * doc comment: "a documented simplification"), not a full TS/AST transform.
 *
 * v0.1 supports exactly the canonical form eve's own docs show — a bare,
 * unaliased `import { defineTool } from "eve/tools";` with no other named
 * imports on that line. Any other form (aliased, destructured differently,
 * `import * as eveTools`, dynamic `import()`) is left untouched: the module
 * runs unwrapped rather than have Keel guess at a rewrite (same "do nothing
 * if it can't be wrapped safely" hard rule as the rest of Level 0).
 *
 * Target = `tool:<name>`, `<name>` = the tool file's basename without
 * extension — eve's own convention already gives each tool its own file, so
 * the basename is a stable, human-legible id (dx-spec §4.1).
 *
 * Idempotency: UNLIKE a `ts:` target — where listing it in keel.toml IS the
 * user's explicit safety assertion (loader-runtime.mjs) — eve's tools are
 * discovered automatically with no such per-target consent. Per the Level 0
 * hard rule (never retry a non-idempotent call by default) and mirroring the
 * `mcp:` pack's `tools/call` treatment, every eve tool defaults to
 * non-idempotent: a failure is observed, not retried (KEEL-E014), unless the
 * user opts a specific `[target."tool:<name>"]` in with its own retry policy
 * (the same escape hatch loader-runtime.mjs documents for `ts:` targets that
 * want failures retried). Tool calls are never dev-cached (args_hash null —
 * they can be side-effecting).
 *
 * Explicitly NOT wrapped: eve's conversation-level durability (its own
 * checkpointing/resume across agent turns). dx-spec draws the line here:
 * "Keel doesn't compete with eve's conversation-level durability; it hardens
 * the effects inside each step, which eve doesn't do at call level."
 */

import { createRequire } from "node:module";
import { basename, join } from "node:path";

const PKG_SPECIFIER = "eve/package.json";

function resolveFrom(cwd, specifier) {
  // Resolve first from the user's project, then from Keel's own deps.
  try {
    return createRequire(join(cwd, "package.json")).resolve(specifier);
  } catch {
    try {
      return createRequire(import.meta.url).resolve(specifier);
    } catch {
      return null;
    }
  }
}

/** The `eve` pack — the four uniform operations (adapter-pack.md). */
export function evePack({ cwd = process.cwd() } = {}) {
  return {
    detect() {
      const pkgPath = resolveFrom(cwd, PKG_SPECIFIER);
      if (!pkgPath) return { matched: false };
      let version;
      try {
        version = createRequire(import.meta.url)(pkgPath)?.version;
      } catch {
        /* version unknown */
      }
      // Always best_effort: eve's tool-module convention is young (dx-spec
      // cites the project as of 2026-06) and this pack has no adapter-CI leg
      // certifying it against a real, pinned eve install — see contracts/
      // adapter-pack.md rule "pinned (version is covered by contract tests)".
      return { matched: true, name: "eve", version, confidence: "best_effort" };
    },
    seams() {
      return [
        {
          patchPoint: "agent/tools/*.ts module rewrite (defineTool's execute)",
          upstreamApi: 'eve/tools — defineTool({ execute }) (documented tools/ discovery convention)',
          whyStable:
            "eve's own documented tools/ directory + defineTool() convention; a source-level ESM rewrite (the same mechanism ts: function targets use), reversible (uninstall = remove keel)",
        },
      ];
    },
    targets() {
      return [
        {
          pattern: "tool:<name>",
          kind: "tool",
          idempotencyRule:
            "non-idempotent by default (eve discovers tools automatically — there is no per-target opt-in like ts: targets have): observed, not retried (KEEL-E014), mirroring the mcp: pack's tools/call default; a target may still opt in via its own retry.on",
          argsHashRule: "none (tool calls can be side-effecting; never dev-cached)",
        },
      ];
    },
    // No policy fragment of its own: tool: targets inherit [defaults.outbound].
    defaults() {
      return {};
    },
  };
}

/** `tool:<name>` from a module path/URL: the file's basename, extension-free. */
export function toolTargetFromPath(path) {
  const base = basename(path).replace(/\.[cm]?[jt]sx?$/, "");
  return `tool:${base}`;
}

// The exact, unaliased, single-import canonical form eve's README shows.
const DEFINE_TOOL_IMPORT_RE =
  /(^|\n)([ \t]*)import[ \t]*\{[ \t]*defineTool[ \t]*\}[ \t]*from[ \t]*["']eve\/tools["'];?/;

/**
 * Rewrite an eve tool module's source so its `defineTool(...)` call routes
 * through the Keel runtime before eve ever sees the definition. Returns
 * `null` when the module does not import the canonical, unaliased
 * `defineTool` from `"eve/tools"` (left completely untouched).
 */
export function transformEveTool(source, target, runtimeUrl) {
  if (!DEFINE_TOOL_IMPORT_RE.test(source)) return null;
  return source.replace(DEFINE_TOOL_IMPORT_RE, (_m, p1, p2) => {
    const lines = [
      `${p2}import { defineTool as __keel$realDefineTool } from "eve/tools";`,
      `${p2}import { wrapEveTool as __keel$wrapEveTool } from ${JSON.stringify(runtimeUrl)};`,
      `${p2}const defineTool = (def) => __keel$wrapEveTool(${JSON.stringify(target)}, __keel$realDefineTool, def);`,
    ];
    return p1 + lines.join("\n");
  });
}
