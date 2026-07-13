/**
 * `ioredis` library adapter — resilience for Redis commands with zero code
 * changes (architecture-spec.md §5.2: "Adapters: ... pg, ioredis, mysql2").
 *
 * Seam: `Redis.prototype.sendCommand(command, stream)` — Commander's single
 * dispatch chokepoint: every generated command method (`redis.get(...)`,
 * `redis.set(...)`, ...) builds a `Command` and calls `this.sendCommand(cmd)`,
 * returning whatever it returns (ioredis's `AGENTS.md`: "Command execution ...
 * is routed through ... direct sendCommand"). Patched on the prototype, so it
 * covers every command method at once and is reversible (uninstall restores
 * the original, DX invariant 2).
 *
 * Only "plain" commands are intercepted — an object exposing the documented
 * `Command` shape (`.name` string, `.args` array, `.promise` a `Promise`,
 * `.constructor` usable to build a same-shaped clone). Anything else (a future
 * ioredis internal type this pack doesn't recognize) is forwarded untouched
 * (Level 0: do nothing if it can't be wrapped safely).
 *
 * Idempotency is judged from an EXPLICIT read-only command table
 * (`IDEMPOTENT_REDIS_COMMANDS`, below), not inferred: `GET`/`MGET`/`EXISTS`/
 * `TTL`/... are retryable; every mutating command (`SET`, `DEL`, `INCR`,
 * `EXPIRE`, `EVAL`, ...), pub/sub, transactions (`MULTI`/`EXEC`/`WATCH`), and
 * any command absent from the table are observed, not retried (KEEL-E014).
 * Calls are never cached (`args_hash` null) — a reply is not safely
 * replayable, and several read commands (`RANDOMKEY`, `SCAN`'s cursor) are not
 * even deterministic.
 *
 * Retry mechanics: a `Command`'s `.promise` settles exactly once, so a failed
 * FIRST attempt (the real, caller-supplied `Command`) cannot be "re-armed" —
 * each retried attempt dispatches a FRESH clone (`new command.constructor(
 * command.name, command.args)`, no callback) over the same connection, and
 * this pack's own promise (not `command.promise`) settles with whichever
 * attempt ultimately wins. This means a CALLBACK passed to the original call
 * fires once, with the FIRST attempt's outcome (ioredis wires a callback to
 * `command.promise` directly); the value returned to `await redis.get(...)` /
 * `.then()` callers — the dominant modern usage — always reflects the final,
 * retried outcome. Documented limitation, not a defect: retrying a call whose
 * completion is a one-shot callback fundamentally cannot make that callback
 * itself retry-aware without a second, ioredis-internal hook this pack does
 * not have.
 *
 * Timeout: like the pg pack, `sendCommand` has no cancellation hook, so a
 * per-attempt deadline (idempotent commands only) is a SOFT race — Keel stops
 * waiting, but an abandoned command keeps occupying the connection until its
 * own reply arrives (silently discarded). ioredis's own `commandTimeout`
 * connection option remains the right tool for a true per-command cutoff.
 */

import { createRequire } from "node:module";
import { pathToFileURL } from "node:url";
import { getBackend, getDiscovery, attachOutcome } from "../runtime.mjs";
import { KeelError } from "../engine.mjs";
import { resolveFrom, durationMs, withSoftTimeout, CONN_ERROR_CODES } from "./_shared.mjs";

const PKG_SPECIFIER = "ioredis/package.json";
const MODULE_SPECIFIER = "ioredis";
// ioredis 5.x is the certified major (adapter-pack contract: "pinned" =
// covered by contract tests via the structural fixture in
// fixtures/ioredis-client.d.ts; outside the range the pack still tries).
const PINNED_MAJOR = "5";

/**
 * Redis commands (lowercase, matching `Command.name`) that are safe to retry:
 * pure reads with no observable side effect on server state. Deliberately
 * conservative and explicit — anything not listed here (including every
 * mutating command, admin/connection commands, pub/sub, scripting, and
 * transactions) is treated as non-idempotent.
 */
