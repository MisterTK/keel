/**
 * Tier 2 durable-flow designation for `keel run` (dx-spec §1 Level 2,
 * architecture-spec §4.3-4.4). Mirrors `python/keel/src/keel/_flow.py`,
 * adapted to Node's file-path identity space and its ASYNC-ONLY intercepted
 * effects (unlike Python's v0.1 flows, Node's flow steps run through
 * `executeAsync` — see `backend.mjs`'s `NativeBackend.execute` and
 * `crates/keel-node/src/lib.rs`'s flow-aware `executeAsync`).
 *
 * When `keel run <script>` targets a module named by a `[flows] entrypoints`
 * `ts:<pathGlob>#<exportName>` entry, the front end runs that export *as a
 * durable flow*: it opens (or resumes) the flow through the native backend, so
 * every intercepted call inside is journaled and — on a rerun after a crash —
 * already-completed steps are substituted from the journal instead of re-fired.
 * `Date.now`/`Math.random` are virtualized inside the flow scope only, and
 * restored on exit.
 *
 * Tier 2 requires the native addon AND an attached journal: the pure-JS
 * `AsyncEngine` stub cannot journal/replay, and a native core with no journal
 * has nothing to resume from — either case is a precise, actionable error
 * (never a silent Tier-1 downgrade — a Level 0 surprise is a P0). Both gates
 * are checked *here*, before `enterFlow`, so the backend's last-resort
 * `KEEL-E040` ("pass a journalPath") is unreachable from `keel run`.
 *
 * # Why this always calls `process.exit()`
 *
 * `keel run` preloads this front end via `node --import keelrun/hook.mjs
 * <script>`. `--import` only PRELOADS a module before Node loads `<script>` as
 * its own main module — it does not take over execution the way Python's
 * `runpy.run_path` does. So once we have imported `<script>` ourselves and run
 * its designated export as a flow, we MUST terminate the process before
 * control returns to Node's normal module loader, or `<script>` would run a
 * SECOND time (once as our flow body, once as Node's own main module) — a
 * correctness bug, not a cosmetic one (it would double-fire every effect).
 * `runAsFlow` therefore never returns normally: it always calls `process.exit`
 * (success: 0; the flow's own `process.exit(n)` naturally wins if it runs
 * first) — this is the one honest simplification here, disclosed rather than
 * silently making the exit code a lossy stand-in for an uncaught exception's
 * usual formatting/trace (a real Node exception is still printed via
 * `console.error` first).
 */

import { readFileSync } from "node:fs";
import { createHash } from "node:crypto";
import { resolve as resolvePath } from "node:path";
import { pathToFileURL } from "node:url";
import { globToRegExp, relativePosix } from "./loader.mjs";

/** Front-end value-step keys (module-docs convention, mirroring Python's
 *  `py:time.time#-`): `ts:` is the Node front end's SAME prefix as its function
 *  targets (the frozen `entrypointRef` grammar has no separate `js:`). Niladic
 *  reads use a `-` args hash. */
const TIME_KEY = "ts:Date.now#-";
const RANDOM_KEY = "ts:Math.random#-";

/** Whether `backend` exposes the Tier 2 flow surface (native only). */
export function backendSupportsFlows(backend) {
  return typeof backend?.enterFlow === "function" && typeof backend?.exitFlow === "function";
}

/** Whether `backend` has a journal attached (the native `persistent` flag).
 *  Tier 2 replay lives in that journal; a native core with none cannot resume. */
export function backendHasJournal(backend) {
  return backend?.persistent === true;
}

function matchesGlob(glob, rel, base) {
  const re = globToRegExp(glob);
  return re.test(rel) || re.test(base);
}

/**
 * The flow entrypoint whose module path matches the `target` script, if any.
 * CONCRETE entries (no `*` in the glob) always win over glob entries, then
 * declaration order — mirroring `extractFunctionTargets`'s own precedence.
 *
 * A glob match is resolved to the CONCRETE relative path that matched (`raw`
 * becomes `ts:<relPath>#<fn>`, `via` records the designating glob) — so two
 * different scripts under one glob never share a flow identity, exactly the
 * property `_flow.py`'s `match_flow` establishes for Python's dotted modules.
 */
