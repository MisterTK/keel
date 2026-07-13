/**
 * `keel sim`: adapter-level fault injection driven by a declarative fault
 * plan (`KEEL_SIM_PLAN=<path>`, set by `keel sim <plan>` on the child).
 *
 * See `docs/sim-format.md` for the full (non-contract) plan format and the
 * architecture-spec §8 rationale. In short: this module wraps the ADAPTER's
 * `effect` closure — the callable `Backend.execute` calls once per Tier 1
 * attempt — never `execute`'s return value, so a scripted failure is
 * genuinely retried/backed-off/breaker-tripped by the real backend's own
 * resilience logic, exactly like a real failure would be. Every other
 * `Backend` member (`report`, `layer`, `persistent`) delegates straight
 * through, mirroring `record.mjs`'s `RecordingBackend`.
 */

import { readFileSync, writeFileSync, openSync, fsyncSync, closeSync } from "node:fs";

export const SIM_VERSION = 1;

/** 128 + SIGKILL(9) — the exit code a POSIX shell reports for a process a
 * real `kill -9` terminated (documented fallback on a platform with no
 * SIGKILL). */
export const SIM_CRASH_EXIT_CODE = 137;

/** Per-`kind` default HTTP status when the directive does not name one. */
const DEFAULT_STATUS = { "5xx": 503, "429": 429, http: 500 };

const TRUTHY = new Set(["1", "true", "yes"]);
function isTruthy(v) {
  return TRUTHY.has(String(v ?? "").trim().toLowerCase());
}

/** Hard-crash this process right now: a real, uncatchable `SIGKILL` sent to
 * our own pid — the closest in-process model of a real `kill -9` (module
 * docs). `process.exit` is deliberately NOT used: it runs `exit`/`beforeExit`
 * listeners (`installExitFlush`), which the simulation is specifically
 * trying to skip. */
export function defaultCrash() {
  try {
    process.kill(process.pid, "SIGKILL");
  } catch {
    /* no SIGKILL on this platform (native Windows) — fall through */
  }
  process.reallyExit ? process.reallyExit(SIM_CRASH_EXIT_CODE) : process.exit(SIM_CRASH_EXIT_CODE);
}

/**
 * Per-target consumed-directive counters, persisted to a JSON sidecar next
 * to the plan file (`<plan path>.cursor.json`) so a `"crash"` directive's
 * hard restart resumes the SAME fault sequence instead of replaying it from
 * the top. Best-effort: a read/write failure degrades to an in-memory
 * (non-persisted) cursor rather than breaking the simulated program.
 */
