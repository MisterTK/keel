/**
 * `node:child_process` interceptor — runtime-native `cmd:` durable-flow dispatch
 * for `spawnSync` / `execFileSync` with zero code changes (issue #27, the Node
 * half of chunk-8; the Python twin is `keel/adapters/subprocess_pack.py`).
 *
 * When a keel.toml declares a `cmd:` flow entrypoint AND a matching
 * `[flows.match."cmd:<name>"]` argv rule (CCR-5), a synchronous subprocess call
 * whose observed argv matches that rule is dispatched as a Tier-2 durable flow
 * (at-most-once dispatch per identity) instead of running unwrapped — the same
 * guarantee `keel exec` gives an external command, but from inside a live Node
 * process with no CLI wrapper. `execSync` (a single *shell command string*, not
 * an argv) is deliberately out of scope and never patched — the same reasoning
 * as the Python side's `shell=True` exclusion; a `{ shell: true }` option on
 * `spawnSync`/`execFileSync` is skipped for the same reason (the shell, not the
 * argv, decides what runs).
 *
 * # Interception mechanism (why `createRequire`, not `import`)
 *
 * The seam is the property `spawnSync`/`execFileSync` on the `node:child_process`
 * module object. We acquire that object via
 * `createRequire(import.meta.url)("node:child_process")` and mutate it in place
 * (idempotent `__keelWrapped` guard + an `uninstall` closure), exactly like
 * `packs/pg.mjs`'s `patchClientQuery` mutates `Client.prototype.query`. The
 * ACQUISITION differs from every other pack (which dynamically `import()` a
 * third-party npm lib): a `node:` builtin's CJS module object is the one an
 * `import cp from "node:child_process"` default binding and a
 * `const cp = require("node:child_process")` both read live, so mutating it is
 * observed by both. An ESM NAMED binding (`import { spawnSync } from …`) is a
 * different story: on current Node it is a SNAPSHOT taken when that consumer
 * module is first evaluated, so it observes the patch only if the patch was
 * applied BEFORE the consumer loaded. That ordering is guaranteed in the real
 * product: `--import keelrun/hook` top-level-awaits `installKeel()` (this pack's
 * `install`) before Node evaluates the app's module graph (see `hook.mjs`), so
 * all three consumer shapes — require, named-import, default-import — observe
 * the patch. A consumer that snapshots the reference itself (`const f =
 * spawnSync`) at eval time genuinely cannot be reached; that is an inherent,
 * documented limit of any monkey-patch seam (the test suite pins all four
 * shapes so this constraint can never silently regress — design §3.2).
 *
 * # Why this is at-most-once DISPATCH, not result-replay (the v1 FFI limit)
 *
 * Open Question 1, resolved empirically (see the chunk report / probe scripts):
 * an already-`Completed` flow's `execute()`/`executeAsync()` DOES substitute the
 * recorded step outcome without re-firing the effect — but only via the ASYNC
 * `executeAsync` path. The native core REFUSES the synchronous `execute()` while
 * a flow is open (KEEL-E005 — "Node effects are async-only", see
 * `crates/keel-node/src/lib.rs`). `spawnSync`/`execFileSync` are synchronous:
 * their return value must be produced on the same tick, so they cannot await
 * `executeAsync` and therefore cannot record OR substitute a journaled step.
 * Recording the command result for later replay-substitution needs a
 * synchronous-execute-in-flow FFI that is not exposed to Node in v1 (deferred).
 *
 * So this pack drives the flow bracket with the two SYNCHRONOUS operations the
 * core does expose inside a flow — `enterFlow` / `exitFlow` — and records no
 * command step. That still delivers the load-bearing guarantee: a `Completed`
 * flow is fenced from re-dispatch, a live holder is fenced or waited per
 * `[flows] on_busy`, and a dead/exhausted flow refuses (KEEL-E032). What it
 * cannot do is hand back the RECORDED result of a completed prior run — so on
 * `enterFlow`'s `replay === true` (the flow already completed) it throws
 * {@link KeelCmdFlowReplayUnsupportedError} rather than re-run the command
 * (violating at-most-once) or fabricate a result. Loud, documented, never
 * silent. The CLI-level `keel exec` / `keel flows` DO replay the recorded
 * outcome (the Rust path drives the journal directly); that is the workaround.
 *
 * # Identity (diverges from `keel exec` — TK sign-off, CCR-5)
 *
 * `args_hash = sha256(argv.join("\0") + "\0" + process.cwd())[..16]` — the
 * in-process hash INCLUDES the caller's cwd (so the same command line launched
 * from two directories is two flows), a deliberate divergence from `exec.rs`'s
 * argv-only `args_hash`. `code_hash = sha256(resolvedProgram + "\0" +
 * argv.join("\0"))[..16]` fences replay across a changed program binary, same
 * shape and 16-hex width as `exec.rs`. `explicit_key` is unset in v1. The
 * child's own `opts.cwd` (where the subprocess runs) is intentionally NOT part
 * of identity in v1 — only the caller's `process.cwd()` is, matching the
 * signed-off formula.
 */