export function matchFlow(targetPath, cwd, entrypoints) {
  if (!entrypoints || entrypoints.length === 0) return null;
  let rel;
  let base;
  try {
    const abs = resolvePath(cwd, targetPath).replace(/\\/g, "/");
    rel = relativePosix(cwd.replace(/\\/g, "/"), abs);
    base = abs.slice(abs.lastIndexOf("/") + 1);
  } catch {
    return null;
  }
  const concrete = entrypoints.filter((e) => !e.glob.includes("*"));
  const globs = entrypoints.filter((e) => e.glob.includes("*"));
  for (const e of concrete) {
    if (e.glob === rel || e.glob === base || matchesGlob(e.glob, rel, base)) return e;
  }
  for (const e of globs) {
    if (matchesGlob(e.glob, rel, base)) {
      return { raw: `ts:${rel}#${e.fn}`, glob: rel, fn: e.fn, via: e.raw };
    }
  }
  return null;
}

/** A stable hash of the flow's CLI arguments — part of its identity, so a
 *  rerun with the same args resumes the same flow. */
function argsHash(args) {
  return createHash("sha256").update(JSON.stringify([...args])).digest("hex").slice(0, 16);
}

/** A hash of the flow script's source, fencing replay across code changes (a
 *  changed deploy is expected to diverge; spec §4.4). `undefined` if unreadable. */
function codeHash(targetPath) {
  try {
    return createHash("sha256").update(readFileSync(targetPath)).digest("hex").slice(0, 16);
  } catch {
    return undefined;
  }
}

/**
 * Patch `Date.now`/`Math.random` to journal-backed values for the duration of
 * a flow, returning a restore function. On replay the backend substitutes the
 * recorded value, so a resumed flow observes the same clock and randomness it
 * did on its first run.
 *
 * `journalTime`/`journalRandom` are SYNCHRONOUS napi calls (`Date.now` and
 * `Math.random` are synchronous JS builtins and cannot be made to return a
 * `Promise` without breaking every caller) — see their docs in
 * `crates/keel-node/src/lib.rs` for why a read racing a concurrently in-flight
 * effect step passes through unjournaled rather than risking a same-thread
 * deadlock. A read that happens *inside* a running effect is likewise NOT
 * journaled by the core (it passes the live value straight through) — only the
 * flow's own top-level reads between steps become value steps.
 */
function virtualizeTimeRandom(backend) {
  const origNow = Date.now;
  const origRandom = Math.random;
  Date.now = () => Number(backend.journalTime(TIME_KEY, origNow()));
  Math.random = () => {
    const drawn = Buffer.alloc(8);
    drawn.writeDoubleLE(origRandom(), 0);
    const recorded = Buffer.from(backend.journalRandom(RANDOM_KEY, drawn));
    return recorded.readDoubleLE(0);
  };
  return function restore() {
    Date.now = origNow;
    Math.random = origRandom;
  };
}

function isTruthy(v) {
  return ["1", "true", "yes"].includes(String(v ?? "").trim().toLowerCase());
}

/** Stamp the flow's terminal `status`, degrading a journal-WRITE failure
 *  (issue #14: `exitFlow` can now throw a `KEEL-E040` when the journal write
 *  itself fails) to a stderr line instead of letting it become a new
 *  exception thrown out of `runAsFlow`'s `catch` block. Every caller of this
 *  helper is already handling the flow body's OWN outcome (a real exception,
 *  the one this module's docs promise is "still printed via `console.error`
 *  first") — a *second, unrelated* journal error thrown here would replace
 *  it before that `console.error` ever runs, discarding the actual bug
 *  behind a generic Node "uncaught (in promise)" dump of the journal error
 *  instead. The write failure is still reported — never silently
 *  swallowed — just not as a stand-in for whatever the caller is handling. */
function exitFlowOrWarn(backend, status) {
  try {
    backend.exitFlow(status);
  } catch (err) {
    const code = err?.code ?? "KEEL-E040";
    const message = err?.message ?? String(err);
    process.stderr.write(`keel ▸ ${code}: flow terminal status not journaled: ${message}\n`);
  }
}

/** Emit the precise what/why/next error (KEEL-E005) for a flow under a
 *  non-native backend and terminate (Tier 2 requires the native core). The
 *  policy is valid; the capability is missing — unsupported-configuration,
 *  not E001. */
