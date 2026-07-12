/**
 * Backend selection, isolated behind one seam (architecture §5.2, "the swap").
 *
 * Priority:
 *   1. native addon (`keel-core-native`) when loadable — the eventual napi core
 *      (Task 7/14). It is probed by dynamic import and must expose an async
 *      `execute`; it may not exist yet, so failure to load is normal.
 *   2. the in-repo Node backend (AsyncEngine over keel-core-stub semantics).
 *
 * `KEEL_BACKEND` overrides selection:
 *   - `stub`   → force the in-repo engine (never probe native)
 *   - `native` → require the native addon; throw KEEL-E040 if not loadable
 *   - unset    → auto (native if loadable, else engine)
 */

import { join } from "node:path";
import { AsyncEngine, realClock, KeelError } from "./engine.mjs";

const NATIVE_CANDIDATES = [
  "keel-core-native", // published/installed addon
  "../../keel-core-native/index.mjs", // in-repo sibling of node/keel (worktree), when built
];

function isTable(v) {
  return v !== null && typeof v === "object" && !Array.isArray(v);
}

/**
 * Front-end adapter over the napi `KeelCore`. The native surface differs from
 * the front end's contract in two ways, bridged here (Task 14 "the swap"):
 *   - the front end drives an ASYNC effect and `await`s `execute`; the native
 *     equivalent is `executeAsync` (tokio↔libuv). We map one to the other.
 *   - the front end reads policy layers via `layer(target, key)`; the native
 *     core does not expose it, so we resolve it here from the configured policy
 *     with the exact rule `AsyncEngine#layer` / the stub use (parity).
 * `persistent` reflects the native journal (the dev-cache `scope=persistent`).
 */
class NativeBackend {
  kind = "native";
  #core;
  #policy = {};
  constructor(core) {
    this.#core = core;
  }
  configure(policy) {
    this.#policy = isTable(policy) ? policy : {};
    this.#core.configure(policy);
  }
  execute(request, effect) {
    return this.#core.executeAsync(request, effect); // returns a Promise<Outcome>
  }
  report() {
    return this.#core.report();
  }
  get persistent() {
    return this.#core.persistent === true;
  }
  layer(target, key) {
    const t = this.#policy.target;
    if (isTable(t) && isTable(t[target]) && t[target][key] !== undefined) return t[target][key];
    const defaults = this.#policy.defaults ?? {};
    if (target.startsWith("llm:") && isTable(defaults.llm) && defaults.llm[key] !== undefined)
      return defaults.llm[key];
    return isTable(defaults.outbound) ? defaults.outbound[key] : undefined;
  }
}

async function tryLoadNative({ journalPath } = {}) {
  for (const spec of NATIVE_CANDIDATES) {
    let mod;
    try {
      mod = await import(spec);
    } catch {
      continue; // addon not present at this spec — try the next candidate
    }
    const Ctor = mod.KeelCoreNative ?? mod.default;
    if (typeof Ctor !== "function") continue;
    let core;
    try {
      core = journalPath ? new Ctor({ journalPath }) : new Ctor();
    } catch (err) {
      if (!journalPath) continue; // no-journal construction failed — genuinely unusable
      // The addon loaded but the journal could not open (unwritable/invalid path).
      // Degrade to an IN-MEMORY native core rather than downgrading to the stub —
      // resilience still comes from the real engine; only cross-run dev-cache
      // replay is lost. Mirrors the Python front end's graceful fallback.
      process.emitWarning(
        `keel: journal at ${journalPath} could not open (${err?.message ?? err}); ` +
          "native core running in-memory (no cross-run dev cache)",
        { code: "KEEL_JOURNAL_UNAVAILABLE" }
      );
      try {
        core = new Ctor();
      } catch {
        continue;
      }
    }
    if (typeof core.executeAsync === "function" && typeof core.configure === "function") {
      return new NativeBackend(core);
    }
  }
  return null;
}

/**
 * Where the native core attaches its journal (persistent dev cache + Tier 2).
 * `KEEL_JOURNAL` overrides the path; an explicit empty value disables it.
 */
function resolveJournalPath(cwd, env) {
  const override = env.KEEL_JOURNAL;
  if (override !== undefined) return override === "" ? null : override;
  return join(cwd, ".keel", "journal.db");
}

/**
 * Resolve the runtime backend. Returns an object exposing async
 * `execute(request, effect)`, `configure(policy)`, `layer(target, key)`,
 * `report()`, and `persistent`.
 */
export async function loadBackend({
  preferred = process.env.KEEL_BACKEND,
  clock,
  cwd = process.cwd(),
  env = process.env,
} = {}) {
  // Normalize + validate the selection, matching the Python twin: unset/empty →
  // "auto"; anything other than auto|native|stub is a loud KEEL-E040, never a
  // silent fall-back to auto.
  const choice = (preferred ?? "auto") || "auto";
  if (choice !== "auto" && choice !== "native" && choice !== "stub")
    throw new KeelError("KEEL-E040", `KEEL_BACKEND must be auto|native|stub, got ${JSON.stringify(choice)}`);

  if (choice !== "stub") {
    const native = await tryLoadNative({ journalPath: resolveJournalPath(cwd, env) });
    if (native) return native;
    if (choice === "native")
      throw new KeelError(
        "KEEL-E040",
        "KEEL_BACKEND=native requested but keel-core-native is not loadable"
      );
  }
  return new AsyncEngine(clock ?? realClock());
}
