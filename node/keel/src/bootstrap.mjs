/**
 * Bootstrap: everything `--import keelrun/hook` does, in one testable function.
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

import { createRequire, register } from "node:module";
import { existsSync } from "node:fs";
import { dirname, join } from "node:path";
import {
  loadPolicy,
  extractFunctionTargets,
  extractFlowEntrypoints,
  extractCmdFlows,
} from "./policy.mjs";
import { loadBackend } from "./backend.mjs";
import { installFetch } from "./fetch.mjs";
import { createDiscovery } from "./discovery.mjs";
import { createSummary, formatSummary, formatSummaryJson, keelOnPath } from "./summary.mjs";
import { emit, jsonLogs } from "./log.mjs";
import { setRuntime } from "./runtime.mjs";
import { applyPackDefaults } from "./defaults.mjs";
import { devCacheOffReason, resolveDevCache, serverlessMarker } from "./packs/llm.mjs";
import { installChildProcessPack } from "./packs/child-process.mjs";
import { installMcpPack } from "./packs/mcp.mjs";
import { installPgPack } from "./packs/pg.mjs";
import { installIoredisPack } from "./packs/ioredis.mjs";
import { installMysql2Pack } from "./packs/mysql2.mjs";
import { evePack } from "./packs/eve.mjs";
import { aiSdkPack } from "./packs/ai-sdk.mjs";
import { installRecording } from "./record.mjs";
import { installSim } from "./sim.mjs";

export function isDisabled(env = process.env) {
  return isTruthy(env.KEEL_DISABLE);
}

/**
 * Framework/library packs the bootstrap auto-detects and patches, in install
 * order. Each `install(opts)` is best-effort (never throws) and resolves to
 * `{ active: boolean, name?, uninstall? }` (`packs/mcp.mjs` etc.). Extend this
 * array — never special-case a new pack elsewhere in `installKeel` — when
 * adding another one; alphabetical by label to minimize merge conflicts.
 */
const FRAMEWORK_PACKS = [
  { label: "child_process", install: installChildProcessPack },
  { label: "ioredis", install: installIoredisPack },
  { label: "mcp: transports", install: installMcpPack },
  { label: "mysql2", install: installMysql2Pack },
  { label: "pg", install: installPgPack },
];

let installed = false;
let refused = null; // the refusal result, once printed — never print it twice

/**
 * This package's own version, for the JSON log lines' `version` field.
 *
 * Fail-open, deliberately: this is a module-load-time file read for a field
 * only `KEEL_LOG_FORMAT=json` ever uses, and Keel's activation contract is
 * fail-open. An unreadable/absent `package.json` (an exotic bundler, a
 * pruned install) must degrade this ONE field to "unknown", not throw out of
 * `import "./bootstrap.mjs"` and leave the app with no Keel at all — least
 * of all in the default text mode, which never reads it.
 */
function readVersion() {
  try {
    return createRequire(import.meta.url)("../package.json").version ?? "unknown";
  } catch {
    return "unknown";
  }
}

const VERSION = readVersion();

export function policyOptional(env = process.env) {
  return String(env?.KEEL_POLICY ?? "").trim().toLowerCase() === "optional";
}

export function missingPolicyError(root) {
  return (
    `keel ▸ error: KEEL_CWD=${root} is set but ${join(root, "keel.toml")} does not exist — ` +
    "Keel NOT activated; the app continues without keel " +
    "(set KEEL_POLICY=optional to run on production defaults instead)\n"
  );
}