function unsupportedOnStub(entry) {
  process.stderr.write(
    `keel ▸ KEEL-E005: Tier 2 durable flow ${JSON.stringify(entry.raw)} needs the native core.\n` +
      "  why:  crash-safe resume journals and replays each step; the pure-JS stub backend cannot do that.\n" +
      "  next: build the native addon (`cargo build -p keel-node --release`) or set KEEL_BACKEND=native, then re-run.\n"
  );
  process.exit(1);
}

/** Emit the precise config-level error (KEEL-E005, unsupported-configuration)
 *  for a native backend with no journal, and terminate. Checked *before*
 *  `enterFlow`, so the backend's last-resort `KEEL-E040` is never reached from
 *  `keel run`. */
function unsupportedWithoutJournal(entry) {
  process.stderr.write(
    `keel ▸ KEEL-E005: durable flow ${JSON.stringify(entry.raw)} needs a journal, but none is attached.\n` +
      "  why:  Tier 2 journals and replays each step; with no journal there is nothing to record to or resume from.\n" +
      "  next: let the native core open .keel/journal.db (check KEEL_JOURNAL and directory permissions), or remove this entrypoint from [flows].\n"
  );
  process.exit(1);
}

/**
 * Run `entry`'s export as a durable flow through `backend`. Opens/resumes the
 * flow, runs the body with `Date.now`/`Math.random` virtualized, stamps the
 * terminal status, and — always — terminates the process (see module docs for
 * why). Never returns.
 */
export async function runAsFlow(targetPath, entry, backend, args, { env = process.env } = {}) {
  if (!backendSupportsFlows(backend)) unsupportedOnStub(entry); // terminates
  if (!backendHasJournal(backend)) unsupportedWithoutJournal(entry); // terminates

  const abs = resolvePath(process.cwd(), targetPath);
  const mod = await import(pathToFileURL(abs).href);
  const fn = mod[entry.fn];
  if (typeof fn !== "function") {
    const designated = entry.via ? ` (designated by [flows] glob ${JSON.stringify(entry.via)})` : "";
    process.stderr.write(
      `keel ▸ KEEL-E040: flow entrypoint ${JSON.stringify(entry.raw)}${designated} names ` +
        `${JSON.stringify(entry.fn)}, which is not an exported function of ${targetPath}` +
        `${entry.via ? "; add it or narrow the glob" : ""}.\n`
    );
    process.exit(1);
    return; // unreachable; keeps the type of `fn` narrowed for lint/readers
  }

  let info;
  try {
    info = backend.enterFlow(entry.raw, argsHash(args), {
      codeHash: codeHash(abs),
      leaseMs: env.KEEL_FLOW_LEASE_MS ? Number(env.KEEL_FLOW_LEASE_MS) : undefined,
    });
  } catch (err) {
    // A lease held by a live holder (E030), dead (E032), or no journal (E040).
    const code = err?.code ?? "KEEL-E040";
    const message = err?.message ?? String(err);
    process.stderr.write(`keel ▸ ${code}: ${message}\n`);
    process.exit(1);
    return;
  }

  const replayed = Boolean(info.replay);
  const verb = replayed ? "replaying completed" : "running";
  if (!isTruthy(env.KEEL_QUIET)) {
    process.stderr.write(`keel ▸ ${verb} flow ${entry.raw} [${info.flow_id}]\n`);
  }

  const restore = virtualizeTimeRandom(backend);
  try {
    await fn();
  } catch (err) {
    restore();
    // The flow body's own exception is printed FIRST, unconditionally — see
    // `exitFlowOrWarn`'s docs for why this must not be reordered after the
    // (now-fallible) terminal-status write.
    console.error(err);
    // Never demote an already-COMPLETED (replayed) flow to `failed` — a
    // replay-miss (KEEL-E031) after a code change, or any error while
    // re-running finished code, must not re-open a done flow for live
    // re-execution (nor march it toward `dead`).
    if (!replayed) exitFlowOrWarn(backend, "failed");
    process.exit(1);
    return;
  }
  restore();
  exitFlowOrWarn(backend, "completed");
  process.exit(0);
}
