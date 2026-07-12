/**
 * Bootstrap: everything `--import keel/hook` does, in one testable function.
 *
 * Order matters: policy → backend.configure → runtime state → fetch seam →
 * register the ESM loader (before the app + its deps import) → exit flush →
 * banner. When KEEL_DISABLE is set, this returns immediately with zero effects
 * so a run is byte-identical to one with no hook at all (DX invariant).
 *
 * Config errors (unparseable keel.toml, invalid policy) throw KEEL-E001 and are
 * intentionally fatal: a broken policy is a loud failure the user must fix, not
 * a silent fall-back to defaults.
 */

import { register } from "node:module";
import { loadPolicy, extractFunctionTargets } from "./policy.mjs";
import { loadBackend } from "./backend.mjs";
import { installFetch } from "./fetch.mjs";
import { createDiscovery } from "./discovery.mjs";
import { setRuntime } from "./runtime.mjs";
import { applyPackDefaults } from "./defaults.mjs";
import { resolveDevCache } from "./packs/llm.mjs";
import { installMcpPack } from "./packs/mcp.mjs";

export function isDisabled(env = process.env) {
  return isTruthy(env.KEEL_DISABLE);
}

let installed = false;

export async function installKeel({ cwd = process.cwd(), env = process.env } = {}) {
  if (isDisabled(env)) return { enabled: false, reason: "KEEL_DISABLE" };
  if (installed) return { enabled: true, reason: "already-installed" };
  installed = true;

  const { policy: raw, source } = loadPolicy(cwd); // throws KEEL-E001 on bad syntax
  // Layer the embedded pack defaults UNDER user config, then resolve the LLM
  // dev cache (mode:"dev" → concrete ttl off-prod, inert when KEEL_ENV=prod).
  // Both steps mirror the Python front end's policy-merge behavior exactly.
  const policy = resolveDevCache(applyPackDefaults(raw), env);
  const backend = await loadBackend({ preferred: env.KEEL_BACKEND });
  backend.configure(policy); // throws KEEL-E001 on invalid policy

  const discovery = createDiscovery(cwd);
  setRuntime({ enabled: true, backend, discovery });

  const uninstallFetch = installFetch(backend, discovery);
  // Framework packs: auto-detect and wrap MCP client transports if present.
  // Best-effort — an absent SDK is a silent no-op; never fatal.
  const mcp = await installMcpPack({ cwd });

  const functionTargets = extractFunctionTargets(policy);
  const wrappable = functionTargets.filter((t) => t.fn);
  if (wrappable.length > 0) {
    register("./loader.mjs", import.meta.url, {
      data: {
        functionTargets: wrappable,
        cwd,
        runtimeUrl: new URL("./loader-runtime.mjs", import.meta.url).href,
      },
    });
  }

  installExitFlush(discovery);
  banner(env, source, wrappable.length, mcp);
  return { enabled: true, backend, discovery, functionTargets, uninstallFetch, mcp };
}

function installExitFlush(discovery) {
  let flushed = false;
  process.once("exit", () => {
    if (flushed) return;
    flushed = true;
    try {
      discovery.flushSync();
    } catch {
      /* best-effort */
    }
  });
}

function banner(env, source, fnCount, mcp) {
  if (isTruthy(env.KEEL_QUIET)) return;
  const seams = ["global fetch"];
  if (fnCount > 0) seams.push(`${fnCount} function target${fnCount === 1 ? "" : "s"}`);
  if (mcp?.active) seams.push("mcp: transports");
  const policyDesc = source === "defaults" ? "production defaults" : `policy ${source}`;
  process.stderr.write(
    `keel ▸ wrapped ${seams.join(" + ")} with ${policyDesc} — \`keel init\` to customize\n`
  );
}

function isTruthy(v) {
  return v === "1" || v === "true" || v === "yes";
}