export const IDEMPOTENT_REDIS_COMMANDS = new Set([
  // generic
  "get",
  "mget",
  "exists",
  "type",
  "ttl",
  "pttl",
  "dump",
  "randomkey",
  "keys",
  "scan",
  "touch",
  // strings
  "strlen",
  "getrange",
  "substr",
  "getbit",
  "bitcount",
  "bitpos",
  // hashes
  "hget",
  "hmget",
  "hgetall",
  "hkeys",
  "hvals",
  "hlen",
  "hstrlen",
  "hexists",
  "hscan",
  "hrandfield",
  // lists
  "llen",
  "lrange",
  "lindex",
  "lpos",
  // sets
  "smembers",
  "sismember",
  "smismember",
  "scard",
  "srandmember",
  "sscan",
  "sdiff",
  "sinter",
  "sunion",
  // sorted sets
  "zscore",
  "zmscore",
  "zrange",
  "zrevrange",
  "zrangebyscore",
  "zrevrangebyscore",
  "zrangebylex",
  "zrevrangebylex",
  "zrank",
  "zrevrank",
  "zcard",
  "zcount",
  "zlexcount",
  "zscan",
  "zrandmember",
  "zdiff",
  "zinter",
  "zunion",
  // streams
  "xlen",
  "xrange",
  "xrevrange",
  "xread",
  // hyperloglog
  "pfcount",
  // geo
  "geopos",
  "geodist",
  "geohash",
  "geosearch",
  // connection / server (read-only)
  "ping",
  "echo",
  "dbsize",
  "time",
  "lastsave",
  "memory",
  "object",
]);

export function isIdempotentRedisCommand(name) {
  return typeof name === "string" && IDEMPOTENT_REDIS_COMMANDS.has(name.toLowerCase());
}

function isPinned(version) {
  return typeof version === "string" && version.split(".")[0] === PINNED_MAJOR;
}

/** The connection host/path a `Redis` instance targets, or "unknown". */
function hostFromRedis(client) {
  try {
    const host = client?.options?.host ?? client?.options?.path;
    return typeof host === "string" && host ? host : "unknown";
  } catch {
    return "unknown";
  }
}

/** The documented `Command` public shape: `.name`, `.args`, a `.promise`, and
 *  a usable `.constructor` — anything else is forwarded untouched. */
function isPlainCommand(command) {
  return (
    command !== null &&
    typeof command === "object" &&
    typeof command.name === "string" &&
    Array.isArray(command.args) &&
    command.promise instanceof Promise &&
    typeof command.constructor === "function"
  );
}

/** A fresh, promise-only `Command` with the same name/args as `command` — used
 *  for retried attempts (the original's `.promise` already settled once and
 *  cannot be re-armed). No callback is attached to the clone (see module
 *  docstring: only the first attempt's callback, if any, ever fires). */
function cloneCommand(command) {
  return new command.constructor(command.name, command.args);
}