class Cursor {
  #path;
  #counts;
  constructor(path) {
    this.#path = path;
    try {
      const loaded = JSON.parse(readFileSync(path, "utf8"));
      this.#counts = loaded && typeof loaded === "object" && !Array.isArray(loaded) ? loaded : {};
    } catch {
      this.#counts = {};
    }
  }
  nextIndex(target) {
    return this.#counts[target] ?? 0;
  }
  bump(target) {
    this.#counts[target] = this.nextIndex(target) + 1;
    try {
      writeFileSync(this.#path, JSON.stringify(this.#counts));
      // fsync so the counter survives the crash we may be about to trigger.
      const fd = openSync(this.#path, "r");
      try {
        fsyncSync(fd);
      } finally {
        closeSync(fd);
      }
    } catch {
      /* best-effort bookkeeping only */
    }
  }
}

/** The directive `index` (0-based, across every attempt this target has ever
 * seen) selects, honoring each entry's `repeat` (default 1) — mirrors
 * `tools/faultproxy`'s Scenario cursor. `null` once the sequence is spent
 * (every further attempt passes through to the real effect live). */
function directiveAt(directives, index) {
  let remaining = index;
  for (const directive of directives) {
    const span = Math.max(1, Number(directive.repeat ?? 1));
    if (remaining < span) return directive;
    remaining -= span;
  }
  return null;
}

/** `null` → passthrough (call the real effect). `"crash"` → the caller must
 * hard-crash. Otherwise the synthetic attempt-result object to return
 * without ever calling the real effect. */
function resolve(directive) {
  const kind = directive.kind ?? "ok";
  if (kind === "ok") return null;
  if (kind === "crash") return "crash";
  if (kind === "conn") return { status: "error", class: "conn", message: "keel sim: injected connection failure" };
  if (kind === "timeout") return { status: "error", class: "timeout", message: "keel sim: injected timeout" };
  const status = Number(directive.status ?? DEFAULT_STATUS[kind] ?? 500);
  const result = { status: "error", class: "http", http_status: status, message: `keel sim: injected HTTP ${status}` };
  if ("retry_after_ms" in directive) result.retry_after_ms = directive.retry_after_ms;
  return result;
}

async function sleep(ms) {
  await new Promise((r) => setTimeout(r, ms));
}

/**
 * Wraps `inner` (a `Backend`): for every `execute` call, wraps the caller's
 * `effect` closure so a scripted fault plan can inject a failure/latency/
 * crash into one Tier 1 attempt without `inner` ever seeing anything but a
 * normal (possibly synthetic) attempt outcome — its own retry/backoff/
 * breaker/cache decisions run for real over it.
 */
export class SimBackend {
  #inner;
  #faults;
  #cursor;
  #crash;
  constructor(inner, faults, cursor, crash = defaultCrash) {
    this.#inner = inner;
    this.#faults = faults;
    this.#cursor = cursor;
    this.#crash = crash;
  }
  configure(policy) {
    return this.#inner.configure(policy);
  }
  #directivesFor(request) {
    const target = request && typeof request === "object" ? String(request.target ?? "") : "";
    return this.#faults[target] ?? null;
  }
  async execute(request, effect) {
    const directives = this.#directivesFor(request);
    if (directives === null) return this.#inner.execute(request, effect);
    const target = String(request.target ?? "");
    const wrapped = (attempt) => this.#apply(target, directives, attempt, effect);
    return this.#inner.execute(request, wrapped);
  }
  report() {
    return this.#inner.report();
  }
  get persistent() {
    return this.#inner.persistent === true;
  }
  layer(target, key) {
    return this.#inner.layer(target, key);
  }
  flushEvents() {
    this.#inner.flushEvents?.();
  }
  async #apply(target, directives, attempt, effect) {
    const index = this.#cursor.nextIndex(target);
    const directive = directiveAt(directives, index);
    this.#cursor.bump(target);
    if (directive === null) return effect(attempt);
    if (directive.delay_ms) await sleep(Math.max(0, Number(directive.delay_ms)));
    const outcome = resolve(directive);
    if (outcome === null) return effect(attempt);
    if (outcome === "crash") {
      this.#crash();
      throw new Error("unreachable: crash() must not return");
    }
    return outcome;
  }
}

function loadFaults(planPath) {
  let data;
  try {
    data = JSON.parse(readFileSync(planPath, "utf8"));
  } catch (err) {
    process.stderr.write(`keel ▸ KEEL_SIM_PLAN=${planPath} could not be read: ${err.message}\n`);
    process.exit(1);
  }
  const faults = data && typeof data === "object" ? data.faults : null;
  if (!faults || typeof faults !== "object") return {};
  const out = {};
  for (const [target, directives] of Object.entries(faults)) {
    if (Array.isArray(directives)) {
      out[target] = directives.filter((d) => d && typeof d === "object");
    }
  }
  return out;
}

/** Wrap `backend` for `keel sim <plan>` (`KEEL_SIM_PLAN=<planPath>`). */
export function installSim(backend, { planPath, env }) {
  const faults = loadFaults(planPath);
  const cursor = new Cursor(`${planPath}.cursor.json`);
  if (!isTruthy(env.KEEL_QUIET)) {
    process.stderr.write(`keel ▸ fault-simulating with ${planPath} — see docs/sim-format.md\n`);
  }
  return new SimBackend(backend, faults, cursor);
}
