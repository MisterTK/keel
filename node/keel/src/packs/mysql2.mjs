/**
 * `mysql2` library adapter — resilience for MySQL queries with zero code
 * changes (architecture-spec.md §5.2: "Adapters: ... pg, ioredis, mysql2").
 *
 * Seam: the BASE (callback) `Connection.prototype.query` and
 * `Connection.prototype.execute` — the single dispatch chokepoint for BOTH
 * public APIs at once. `mysql2/promise`'s `PromiseConnection` is a thin
 * wrapper: its `query`/`execute` construct a promise whose executor calls the
 * SAME base callback method internally, so patching the base class covers
 * plain callback-style code AND promise/`async`-`await` code through one seam
 * (mirrors the `mcp:` pack's rationale — the narrowest seam that covers every
 * caller). Patched on the prototype, so it is reversible (uninstall restores
 * the original, DX invariant 2).
 *
 * What is intercepted, and what is forwarded verbatim (Level 0: do nothing if
 * it can't be wrapped safely):
 *   - a call WITH a trailing callback function is intercepted — this is the
 *     shape both callback-style users AND `mysql2/promise` always use (the
 *     promise wrapper never calls the base method without one).
 *   - a call WITHOUT a callback (`connection.query(sql)`, no third/second
 *     arg) returns mysql2's own `Query`/`Execute` command object — an
 *     EventEmitter for STREAMING large result sets row-by-row
 *     (`.on('result', ...)`), not a single request/response. Forwarded
 *     untouched.
 *
 * A documented, narrow deviation from real mysql2 behavior: a wrapped
 * (callback) call always returns `undefined` — never the real `Query`/
 * `Execute` command object mysql2 itself would return, even on a call that is
 * never retried. `mysql2/promise` ignores that return value entirely (it only
 * relies on the callback firing), so this affects only advanced callback-style
 * code that ALSO chains events off the returned emitter — an unusual
 * combination, since streaming (the emitter's actual purpose) requires
 * omitting the callback in the first place, which already forwards untouched
 * above. The callback itself — the sole completion path `mysql2/promise`
 * depends on — always fires exactly once, with the correct final (possibly
 * retried) `(err, results, fields)`.
 *
 * Idempotency is judged from the SQL verb (`sql.mjs`, shared with the pg
 * pack): a bare `SELECT` is retryable; INSERT/UPDATE/DELETE, a `WITH` CTE, DDL,
 * and any unrecognized statement are observed, not retried (KEEL-E014). Calls
 * are never cached (`args_hash` null).
 *
 * Timeout: like the pg/ioredis packs, mysql2's `query`/`execute` have no
 * cooperative cancellation hook exposed through this seam (mysql2's own
 * per-query `timeout` option works by DESTROYING the whole connection, which
 * would break unrelated concurrent work on a shared connection — too
 * destructive to inject automatically), so a per-attempt deadline (idempotent
 * calls only) is a SOFT race: Keel stops waiting, but an abandoned attempt
 * keeps running until its own callback fires (silently discarded).
 */

import { createRequire } from "node:module";
import { pathToFileURL } from "node:url";
import { getBackend, getDiscovery, attachOutcome } from "../runtime.mjs";
import { KeelError } from "../engine.mjs";
import { resolveFrom, durationMs, withSoftTimeout, CONN_ERROR_CODES } from "./_shared.mjs";
import { sqlVerb, isIdempotentSql } from "./sql.mjs";

const PKG_SPECIFIER = "mysql2/package.json";
const MODULE_SPECIFIER = "mysql2";
// mysql2 3.x is the certified major (adapter-pack contract: "pinned" =
// covered by contract tests via the structural fixture in
// fixtures/mysql2-connection.d.ts; outside the range the pack still tries).
const PINNED_MAJOR = "3";

function isPinned(version) {
  return typeof version === "string" && version.split(".")[0] === PINNED_MAJOR;
}

/** The connection host `query`/`execute` is bound to, or "unknown". mysql2
 *  stores the resolved `ConnectionOptions` on `connection.config`. */
function hostFromConnection(conn) {
  try {
    const host = conn?.config?.host;
    return typeof host === "string" && host ? host : "unknown";
  } catch {
    return "unknown";
  }
}

/** True iff the call's last argument is a callback function — the one shape
 *  this pack wraps (see module docstring); a callback-less call returns a
 *  streaming command object and is forwarded untouched. */
function hasCallback(args) {
  return args.length > 0 && typeof args[args.length - 1] === "function";
}

/** The query text from a `query(...)`/`execute(...)` call's (callback-less)
 *  argument list: a bare SQL string, or a `{sql, values, ...}` options
 *  object. `null` when it cannot be determined. */
function queryTextFromArgs(baseArgs) {
  const first = baseArgs[0];
  if (typeof first === "string") return first;
  if (first !== null && typeof first === "object" && typeof first.sql === "string") return first.sql;
  return null;
}

/** Classify a thrown mysql2/transport error into a core error class. */
export function classifyMysql2Error(err) {
  if (err?.name === "KeelTimeoutError") return "timeout"; // Keel's own soft deadline
  const code = err?.code;
  if (typeof code === "string") {
    if (CONN_ERROR_CODES.test(code)) return "conn";
    if (code === "PROTOCOL_CONNECTION_LOST" || code === "PROTOCOL_ENQUEUE_AFTER_QUIT") return "conn";
    if (code === "PROTOCOL_SEQUENCE_TIMEOUT" || code === "ER_LOCK_WAIT_TIMEOUT") return "timeout";
  }
  return "other";
}

/** Dispatch one attempt of a callback-style `query`/`execute` call, returning
 *  a promise of `[results, fields]`. */