/** Classify a thrown ioredis/transport error into a core error class. */
export function classifyRedisError(err) {
  if (err?.name === "KeelTimeoutError") return "timeout"; // Keel's own soft deadline
  const code = err?.code;
  if (typeof code === "string" && CONN_ERROR_CODES.test(code)) return "conn";
  if (err?.name === "MaxRetriesPerRequestError") return "conn"; // ioredis gave up reconnecting
  if (/\b(connection is closed|isn't writeable|connection ended)\b/i.test(err?.message ?? "")) return "conn";
  return "other";
}

/**
 * Wrap a `Redis.prototype.sendCommand` so each plain command routes through
 * the backend. `deps.backend`/`deps.discovery` override the globals (for
 * tests/embedding).
 */
export function makeWrappedSendCommand(original, deps = {}) {
  return function keelSendCommand(command, stream) {
    const backend = deps.backend ?? getBackend();
    if (!backend || !isPlainCommand(command)) {
      return original.call(this, command, stream); // disabled, or a shape we never wrap
    }
    const name = command.name.toLowerCase();
    const target = hostFromRedis(this);
    const idempotent = isIdempotentRedisCommand(name);
    const op = `redis ${name} ${target}`;
    const req = { v: 1, target, op, idempotent, args_hash: null };
    const timeoutMs = idempotent ? durationMs(backend.layer(target, "timeout")) : null;

    let attempts = 0;
    let liveResult;
    let haveResult = false;
    let liveErr;
    const started = performance.now();
    return backend
      .execute(req, async () => {
        attempts++;
        const cmd = attempts === 1 ? command : cloneCommand(command);
        original.call(this, cmd, stream);
        try {
          liveResult = timeoutMs ? await withSoftTimeout(cmd.promise, timeoutMs, "ioredis") : await cmd.promise;
          haveResult = true;
          return { status: "ok", payload: null };
        } catch (err) {
          liveErr = err;
          return { status: "error", class: classifyRedisError(err), message: err?.message ?? String(err) };
        }
      })
      .then((outcome) => {
        (deps.discovery ?? getDiscovery())?.observe(target, outcome, performance.now() - started);
        if (outcome.result === "ok") return attachOutcome(haveResult ? liveResult : outcome.payload, outcome);
        if (liveErr instanceof Error) throw attachOutcome(liveErr, outcome);
        const e = new KeelError(outcome.error?.code ?? "KEEL-E040", outcome.error?.message ?? "keel ioredis failure");
        throw attachOutcome(e, outcome);
      });
  };
}

/**
 * Patch `RedisClass.prototype.sendCommand` in place. Idempotent (a second
 * patch is a no-op) and reversible (returns an uninstall that restores the
 * original).
 */
export function patchSendCommand(RedisClass, deps = {}) {
  const proto = RedisClass?.prototype;
  if (!proto || typeof proto.sendCommand !== "function" || proto.sendCommand.__keelWrapped) return () => {};
  const original = proto.sendCommand;
  const wrapped = makeWrappedSendCommand(original, deps);
  wrapped.__keelWrapped = true;
  wrapped.__keelOriginal = original;
  proto.sendCommand = wrapped;
  return function uninstall() {
    if (proto.sendCommand === wrapped) proto.sendCommand = original;
  };
}

/** The `ioredis` adapter pack — the four uniform operations (adapter-pack.md). */
export function ioredisPack({ cwd = process.cwd() } = {}) {
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
      return {
        matched: true,
        name: "ioredis",
        version,
        confidence: isPinned(version) ? "pinned" : "best_effort",
      };
    },
    seams() {
      return [
        {
          patchPoint: "Redis.prototype.sendCommand",
          upstreamApi: "ioredis — Redis.prototype.sendCommand(command, stream)",
          whyStable:
            "the single dispatch chokepoint every generated command method (get, set, ...) routes through and returns the result of",
        },
      ];
    },
    targets() {
      return [
        {
          pattern: "<db host>",
          kind: "host",
          idempotencyRule:
            "keyed off an explicit read-only command table (GET/MGET/EXISTS/TTL/...); every mutating command, pub/sub, transaction, and any command absent from the table is observed-not-retried (KEEL-E014)",
          argsHashRule: "none (commands are not cached — a reply is not safely replayable)",
        },
      ];
    },
    // host targets inherit [defaults.outbound]; no ioredis-specific fragment.
    defaults() {
      return {};
    },
  };
}

/**
 * Auto-detect `ioredis` and patch it (best-effort; never throws). Called by
 * the bootstrap. `redisModule` may be injected (tests); otherwise the module
 * is dynamically imported only when resolvable — absent `ioredis` is a silent
 * no-op.
 */
export async function installIoredisPack({ cwd = process.cwd(), redisModule } = {}) {
  try {
    const mod = redisModule ?? (await loadRedisModule(cwd));
    const RedisClass = mod?.default ?? mod?.Redis;
    if (typeof RedisClass !== "function") return { active: false };
    const uninstall = patchSendCommand(RedisClass);
    return { active: true, name: "ioredis", uninstall };
  } catch {
    return { active: false }; // detection/patch is best-effort, never fatal
  }
}

async function loadRedisModule(cwd) {
  const resolved = resolveFrom(cwd, MODULE_SPECIFIER);
  if (!resolved) return null;
  return import(pathToFileURL(resolved).href);
}