export async function installKeel({ cwd = process.cwd(), env = process.env, cwdSource = "cwd" } = {}) {
  if (isDisabled(env)) return { enabled: false, reason: "KEEL_DISABLE" };
  if (refused) return { ...refused };
  if (installed) return { enabled: true, reason: "already-installed" };

  const { policy: raw, source } = loadPolicy(cwd); // throws KEEL-E001 on bad syntax
  if (source === "defaults" && cwdSource === "KEEL_CWD" && !policyOptional(env)) {
    const text = missingPolicyError(cwd);
    emit(env, text, {
      keel: "error",
      code: "policy-missing-at-keel-cwd",
      keel_cwd: String(cwd),
      message: text.slice("keel ▸ error: ".length).replace(/\n$/, ""),
      version: VERSION,
    });
    refused = { enabled: false, reason: "policy-missing-at-keel-cwd", root: cwd };
    return { ...refused };
  }
  installed = true;
  // Policy provenance, captured once: the two fields ("did my policy ship?",
  // "how many calls were served from cache?") that would have named the
  // 2026-09-15 outage in one log query (F10). Read again at exit by the flush.
  const meta = {
    keel_cwd: env.KEEL_CWD || null,
    policy_path: source === "defaults" ? null : join(cwd, "keel.toml"),
    policy_source: source,
    version: VERSION,
  };
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

  // `keel sim <plan>`: adapter-level fault injection driven by a declarative
  // plan (docs/sim-format.md), wired BEFORE the recording tee below so a run
  // that is somehow both simulated and recorded captures what actually
  // happened (including any injected faults).
  const simBackend = env.KEEL_SIM_PLAN
    ? installSim(backend, { planPath: env.KEEL_SIM_PLAN, env })
    : backend;

  // `keel record run`: tee every intercepted effect into a recording file
  // (docs/recording-format.md). A pure observer — never changes what a
  // wrapped call sees — so `effectiveBackend` (not `backend`) is what every
  // seam below actually wires up.
  const effectiveBackend = env.KEEL_RECORD
    ? installRecording(simBackend, {
        path: env.KEEL_RECORD,
        target: process.argv[1] ?? "",
        args: process.argv.slice(2),
        env,
      })
    : simBackend;

  // The explicit `[target."…"]` keys of the SAME effective policy the core
  // just configured — discovery's "wrapped" classification (dx-spec §2's
  // coverage gap) must agree with what actually applied.
  const knownTargets = new Set(Object.keys(policy.target ?? {}));
  // `[telemetry].console` (schema default true) gates the exit-time summary.
  // policy.mjs applies no schema defaults, so an absent table is `undefined`
  // here — only an explicit `false` turns it off. KEEL_QUIET silences it
  // exactly as it silences the banner.
  const consoleEnabled = policy.telemetry?.console !== false && !isTruthy(env.KEEL_QUIET);
  const summary = consoleEnabled ? createSummary() : null;
  const discovery = createDiscovery(cwd, { knownTargets, summary });
  setRuntime({ enabled: true, backend: effectiveBackend, discovery });

  // Outbound host/URL-pattern targets (docs/targeting.md) are resolved by the
  // backend itself (`backend.resolveTarget(...)`, configured with this same
  // effective policy just above) — `fetch`'s target judgment consults it
  // directly, so `[target."*.internal.corp"]`-style keys actually select
  // requests. The core still sees one exact key per call.
  const uninstallFetch = installFetch(effectiveBackend, discovery);
  // Framework/library packs: auto-detect and wrap each one if present.
  // Best-effort — an absent library is a silent no-op; never fatal. The
  // `child_process` pack additionally consumes the parsed `cmd:` flow rules
  // (`extractCmdFlows`, the `cmd:` sibling of `extractFlowEntrypoints`'s `ts:`
  // handling); other packs ignore the extra key.
  const cmdFlows = extractCmdFlows(policy);
  const packs = [];
  for (const { label, install } of FRAMEWORK_PACKS) {
    packs.push({ label, ...(await install({ cwd, cmdFlows })) });
  }
  // eve and ai-sdk are detection-only at this seam: neither patches anything
  // (eve's `tool:` targets are wrapped by the loader below, from source; the
  // ai-sdk seam is the user's own explicit `wrapLanguageModel` call — nothing
  // for the bootstrap to install). `detect()` never throws by construction,
  // but every pack surface stays best-effort here too — a detection bug must
  // never be fatal to `keel run`.
  let eveDetection = { matched: false };
  let aiSdkDetection = { matched: false };
  try {
    eveDetection = evePack({ cwd }).detect();
  } catch {
    /* best-effort — an eve-detection bug must never break the run */
  }
  try {
    aiSdkDetection = aiSdkPack({ cwd }).detect();
  } catch {
    /* best-effort — an ai-sdk-detection bug must never break the run */
  }

  const functionTargets = extractFunctionTargets(policy);
  const wrappable = functionTargets.filter((t) => t.fn);
  // Tier 2: `[flows] entrypoints` from the SAME effective policy — `hook.mjs`
  // matches `process.argv[1]` against these before Node loads it as the main
  // module (see `src/flow.mjs`'s module docs for why this must happen here,
  // before the normal ESM entry runs).
  const flowEntrypoints = extractFlowEntrypoints(policy);
  if (wrappable.length > 0 || eveDetection.matched) {
    register("./loader.mjs", import.meta.url, {
      data: {
        functionTargets: wrappable,
        cwd,
        runtimeUrl: new URL("./loader-runtime.mjs", import.meta.url).href,
        eveEnabled: eveDetection.matched,
      },
    });
  }

  installExitFlush(discovery, { backend: effectiveBackend, summary, meta, env });
  banner(env, source, wrappable.length, packs, eveDetection, aiSdkDetection, cwd, cwdSource);
  return {
    enabled: true,
    backend: effectiveBackend,
    discovery,
    functionTargets,
    flowEntrypoints,
    uninstallFetch,
    packs,
    eve: eveDetection,
    aiSdk: aiSdkDetection,
  };
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
export function installExitFlush(
  discovery,
  { proc = process, backend = null, summary = null, meta = null, env = process.env } = {}
) {
  let flushed = false;
  const flush = () => {
    if (flushed) return;
    flushed = true;
    try {
      discovery.flushSync();
    } catch {
      /* best-effort — discovery never throws into the user's program */
    }
    // The native engine's live NDJSON event feed (`.keel/events/`) flushes
    // its writer thread whenever the queue drains, which a long-lived
    // process never needs help with — but a short-lived script can exit
    // before its last few events land on disk. Best-effort: the JS engine
    // (non-native) backend has no such method.
    try {
      backend?.flushEvents?.();
    } catch {
      /* best-effort — event flush never throws into the user's program */
    }
    // The console summary (design spec Part A): its own counters only,
    // synchronous, never throws. After persistence so a formatter bug can
    // never cost a discovery write.
    try {
      if (summary) {
        if (jsonLogs(env)) {
          // Unconditional, unlike the text form: a zero line proves Keel was
          // live and intercepted nothing, which is exactly what the outage
          // post-mortem had no way to establish.
          proc.stderr.write(formatSummaryJson(summary.counts(), meta ?? {}));
        } else {
          const text = formatSummary(summary.counts(), keelOnPath());
          if (text) proc.stderr.write(text);
        }
      }
    } catch {
      /* observability never fails the process */
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

/**
 * #85: a process launched with cwd in a subdirectory (e.g. uvicorn-style
 * `cwd=agents/`, or here a Node server started from a nested working dir)
 * makes `loadPolicy(cwd)` find no `keel.toml` and fall back to Level 0
 * defaults — silently, since that fallback is normal for a genuinely
 * unconfigured project. This walk exists only to tell the two cases apart
 * for the banner: is there a `keel.toml` in one of the NEXT (at most
 * `maxLevels`) parent directories up from `cwd` that was never looked at?
 *
 * Fail-open by construction: any filesystem error while walking (a
 * permission problem, a vanished directory, …) returns null rather than
 * throwing — this is purely cosmetic (the banner), never load-bearing for
 * policy resolution, which already ran and already decided "defaults"
 * before this is called. Mirrors the Python front end's walk
 * (`python/keel/src/keel/bootstrap.py`) and the doctor bounded-parent-walk
 * convention (8 levels, stop at the filesystem root).
 */
function policyAboveCwd(cwd, maxLevels = 8) {
  try {
    let current = cwd;
    for (let i = 0; i < maxLevels; i++) {
      const parent = dirname(current);
      if (parent === current) return null; // reached the filesystem root
      if (existsSync(join(parent, "keel.toml"))) return parent;
      current = parent;
    }
  } catch {
    return null;
  }
  return null;
}

function banner(env, source, fnCount, packs, eve, aiSdk, cwd, cwdSource = "cwd") {
  if (isTruthy(env.KEEL_QUIET)) return;
  const seams = ["global fetch"];
  if (fnCount > 0) seams.push(`${fnCount} function target${fnCount === 1 ? "" : "s"}`);
  for (const p of packs) if (p.active) seams.push(p.label);
  if (eve?.matched) seams.push("eve tool modules");
  if (aiSdk?.matched) seams.push(`ai-sdk ${aiSdk.version ?? ""}`.trim());
  let desc = source === "defaults" ? "production defaults" : `policy ${join(cwd, "keel.toml")}`;
  // WHY the dev cache is off, read from the SAME resolution the cache itself
  // uses (`devCacheOffReason`), so the JSON form can carry it as a field:
  // "was the dev cache on in that container, and if not why" is one of the
  // questions the 2026-09-15 post-mortem had to answer by inference, and a
  // log pipeline can only index what the object names (F10). Note this is
  // WIDER than the banner's parenthetical, which stays marker-only prose
  // (byte-unchanged): an explicit `KEEL_ENV=prod` also turns the cache off,
  // and a field named `dev_cache_off` reporting null there would be a lie.
  const devCacheOff = devCacheOffReason(env);
  if (!String(env.KEEL_ENV ?? "").trim()) {
    const marker = serverlessMarker(env);
    if (marker !== null) desc = `${desc} (dev cache off: ${marker} detected)`;
  }
  const head = `keel ▸ wrapped ${seams.join(" + ")} with ${desc}`;
  // `note` is the text after the em-dash — the one place the tail variants
  // differ. Held as a value (rather than written inline per variant) so the
  // JSON form can carry it as a field; the text assembled below is
  // byte-identical to what each variant used to write directly.
  let note = null;
  if (source === "defaults") {
    // On the defaults path only (a real policy loaded means this cwd is
    // already the right one — zero cost there), check whether a keel.toml
    // exists somewhere above cwd that loadPolicy never looked at (#85). If
    // so, the usual "keel init to customize" nudge reads as if nothing is
    // wrong, when actually the adopter's policy silently never loaded — name
    // both paths and the fix instead.
    const found = policyAboveCwd(cwd);
    if (found) {
      note = `found keel.toml at ${found} but running from ${cwd}; set KEEL_CWD=${found} to load it`;
    } else if (cwdSource === "KEEL_CWD") {
      // Only reachable under KEEL_POLICY=optional (installKeel refuses otherwise).
      note = `KEEL_CWD=${cwd} is set but ${join(cwd, "keel.toml")} does not exist (KEEL_POLICY=optional)`;
    } else {
      note = `no keel.toml in ${cwd}; \`keel init\` to customize`;
    }
  }
  const text = note === null ? `${head}\n` : `${head} — ${note}\n`;
  const obj = {
    dev_cache_off: devCacheOff,
    keel: "activation",
    policy_path: source === "defaults" ? null : join(cwd, "keel.toml"),
    policy_source: source,
    root: String(cwd),
    root_source: cwdSource,
    version: VERSION,
    wrapped: seams.join(" + "),
  };
  if (note !== null) obj.note = note;
  emit(env, text, obj);
}

// Cross-language parity with the Python front end's `.strip().lower() in
// {"1","true","yes"}`, so `KEEL_DISABLE=" TRUE "` / `KEEL_QUIET=Yes` behave
// identically in both front ends.
function isTruthy(v) {
  return ["1", "true", "yes"].includes(String(v ?? "").trim().toLowerCase());
}