import { createRequire } from "node:module";
import { createHash } from "node:crypto";
import { existsSync, statSync } from "node:fs";
import { join } from "node:path";
import { getBackend, flowScopeActive } from "../runtime.mjs";

// --- error types (propagate to the caller's spawnSync/execFileSync) ---------

/**
 * A matched `cmd:` flow already completed in a prior run, so re-dispatching it
 * would violate at-most-once — but in-process replay-skip of the RECORDED
 * result needs native FFI not exposed to Node in v1 (see the module docs). We
 * refuse loudly instead of re-running or fabricating. `keel exec`/`keel flows`
 * replay the recorded outcome at the CLI level.
 */
export class KeelCmdFlowReplayUnsupportedError extends Error {
  constructor(entrypoint, flowId) {
    super(
      `KEEL-E005: ${entrypoint} [${flowId}] already completed in a prior run; in-process ` +
        `replay-skip of a cmd: flow's recorded result needs native FFI not yet exposed to ` +
        `Node (a documented v1 limit). The command was NOT re-run (at-most-once dispatch). ` +
        `Use \`keel exec --flow ${entrypoint.slice(4)} -- <argv>\` (or \`keel flows\`) for ` +
        `CLI-level durable replay, or remove [flows.match."${entrypoint}"] to run it unwrapped.`
    );
    this.name = "KeelCmdFlowReplayUnsupportedError";
    this.code = "KEEL-E005";
    this.entrypoint = entrypoint;
    this.flowId = flowId;
  }
}

/** A live process holds the flow's lease and `[flows] on_busy = fail` (or a
 *  bounded `wait` elapsed). Maps to the core's KEEL-E030. */
export class KeelCmdFlowBusyError extends Error {
  constructor(entrypoint, argsHash, detail) {
    super(`KEEL-E030: ${entrypoint} (args_hash ${argsHash}) is busy — ${detail}. See \`keel explain KEEL-E030\`.`);
    this.name = "KeelCmdFlowBusyError";
    this.code = "KEEL-E030";
    this.entrypoint = entrypoint;
  }
}

/** The flow is dead / has exhausted its attempt cap (core KEEL-E032). Always a
 *  hard failure regardless of `on_busy` — never silently skipped. */
export class KeelCmdFlowDeadError extends Error {
  constructor(entrypoint, message) {
    super(`KEEL-E032: ${entrypoint} is dead: ${message} See \`keel explain KEEL-E032\`.`);
    this.name = "KeelCmdFlowDeadError";
    this.code = "KEEL-E032";
    this.entrypoint = entrypoint;
  }
}

// --- identity ----------------------------------------------------------------

/** The identity-digest width (hex chars) — matches `exec.rs`'s `sha16`. */
const HASH_WIDTH = 16;

function sha16(s) {
  return createHash("sha256").update(s).digest("hex").slice(0, HASH_WIDTH);
}

/** `args_hash`, cwd-inclusive (see module docs). `argv` is `[program, …args]`. */
export function argsHashWithCwd(argv, cwd) {
  return sha16(argv.join("\0") + "\0" + cwd);
}

