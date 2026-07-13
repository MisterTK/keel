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
  // Backend first: whether it's persistent (native + attached journal) decides
  // whether the LLM dev cache resolves to `scope="persistent"` (cross-run replay).
  const backend = await loadBackend({ preferred: env.KEEL_BACKEND, cwd, env });
  // Layer the embedded pack defaults UNDER user config, then resolve the LLM
  // dev cache (mode:"dev" → concrete ttl off-prod, inert when KEEL_ENV=prod;
  // scope=persistent when the backend can persist). Mirrors the Python front end.
  const policy = applyJournalEnvOverride(
    resolveDevCache(applyPackDefaults(raw), env, { persistent: backend.persistent }),
    env
  );
  backend.configure(policy); // throws KEEL-E001/KEEL-E005 on invalid/unsupported policy

  // The explicit `[target."…"]` keys of the SAME effective policy the core
  // just configured — discovery's "wrapped" classification (dx-spec §2's
  // coverage gap) must agree with what actually applied.
  const knownTargets = new Set(Object.keys(policy.target ?? {}));
  const discovery = createDiscovery(cwd, { knownTargets });
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

/**
 * `KEEL_JOURNAL` is the journal escape hatch: when it is set in the environment
 * (even to the empty string, which *disables* the journal), the construction-
 * time selection made from it wins over keel.toml's `journal` key. The core
 * honors the effective policy's `journal` at configure time, so the override is
 * composed here — the key is dropped before `configure`, leaving the
 * env-selected (or disabled) construction attachment in force. Precedence:
 * KEEL_JOURNAL (when set) > policy `journal` > `.keel/journal.db`. Mirrors the
 * Python front end exactly (parity). Exported for its unit test.
 */
export function applyJournalEnvOverride(policy, env) {
  if (!("KEEL_JOURNAL" in env) || !("journal" in policy)) return policy;
  const { journal: _dropped, ...rest } = policy;
  return rest;
}

/**
 * Persist buffered discovery on EVERY exit path — normal exit, an empty event
 * loop, and the signals a dev server is actually stopped with. `process.once`
 * only covers the 'exit' event, which does NOT fire under default SIGINT/SIGTERM
 * disposition, so a Ctrl-C'd Node server used to write nothing to discovery.db
 * for the whole session (the Python front end persists per call). Exported for
 * a child-process test.
 *
 * Signal handling preserves exit semantics: we flush, then either re-raise the
 * signal (when we are the only handler, so the default disposition still
 * terminates with code 128+signum) or step aside (when the app has its own
 * handler that owns termination). We never swallow the signal.
 */
export function installExitFlush(discovery, { proc = process } = {}) {
  let flushed = false;
  const flush = () => {
    if (flushed) return;
    flushed = true;
    try {
      discovery.flushSync();
    } catch {
      /* best-effort — discovery never throws into the user's program */
    }
  };
  proc.once("exit", flush); // normal exit / process.exit()
  proc.once("beforeExit", flush); // event loop drained without an explicit exit
  for (const sig of ["SIGINT", "SIGTERM", "SIGHUP"]) {
    proc.once(sig, () => {
      flush();
      // `once` has already removed THIS listener, so a zero count means no other
      // handler remains → re-raise for default termination (correct exit code).
      // A remaining handler (the app's own) owns exit; we only flushed.
      if (proc.listenerCount(sig) === 0) {
        try {
          proc.kill(proc.pid, sig);
        } catch {
          /* best-effort re-raise */
        }
      }
    });
  }
  return flush;
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

// Cross-language parity with the Python front end's `.strip().lower() in
// {"1","true","yes"}`, so `KEEL_DISABLE=" TRUE "` / `KEEL_QUIET=Yes` behave
// identically in both front ends.
function isTruthy(v) {
  return ["1", "true", "yes"].includes(String(v ?? "").trim().toLowerCase());
}
