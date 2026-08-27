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
 * # Replay-skip: at-most-once dispatch AND result substitution (issue #42)
 *
 * A matched call runs its real `spawnSync`/`execFileSync` *inside* a journaled
 * effect step — `backend.executeSync(request, effect)` between `enterFlow` and
 * `exitFlow` — which is the exact shape Python's
 * `subprocess_pack.py::_dispatch` uses. Two properties fall out of that one
 * decision, and both are the core's, not this pack's:
 *
 *   1. **Live run** — the effect fires, the command runs for real, and its
 *      outcome is recorded as the flow's step-1 payload.
 *   2. **Re-dispatch of the same identity** — the core substitutes that
 *      recorded step and NEVER fires the effect, so the command does not run a
 *      second time; this pack rebuilds the caller-facing return value from the
 *      recorded payload ({@link rebuildSpawnSync} / {@link rebuildExecFileSync}).
 *
 * "The effect never fired" is how the two cases are told apart (the `live`
 * side-band below — Python's twin does the same), NOT `enterFlow`'s `replay`
 * flag: a flow this pack stamped `failed` (a nonzero exit) re-enters as a
 * *resume*, whose recorded terminal step the core substitutes just the same.
 *
 * Until v0.6 this pack recorded no step at all and threw
 * `KeelCmdFlowReplayUnsupportedError` on a completed re-dispatch, because the
 * native core refused the synchronous `execute()` inside any open flow
 * (KEEL-E005 — "Node effects are async-only"). That guard was relaxed for the
 * flow-handle path in `crates/keel-node/src/lib.rs::execute`: a synchronous
 * effect never yields to the event loop, so it needs no async bridge, and
 * routing it through the open `FlowHandle` (rather than the bare engine) is
 * precisely what the guard existed to require. `spawnSync`/`execFileSync` now
 * have full Python parity.
 *
 * What still refuses loudly is a re-dispatch the core answers with a terminal
 * ERROR — a recorded LAUNCH failure (the command never ran), or no recorded
 * step at all (KEEL-E031, e.g. a journal written before this pack recorded
 * steps). Neither can be turned into a caller-facing result honestly, so
 * {@link KeelCmdFlowFailedError} is raised rather than re-running or
 * fabricating a success — the same stance as Python's `KeelCmdFlowFailed`.
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
import { Buffer } from "node:buffer";
import { join } from "node:path";
import { getBackend, flowScopeActive } from "../runtime.mjs";

// --- error types (propagate to the caller's spawnSync/execFileSync) ---------

/**
 * A re-dispatch the core answered with a terminal ERROR instead of a
 * substitutable result. Two shapes, both refused loudly rather than re-run or
 * fabricated (the exact stance of Python's `subprocess_pack.KeelCmdFlowFailed`):
 *
 *   * the recorded step IS a launch failure — the command never ran (ENOENT, a
 *     `timeout`/`ETIMEDOUT` kill), so there is no result to hand back and
 *     re-attempting it in-process would break at-most-once dispatch;
 *   * `KEEL-E031` — the identity is Completed but carries NO recorded step for
 *     this command (a journal written by a Keel older than #42's replay-skip,
 *     or completed out-of-process by `keel exec`). Nothing to substitute.
 *
 * `KEEL-E005` (unsupported-configuration): a capability the in-process seam
 * does not provide, not a policy or runtime error. It is the same code the
 * removed `KeelCmdFlowReplayUnsupportedError` carried, now scoped to the two
 * cases that genuinely still refuse.
 */
export class KeelCmdFlowFailedError extends Error {
  constructor(entrypoint, outcomeError) {
    const detail = outcomeError?.message ?? "no substitutable result was recorded";
    const cause =
      outcomeError?.code === "KEEL-E031"
        ? "completed with NO recorded step for this command (journaled before in-process " +
          "replay-skip existed, or completed by `keel exec`)"
        : "previously failed to LAUNCH — the command never ran";
    super(
      `KEEL-E005: ${entrypoint} ${cause}, so there is nothing to replay: ${detail}. The command ` +
        `was NOT re-run (at-most-once dispatch). Change the argv/cwd (a new identity), or ` +
        `re-drive it with \`keel exec --flow ${entrypoint.slice(4)} -- <argv>\`.`
    );
    this.name = "KeelCmdFlowFailedError";
    this.code = "KEEL-E005";
    this.entrypoint = entrypoint;
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

/** The most-specific compiled rule whose per-position patterns all match the
 *  LEADING `rule.arity` positions of `argv`, or `null`. A PREFIX match, not
 *  exact arity: the observed argv must have AT LEAST as many elements as the
 *  rule's patterns, and trailing observed args beyond the declared prefix are
 *  unconstrained (CCR-5 / the chunk-8 design doc §2.2, mirroring Python's
 *  `_match` in `subprocess_pack.py` exactly). A single `*` item still matches
 *  exactly one position — this is not a variadic/`**` item, just an
 *  unconstrained tail; issue #55 fixed this function's prior exact-arity
 *  divergence from that spec. */
export function matchArgv(compiled, argv) {
  for (const rule of compiled) {
    if (argv.length < rule.arity) continue;
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

// --- step payload (de)serialization for replay-substitution ------------------

/** A readable `op` for the journal/trace (display only; never part of the step
 *  key). Byte-identical to Python's `subprocess_pack._op_string`. */
function opString(argv) {
  const joined = "cmd " + argv.join(" ");
  return joined.length <= 200 ? joined : joined.slice(0, 197) + "...";
}

/**
 * A JSON-safe envelope for a captured stdout/stderr so a substituted result is
 * byte-identical to the recorded one: a `Buffer`/`Uint8Array` (the default, no
 * `encoding`) round-trips through base64, a string (`{ encoding: "utf8" }`)
 * verbatim, and an absent stream (`stdio: "inherit"`, or a spawn failure) as
 * `null`. The `{t,v}` shape is Python's `_encode_stream`, deliberately — the
 * two packs journal the same envelope.
 */
function encodeStream(value) {
  if (value === null || value === undefined) return null;
  if (typeof value === "string") return { t: "s", v: value };
  if (ArrayBuffer.isView(value)) {
    return {
      t: "b",
      v: Buffer.from(value.buffer, value.byteOffset, value.byteLength).toString("base64"),
    };
  }
  return { t: "s", v: String(value) };
}

function decodeStream(env) {
  if (!env || typeof env !== "object") return null;
  if (env.t === "b") return Buffer.from(String(env.v ?? ""), "base64");
  return env.v ?? null;
}

/** The recorded shape of one `spawnSync` outcome (a run that reached an exit
 *  status or a signal — a spawn failure takes the error branch instead). */
function payloadSpawnSync(result) {
  return {
    kind: "spawnSync",
    status: result?.status ?? null,
    signal: result?.signal ?? null,
    pid: typeof result?.pid === "number" ? result.pid : null,
    stdout: encodeStream(result?.stdout),
    stderr: encodeStream(result?.stderr),
  };
}

/** Rebuild the object `spawnSync` would have returned, key order included
 *  (`status, signal, output, pid, stdout, stderr` — what Node itself emits for
 *  a run that produced an exit status). `output` shares the very same
 *  stdout/stderr values, exactly as the real result does. */
export function rebuildSpawnSync(payload) {
  const stdout = decodeStream(payload?.stdout);
  const stderr = decodeStream(payload?.stderr);
  return {
    status: payload?.status ?? null,
    signal: payload?.signal ?? null,
    output: [null, stdout, stderr],
    pid: payload?.pid ?? 0,
    stdout,
    stderr,
  };
}

/** The recorded shape of an `execFileSync` that RETURNED (stdout, or `null`
 *  under `stdio: "inherit"`). */
function payloadExecFileOk(stdout) {
  return { kind: "execFileSync", threw: false, stdout: encodeStream(stdout) };
}

/** The recorded shape of an `execFileSync` that THREW after the command
 *  actually ran (a nonzero exit / a signal): everything Node hangs off that
 *  error, so the replayed throw carries the same diagnostics. */
function payloadExecFileThrew(err) {
  return {
    kind: "execFileSync",
    threw: true,
    status: err?.status ?? null,
    signal: err?.signal ?? null,
    pid: typeof err?.pid === "number" ? err.pid : null,
    stdout: encodeStream(err?.stdout),
    stderr: encodeStream(err?.stderr),
    message: String(err?.message ?? "Command failed"),
  };
}

/** Rebuild `execFileSync`'s caller-facing outcome: RETURN the recorded stdout,
 *  or THROW a reconstructed error carrying the recorded `status`/`signal`/
 *  `stdout`/`stderr`/`output`/`pid` — `execFileSync`'s nonzero-exit throw is
 *  part of its contract, so replay must reproduce it (the same reason Python's
 *  `_rebuild_run` re-raises a recorded `CalledProcessError`). */
export function rebuildExecFileSync(payload) {
  const stdout = decodeStream(payload?.stdout);
  if (!payload?.threw) return stdout;
  const stderr = decodeStream(payload?.stderr);
  const err = new Error(payload.message ?? "Command failed");
  err.status = payload.status ?? null;
  err.signal = payload.signal ?? null;
  err.pid = payload.pid ?? 0;
  err.output = [null, stdout, stderr];
  err.stdout = stdout;
  err.stderr = stderr;
  throw err;
}

/**
 * True when Node failed to run the child to completion — a spawn failure
 * (`ENOENT`) or a `timeout` kill (`ETIMEDOUT`). Both carry the libuv
 * `errno`/`syscall` pair; a plain nonzero EXIT carries neither (it has a
 * numeric `status`). This is the Node spelling of Python's
 * `OSError`-vs-`CalledProcessError` split in `_run_wrapper`, and it decides
 * whether the journaled step is a terminal ERROR (never ran → a re-dispatch
 * refuses) or a terminal OK (ran → a re-dispatch substitutes).
 */
function isLaunchFailure(err) {
  return err != null && err.errno !== undefined && err.syscall !== undefined;
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
 * Open (or resume/replay) the durable flow for one matched call, handling
 * on_busy and dead. Returns `{ passthrough: true }` when the caller should run
 * the command UNWRAPPED (on_busy = skip), or `{ open: true, replay }` when a
 * handle was acquired and the caller must drive the step then close the flow.
 * Throws {@link KeelCmdFlowBusyError} / {@link KeelCmdFlowDeadError} for the
 * refusal cases (see module docs).
 *
 * `replay` is reported for debugging only — it is NOT the live-vs-substituted
 * signal (see {@link dispatch}): a flow this pack stamped `failed` re-enters
 * with `replay === false` and still has its terminal step substituted.
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
      debug(env, `cmd: ${entrypoint} [${info.flow_id}] already completed; substituting the recorded result.`);
    }
    return { open: true, replay: Boolean(info.replay) };
  }
}

/** Close a live flow entered by {@link beginFlow}. `ok` = the command
 *  succeeded (exit 0, no signal, no spawn error). */
function endFlow(backend, ok) {
  exitFlowOrWarn(backend, ok ? "completed" : "failed");
}

/**
 * Drive one matched call through the enter → execute → exit bracket, the exact
 * shape of Python's `subprocess_pack._dispatch`.
 *
 * `live` is the per-primitive side-band the effect closure fills in
 * (`fired`/`ok`/`result`/`throwErr`), plus `runUnwrapped` (the on_busy = skip
 * escape) and `fromPayload` (rebuild the caller-facing value from a substituted
 * step). `live.fired` — NOT `enterFlow`'s `replay` flag — is what distinguishes
 * a real run from a core-substituted one: the core simply never calls the
 * effect when it has a recorded terminal step for this `(seq, step_key)`.
 */
function dispatch(backend, decision, env, live) {
  const { rule, argv, cwd } = decision;
  const flow = beginFlow(backend, rule, argv, cwd, env);
  if (flow.passthrough) return live.runUnwrapped();
  const request = {
    v: 1,
    target: rule.name,
    op: opString(argv),
    args_hash: argsHashWithCwd(argv, cwd),
    idempotent: false,
  };
  let outcome;
  try {
    outcome = backend.executeSync(request, live.effect);
  } catch (err) {
    // The core refused the step (e.g. a native addon older than the KEEL-E005
    // guard relaxation this parity depends on). The effect never fired, so the
    // command did not run: release the handle and surface it — never silently
    // run unwrapped, which on a completed identity would be the at-most-once
    // violation this whole pack exists to prevent.
    exitFlowOrWarn(backend, "failed");
    throw err;
  }
  if (live.fired) {
    // A real run: the terminal status and the caller-facing value are exactly
    // what they were before the step was journaled (issue #42's no-regression
    // requirement) — `spawnSync` returns its result object even for a nonzero
    // exit or a spawn error, `execFileSync` re-throws the ORIGINAL error.
    endFlow(backend, live.ok);
    if (live.throwErr !== null) throw live.throwErr;
    return live.result;
  }
  // Substituted from the journal — the command did NOT run a second time.
  if (outcome?.result === "error") {
    // Either the recorded step is a terminal error (the command never
    // launched) or there is no recorded step at all (KEEL-E031). Neither can be
    // turned into a caller-facing result honestly — refuse, never re-run.
    exitFlowOrWarn(backend, "failed");
    throw new KeelCmdFlowFailedError(rule.name, outcome?.error);
  }
  // "completed", never a status derived from the recorded exit code: stamping
  // `failed` here would DOWNGRADE an already-Completed flow in the journal.
  // Python's `_dispatch` closes the substituted path the same way.
  exitFlowOrWarn(backend, "completed");
  return live.fromPayload(outcome?.payload ?? {});
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
 * 0`; a spawn failure like ENOENT is in `error`). So the effect runs it, reads
 * the outcome object, and journals it; the wrapper stamps the flow terminal
 * status and returns the object unchanged (the caller still sees
 * `error`/`status`/`signal` exactly as before). A re-dispatch of the same
 * identity returns {@link rebuildSpawnSync}'s reconstruction of the RECORDED
 * result without running anything. `deps.backend` overrides the global
 * (tests/embedding).
 */
export function makeWrappedSpawnSync(original, deps = {}) {
  const { compiled = [], env = process.env } = deps;
  return function keelSpawnSync(...args) {
    const backend = deps.backend ?? getBackend();
    const decision = precheck(backend, compiled, args);
    if (!decision) return original.apply(this, args);
    const self = this;
    const live = {
      fired: false,
      ok: false,
      result: null,
      throwErr: null,
      runUnwrapped: () => original.apply(self, args),
      fromPayload: rebuildSpawnSync,
      effect: () => {
        live.fired = true;
        let result;
        try {
          result = original.apply(self, args);
        } catch (err) {
          // `spawnSync` does not throw for a child outcome, but it does for a
          // bad argument/option. Keep the ORIGINAL exception (never swallowed
          // into the outcome envelope) and journal a terminal error step.
          live.throwErr = err;
          return { status: "error", class: "other", message: String(err?.message ?? err) };
        }
        live.result = result;
        live.ok = !result?.error && result?.status === 0 && result?.signal == null;
        if (result?.error) {
          // The child never ran to completion (ENOENT / a `timeout` kill): a
          // terminal ERROR step, exactly like Python's `OSError` branch.
          return {
            status: "error",
            class: "other",
            message: String(result.error?.message ?? result.error),
          };
        }
        return { status: "ok", payload: payloadSpawnSync(result) };
      },
    };
    return dispatch(backend, decision, env, live);
  };
}

/**
 * Wrap `execFileSync`. UNLIKE `spawnSync`, `execFileSync` THROWS on a nonzero
 * exit (an Error carrying `.status`/`.signal`/`.stdout`/`.stderr`) AND on a
 * spawn failure (an Error carrying `.code = 'ENOENT'`, `.status = null`); on
 * success it RETURNS stdout. The effect records success/failure by catching: a
 * throw → the ORIGINAL error is re-thrown unchanged (never swallowed — the
 * caller sees the exact same exception) after `exitFlow("failed")`; a return →
 * `exitFlow("completed")` then return stdout. The two throw KINDS journal
 * differently, which is what makes replay honest: a nonzero exit RAN, so it is
 * a terminal OK step whose recorded throw is reproduced by
 * {@link rebuildExecFileSync}; a launch failure never ran, so it is a terminal
 * ERROR step and a re-dispatch refuses with {@link KeelCmdFlowFailedError}.
 */
export function makeWrappedExecFileSync(original, deps = {}) {
  const { compiled = [], env = process.env } = deps;
  return function keelExecFileSync(...args) {
    const backend = deps.backend ?? getBackend();
    const decision = precheck(backend, compiled, args);
    if (!decision) return original.apply(this, args);
    const self = this;
    const live = {
      fired: false,
      ok: false,
      result: null,
      throwErr: null,
      runUnwrapped: () => original.apply(self, args),
      fromPayload: rebuildExecFileSync,
      effect: () => {
        live.fired = true;
        let out;
        try {
          out = original.apply(self, args);
        } catch (err) {
          live.throwErr = err;
          if (isLaunchFailure(err)) {
            return { status: "error", class: "other", message: String(err?.message ?? err) };
          }
          return { status: "ok", payload: payloadExecFileThrew(err) };
        }
        live.result = out;
        live.ok = true;
        return { status: "ok", payload: payloadExecFileOk(out) };
      },
    };
    return dispatch(backend, decision, env, live);
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
 *  journal). Inlined rather than importing `flow.mjs` to avoid coupling.
 *  `executeSync` is required too: without it a matched command could be
 *  admission-fenced but never journaled, i.e. no replay-substitution (#42). */
function supportsFlows(backend) {
  return (
    typeof backend?.enterFlow === "function" &&
    typeof backend?.exitFlow === "function" &&
    typeof backend?.executeSync === "function" &&
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