/** argv[0] resolved through a PATH lookup (`which`-style), falling back to the
 *  verbatim string — mirrors `exec.rs::resolve_program`. A path-bearing argv[0]
 *  is returned unchanged; a bare name is searched across `PATH` entries. */
export function resolveProgram(argv0, env = process.env) {
  if (argv0.includes("/") || (process.platform === "win32" && argv0.includes("\\"))) return argv0;
  const raw = env.PATH ?? env.Path; // win32 casing tolerance
  if (!raw) return argv0;
  const sep = process.platform === "win32" ? ";" : ":";
  for (const dir of raw.split(sep)) {
    if (!dir) continue;
    const candidate = join(dir, argv0);
    try {
      if (existsSync(candidate) && statSync(candidate).isFile()) return candidate;
    } catch {
      /* unreadable PATH entry — skip */
    }
  }
  return argv0;
}

/** `code_hash`: fences replay across a changed program binary (module docs). */
export function codeHash(argv, env = process.env) {
  return sha16(resolveProgram(argv[0], env) + "\0" + argv.join("\0"));
}

// --- argv matching (single-`*` dialect, per-position, case-sensitive) --------

/** One argv pattern item → an anchored, case-SENSITIVE RegExp. `*` matches any
 *  run of characters WITHIN that argv position (it does not cross positions —
 *  each pattern item matches exactly one argv element). Case-sensitive because
 *  argv values are (unlike hostnames, which `judge.mjs` lower-cases). */
function itemRegExp(item) {
  const escaped = item
    .split("*")
    .map((s) => s.replace(/[.*+?^${}()|[\]\\]/g, "\\$&"))
    .join(".*");
  return new RegExp("^" + escaped + "$");
}

/**
 * Compile the `cmd:` flows into a match table sorted most-specific-first, ready
 * to test an observed argv against. Input is `extractCmdFlows(policy)`'s output
 * (`{ [entrypoint]: { name, argvPatterns, onBusy } }`). Rule-less entrypoints
 * (empty `argvPatterns`) are dropped — they can never match in-process.
 *
 * Specificity / tie-break mirrors `judge.mjs`'s outbound-target ordering:
 * `(wildcard_count ascending, -literal_length, key lexicographic)` — a total
 * order, so two runs (and the Python twin) always pick the same rule.
 */
export function compileCmdMatchers(cmdFlows) {
  const rules = [];
  for (const key of Object.keys(cmdFlows ?? {})) {
    const entry = cmdFlows[key];
    const patterns = entry?.argvPatterns;
    if (!Array.isArray(patterns) || patterns.length === 0) continue;
    const wildcards = patterns.reduce((n, p) => n + (p.split("*").length - 1), 0);
    const literal = patterns.reduce((n, p) => n + p.replace(/\*/g, "").length, 0);
    rules.push({
      name: entry.name,
      onBusy: entry.onBusy ?? "skip",
      regexes: patterns.map(itemRegExp),
      arity: patterns.length,
      wildcards,
      literal,
    });
  }
  rules.sort(
    (a, b) =>
      a.wildcards - b.wildcards ||
      b.literal - a.literal ||
      (a.name < b.name ? -1 : a.name > b.name ? 1 : 0)
  );
  return rules;
}

/** The most-specific compiled rule whose per-position patterns all match
 *  `argv` (exact arity), or `null`. A single `*` item matches one position, so
 *  arity must be exact — v1 has no variadic/`**` item (the frozen dialect). */
export function matchArgv(compiled, argv) {
  for (const rule of compiled) {
    if (rule.arity !== argv.length) continue;
    let ok = true;
    for (let i = 0; i < rule.arity; i++) {
      if (!rule.regexes[i].test(argv[i])) {
        ok = false;
        break;
      }
    }
    if (ok) return rule;
  }
  return null;
}

// --- argument-shape helpers (spawnSync/execFileSync share a signature) -------

/** The observed argv `[program, …args]` from a `(cmd[, args][, opts])` call.
 *  Extra args are only present when arg 1 is a real array; otherwise it is the
 *  options object (or absent) and the argv is just `[cmd]`. Elements are
 *  coerced to strings for matching/hashing only — the real call is untouched. */