function callOnce(original, self, baseArgs) {
  return new Promise((resolve, reject) => {
    original.call(self, ...baseArgs, (err, results, fields) => {
      if (err) reject(err);
      else resolve([results, fields]);
    });
  });
}

/**
 * Wrap a `Connection.prototype.query`/`.execute` so each callback-form call
 * routes through the backend. `deps.backend`/`deps.discovery` override the
 * globals (for tests/embedding).
 */
export function makeWrappedQuery(original, deps = {}) {
  return function keelQuery(...args) {
    const backend = deps.backend ?? getBackend();
    if (!backend || !hasCallback(args)) {
      return original.apply(this, args); // disabled, or a streaming call — pass through untouched
    }
    const userCallback = args[args.length - 1];
    const baseArgs = args.slice(0, -1);
    const text = queryTextFromArgs(baseArgs);
    const target = hostFromConnection(this);
    const verb = sqlVerb(text) ?? "QUERY";
    const idempotent = isIdempotentSql(text);
    const op = `mysql2 ${verb} ${target}`;
    const req = { v: 1, target, op, idempotent, args_hash: null };
    const timeoutMs = idempotent ? durationMs(backend.layer(target, "timeout")) : null;

    let liveResults;
    let liveFields;
    let haveResult = false;
    let liveErr;
    const started = performance.now();
    backend
      .execute(req, async () => {
        try {
          const call = callOnce(original, this, baseArgs);
          const [results, fields] = timeoutMs ? await withSoftTimeout(call, timeoutMs, "mysql2") : await call;
          liveResults = results;
          liveFields = fields;
          haveResult = true;
          return { status: "ok", payload: null };
        } catch (err) {
          liveErr = err;
          return { status: "error", class: classifyMysql2Error(err), message: err?.message ?? String(err) };
        }
      })
      .then((outcome) => {
        (deps.discovery ?? getDiscovery())?.observe(target, outcome, performance.now() - started);
        if (outcome.result === "ok") {
          userCallback(null, attachOutcome(haveResult ? liveResults : outcome.payload, outcome), liveFields);
          return;
        }
        if (liveErr instanceof Error) {
          userCallback(attachOutcome(liveErr, outcome));
          return;
        }
        const e = new KeelError(outcome.error?.code ?? "KEEL-E040", outcome.error?.message ?? "keel mysql2 failure");
        userCallback(attachOutcome(e, outcome));
      });
    // A documented deviation from real mysql2 (module docstring): the real
    // Query/Execute command object is never reconstructed for a wrapped call.
    return undefined;
  };
}

/**
 * Patch `ConnectionClass.prototype.query` and `.execute` in place. Idempotent
 * (a second patch is a no-op) and reversible (returns an uninstall that
 * restores both originals).
 */
export function patchConnectionQuery(ConnectionClass, deps = {}) {
  const proto = ConnectionClass?.prototype;
  if (!proto) return () => {};
  const uninstalls = [];
  for (const method of ["query", "execute"]) {
    if (typeof proto[method] !== "function" || proto[method].__keelWrapped) continue;
    const original = proto[method];
    const wrapped = makeWrappedQuery(original, deps);
    wrapped.__keelWrapped = true;
    wrapped.__keelOriginal = original;
    proto[method] = wrapped;
    uninstalls.push(() => {
      if (proto[method] === wrapped) proto[method] = original;
    });
  }
  return function uninstall() {
    for (const fn of uninstalls) fn();
  };
}

/** The `mysql2` adapter pack — the four uniform operations (adapter-pack.md). */
export function mysql2Pack({ cwd = process.cwd() } = {}) {
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
        name: "mysql2",
        version,
        confidence: isPinned(version) ? "pinned" : "best_effort",
      };
    },
    seams() {
      return [
        {
          patchPoint: "Connection.prototype.query / Connection.prototype.execute",
          upstreamApi:
            "mysql2 — Connection.query(sql[, values], callback) / Connection.execute(sql[, values], callback); mysql2/promise's PromiseConnection calls these internally",
          whyStable:
            "the single dispatch chokepoint shared by the callback API and the promise API (mysql2/promise wraps this same method), so one seam covers both",
        },
      ];
    },
    targets() {
      return [
        {
          pattern: "<db host>",
          kind: "host",
          idempotencyRule:
            "keyed off the SQL verb of the query text: a bare SELECT is retryable; INSERT/UPDATE/DELETE, WITH, DDL, and any unrecognized statement are observed-not-retried (KEEL-E014)",
          argsHashRule: "none (queries are not cached — a result set is not safely replayable)",
        },
      ];
    },
    // host targets inherit [defaults.outbound]; no mysql2-specific fragment.
    defaults() {
      return {};
    },
  };
}

/**
 * Auto-detect `mysql2` and patch it (best-effort; never throws). Called by the
 * bootstrap. `mysqlModule` may be injected (tests); otherwise the module is
 * dynamically imported only when resolvable — absent `mysql2` is a silent
 * no-op. Patches the BASE `Connection` class (`mysql2`, not `mysql2/promise`)
 * — see module docstring for why this single seam covers both APIs.
 */
export async function installMysql2Pack({ cwd = process.cwd(), mysqlModule } = {}) {
  try {
    const mod = mysqlModule ?? (await loadMysql2Module(cwd));
    if (!mod || typeof mod.Connection !== "function") return { active: false };
    const uninstall = patchConnectionQuery(mod.Connection);
    return { active: true, name: "mysql2", uninstall };
  } catch {
    return { active: false }; // detection/patch is best-effort, never fatal
  }
}

async function loadMysql2Module(cwd) {
  const resolved = resolveFrom(cwd, MODULE_SPECIFIER);
  if (!resolved) return null;
  return import(pathToFileURL(resolved).href);
}