function argvOf(args) {
  const rest = Array.isArray(args[1]) ? args[1] : [];
  return [String(args[0]), ...rest.map(String)];
}

/** The options object of a `(cmd[, args][, opts])` call, or `null`. */
function optsOf(args) {
  if (Array.isArray(args[1])) return args[2] && typeof args[2] === "object" ? args[2] : null;
  return args[1] && typeof args[1] === "object" ? args[1] : null;
}

// --- synchronous, bounded wait (for on_busy = wait) --------------------------

/** Poll cadence for `on_busy = wait`, matching `exec.rs`'s 500ms. */
const WAIT_POLL_MS = 500;
/**
 * Max total time an `on_busy = wait` blocks before giving up with a
 * timeout-specific {@link KeelCmdFlowBusyError}. A DELIBERATE divergence from
 * `exec.rs`'s UNBOUNDED wait: `exec.rs` is a one-shot CLI a human watches and
 * can ^C; this runs inside a live (possibly production) Node process where an
 * unbounded synchronous block would wedge the event loop indefinitely. Blocking
 * synchronously at all is fine here — `spawnSync`/`execFileSync` already block
 * the loop by nature — but it must be bounded.
 */
const WAIT_MAX_MS = 30_000;

/** A genuinely synchronous, event-loop-blocking sleep — `Atomics.wait` on a
 *  throwaway shared buffer that never changes, so it always waits the full
 *  `ms`. Consistent with the blocking nature of the primitives we wrap. */
function syncSleep(ms) {
  Atomics.wait(new Int32Array(new SharedArrayBuffer(4)), 0, 0, ms);
}

function isTruthy(v) {
  return ["1", "true", "yes"].includes(String(v ?? "").trim().toLowerCase());
}

function debug(env, msg) {
  if (isTruthy(env?.KEEL_DEBUG)) process.stderr.write(`keel ▸ ${msg}\n`);
}

// --- the flow bracket --------------------------------------------------------

/** Stamp a flow's terminal status, degrading a journal-WRITE failure (issue
 *  #14: `exitFlow` can throw KEEL-E040) to a stderr line — never letting it
 *  replace the command's own result/exception. Mirrors `flow.mjs`'s
 *  `exitFlowOrWarn`. */
function exitFlowOrWarn(backend, status) {
  try {
    backend.exitFlow(status);
  } catch (err) {
    const code = err?.code ?? "KEEL-E040";
    process.stderr.write(
      `keel ▸ ${code}: cmd: flow terminal status not journaled: ${err?.message ?? err}\n`
    );
  }
}

/**
 * Open the durable flow for one matched call, handling on_busy / dead / replay.
 * Returns `{ passthrough: true }` when the caller should run the command
 * UNWRAPPED (on_busy = skip), or `{ open: true }` when a live flow was entered
 * and the caller must run the command then call {@link endFlow}. Throws
 * {@link KeelCmdFlowReplayUnsupportedError} / {@link KeelCmdFlowBusyError} /
 * {@link KeelCmdFlowDeadError} for the refusal cases (see module docs).
 */
function beginFlow(backend, rule, argv, cwd, env) {
  const entrypoint = rule.name;
  const ah = argsHashWithCwd(argv, cwd);
  const ch = codeHash(argv, env);
  const leaseMs = env?.KEEL_FLOW_LEASE_MS ? Number(env.KEEL_FLOW_LEASE_MS) : undefined;
  let waited = 0;
  for (;;) {
    let info;
    try {
      info = backend.enterFlow(entrypoint, ah, { codeHash: ch, leaseMs });
    } catch (err) {
      const code = err?.code;
      if (code === "KEEL-E030") {
        // A live holder leases the flow. A dead cross-process holder is
        // reclaimed only once its lease TTL expires (no PID-liveness probe —
        // a v1 limit vs exec.rs's instant kill(pid,0); TTL is uniform for
        // same-process and cross-process holders alike).
        if (rule.onBusy === "fail") throw new KeelCmdFlowBusyError(entrypoint, ah, "on_busy = fail");
        if (rule.onBusy === "skip") {
          // Reinterpret "skip" for an in-process caller that expects a real
          // result: skip FLOW-TRACKING, run the command unwrapped anyway
          // (exec.rs's literal "exit 0 without running" only makes sense for a
          // one-shot CLI). NOT a fabricated success.
          debug(env, `cmd: ${entrypoint} busy; on_busy=skip → running unwrapped (not flow-tracked).`);
          return { passthrough: true };
        }
        // on_busy = wait: bounded synchronous retry (see WAIT_MAX_MS).
        if (waited >= WAIT_MAX_MS) {
          throw new KeelCmdFlowBusyError(entrypoint, ah, `on_busy = wait timed out after ${WAIT_MAX_MS}ms`);
        }
        syncSleep(WAIT_POLL_MS);
        waited += WAIT_POLL_MS;
        continue;
      }
      if (code === "KEEL-E032") throw new KeelCmdFlowDeadError(entrypoint, err?.message ?? String(err));
      // KEEL-E040 (no journal) or any other enterFlow failure: surface it
      // honestly rather than silently running unwrapped.
      throw err;
    }
    if (info.replay) {
      // Already completed: release the handle we just entered, then refuse
      // (branch B — see module docs). Never re-run, never fabricate.
      exitFlowOrWarn(backend, "completed");
      throw new KeelCmdFlowReplayUnsupportedError(entrypoint, info.flow_id);
    }
    return { open: true };
  }
}

/** Close a live flow entered by {@link beginFlow}. `ok` = the command
 *  succeeded (exit 0, no signal, no spawn error). */
function endFlow(backend, ok) {
  exitFlowOrWarn(backend, ok ? "completed" : "failed");
}

/**
 * Decide whether a call is eligible for dispatch, returning `{ rule, argv, cwd }`
 * or `null` (run unwrapped). `null` covers: keel disabled, a non-string
 * program, `{ shell: true }` (the shell decides what runs — out of scope),
 * being nested inside an already-open flow (running our own would clobber the
 * outer flow's single native slot — pass through, matching the pre-pack
 * behavior of a sync subprocess inside a `keel run` ts: flow), and no matching
 * `[flows.match]` rule.
 */
function precheck(backend, compiled, args) {
  if (!backend) return null;
  if (typeof args[0] !== "string") return null;
  const opts = optsOf(args);
  if (opts && opts.shell) return null;
  if (flowScopeActive()) return null;
  const argv = argvOf(args);
  const rule = matchArgv(compiled, argv);
  if (!rule) return null;
  return { rule, argv, cwd: process.cwd() };
}

// --- the wrappers ------------------------------------------------------------

/**
 * Wrap `spawnSync`. `spawnSync` NEVER throws for a normal outcome — it returns
 * `{ status, signal, error, stdout, stderr, … }` (a nonzero exit is `status !==
 * 0`; a spawn failure like ENOENT is in `error`). So we run it, read the
 * outcome object, stamp the flow terminal status, and return the object
 * unchanged (the caller still sees `error`/`status`/`signal` exactly as
 * before). `deps.backend` overrides the global (tests/embedding).
 */
export function makeWrappedSpawnSync(original, deps = {}) {
  const { compiled = [], env = process.env } = deps;
  return function keelSpawnSync(...args) {
    const backend = deps.backend ?? getBackend();
    const decision = precheck(backend, compiled, args);
    if (!decision) return original.apply(this, args);
    const flow = beginFlow(backend, decision.rule, decision.argv, decision.cwd, env);
    if (flow.passthrough) return original.apply(this, args);
    const result = original.apply(this, args);
    const ok = !result?.error && result?.status === 0 && result?.signal == null;
    endFlow(backend, ok);
    return result;
  };
}

/**
 * Wrap `execFileSync`. UNLIKE `spawnSync`, `execFileSync` THROWS on a nonzero
 * exit (an Error carrying `.status`/`.signal`/`.stdout`/`.stderr`) AND on a
 * spawn failure (an Error carrying `.code = 'ENOENT'`, `.status = null`); on
 * success it RETURNS stdout. So we record success/failure by catching: a throw
 * → `exitFlow("failed")` then re-throw the ORIGINAL error unchanged (never
 * swallowed — the caller sees the exact same exception, whether a nonzero exit
 * or a launch failure); a return → `exitFlow("completed")` then return stdout.
 */
export function makeWrappedExecFileSync(original, deps = {}) {
  const { compiled = [], env = process.env } = deps;
  return function keelExecFileSync(...args) {
    const backend = deps.backend ?? getBackend();
    const decision = precheck(backend, compiled, args);
    if (!decision) return original.apply(this, args);
    const flow = beginFlow(backend, decision.rule, decision.argv, decision.cwd, env);
    if (flow.passthrough) return original.apply(this, args);
    let out;
    try {
      out = original.apply(this, args);
    } catch (err) {
      endFlow(backend, false);
      throw err;
    }
    endFlow(backend, true);
    return out;
  };
}

// --- patch application (idempotent + reversible) -----------------------------

/** Swap one method on `cp` for its wrapped form (idempotent; a second patch is
 *  a no-op) and return an uninstall that restores the original. */
function patchOne(cp, key, makeWrapped, deps) {
  const original = cp?.[key];
  if (typeof original !== "function" || original.__keelWrapped) return () => {};
  const wrapped = makeWrapped(original, deps);
  wrapped.__keelWrapped = true;
  wrapped.__keelOriginal = original;
  cp[key] = wrapped;
  return function uninstall() {
    if (cp[key] === wrapped) cp[key] = original;
  };
}

/** Patch `spawnSync` + `execFileSync` on the `node:child_process` object `cp`
 *  in place. Idempotent + reversible. `execSync` is never touched. */
export function patchChildProcess(cp, deps = {}) {
  const undo = [
    patchOne(cp, "spawnSync", makeWrappedSpawnSync, deps),
    patchOne(cp, "execFileSync", makeWrappedExecFileSync, deps),
  ];
  return function uninstall() {
    for (const u of undo) u();
  };
}

/** True iff `backend` can drive a Tier-2 flow (native surface + attached
 *  journal). Inlined rather than importing `flow.mjs` to avoid coupling. */
function supportsFlows(backend) {
  return (
    typeof backend?.enterFlow === "function" &&
    typeof backend?.exitFlow === "function" &&
    backend?.persistent === true
  );
}

/**
 * Auto-detect the `[flows.match]` config and patch `node:child_process`
 * (best-effort; never throws out of install). Called by the bootstrap with the
 * parsed `cmdFlows` (`extractCmdFlows(policy)`). Returns `{ active, name?,
 * uninstall? }`.
 *
 * No matchable rule → `{ active: false }` and NO patch (NFR2: near-zero cost
 * when no `[flows.match]` rules exist). Rules present but the backend cannot do
 * Tier 2 (no native core / no journal) → one loud stderr notice and no patch:
 * commands run unwrapped rather than crashing every call — but the user is told
 * their declared interception is inactive (not a silent Level-0 downgrade).
 */
export function installChildProcessPack({
  cwd = process.cwd(),
  cmdFlows = {},
  env = process.env,
  backend,
  childProcessModule,
} = {}) {
  try {
    const compiled = compileCmdMatchers(cmdFlows);
    if (compiled.length === 0) return { active: false };
    const be = backend ?? getBackend();
    if (!supportsFlows(be)) {
      if (!isTruthy(env.KEEL_QUIET)) {
        process.stderr.write(
          "keel ▸ KEEL-E005: [flows.match] cmd: interception is configured but Tier 2 needs " +
            "the native core + an attached journal; commands run unwrapped. Build the native addon " +
            "(`cargo build -p keel-node --release`) or set KEEL_BACKEND=native, and ensure a journal.\n"
        );
      }
      return { active: false };
    }
    const cp = childProcessModule ?? createRequire(import.meta.url)("node:child_process");
    const uninstall = patchChildProcess(cp, { compiled, env });
    return { active: true, name: "child_process", uninstall };
  } catch {
    return { active: false }; // detection/patch is best-effort, never fatal to the run
  }
}
