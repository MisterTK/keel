/**
 * `pg` (node-postgres) library adapter — resilience for Postgres queries with
 * zero code changes (architecture-spec.md §5.2: "Adapters: ... pg, ioredis,
 * mysql2").
 *
 * Seam: `Client.prototype.query` — the single query-dispatch chokepoint every
 * `pg` call passes through, including `Pool.query`: a pool checks a real
 * `Client` out and calls this SAME method on it (pg-pool delegates, it does
 * not reimplement dispatch), so patching the `Client` prototype covers both.
 * The patch mutates the prototype method (reversible: uninstall restores the
 * original), so `uninstall = remove the package` holds (DX invariant 2).
 *
 * What is intercepted, and what is forwarded verbatim (Level 0: do nothing if
 * it can't be wrapped safely, rather than risk changing behavior):
 *   - `client.query(text[, values])` / `client.query({text, values, name,
 *     rowMode})` with NO callback resolves to a real `Promise` (pg's
 *     documented "without a callback" form) — this is the shape Keel wraps.
 *   - `client.query(text, callback)` (any arity) returns `undefined` in real
 *     pg and drives completion through the callback, not the return value
 *     (pg's upgrading guide: "returns undefined ... flow control [is] limited
 *     to the callback"). Retrying would mean firing that callback more than
 *     once in a way pg's own contract never promises, so a callback
 *     invocation is forwarded to the original untouched and unobserved.
 *   - a SUBMITTABLE argument (anything with a `.submit` function — a raw
 *     `Query` instance, `pg-cursor`, `pg-query-stream`) implements its own
 *     event-emitter protocol over multiple rows, not a single request/response
 *     Keel can retry; `client.query` just returns the instance. Forwarded
 *     untouched.
 *
 * Idempotency is judged from the SQL verb (`sql.mjs`, shared with the mysql2
 * pack): a bare `SELECT` is retryable; INSERT/UPDATE/DELETE, a `WITH` CTE
 * (which may wrap a data-modifying statement — a write disguised as a read),
 * DDL, and any unrecognized statement are observed, not retried (KEEL-E014).
 * Calls are never cached (`args_hash` null) — a result set is not safely
 * replayable across arbitrary schemas/parameter values.
 *
 * Timeout: like the fetch/mcp seams, a per-attempt deadline is imposed for
 * idempotent (SELECT) queries only, from the target's `timeout`. UNLIKE those
 * seams, `pg`'s public `query()` has no cancellation hook (no `AbortSignal`
 * parameter, no cooperative cancel) — so the deadline is a SOFT race: Keel
 * stops waiting and moves to the next attempt (or gives up), but the in-flight
 * query keeps running against the server until its own result/error arrives
 * (silently discarded, no unhandled-rejection). This is a documented
 * limitation, not a true cancel; `statement_timeout`/`query_timeout` (pg
 * connection config) remain the right tool for server-side cancellation.
 */

import { createRequire } from "node:module";
import { pathToFileURL } from "node:url";
import { getBackend, getDiscovery, attachOutcome } from "../runtime.mjs";
import { KeelError } from "../engine.mjs";
import { resolveFrom, durationMs, withSoftTimeout, CONN_ERROR_CODES } from "./_shared.mjs";
import { sqlVerb, isIdempotentSql } from "./sql.mjs";

const PKG_SPECIFIER = "pg/package.json";
const MODULE_SPECIFIER = "pg";
// pg 8.x is the certified major (adapter-pack contract: "pinned" = covered by
// contract tests via the structural fixture in fixtures/pg-client.d.ts;
// outside the range the pack still tries, reported best_effort).
const PINNED_MAJOR = "8";

function isPinned(version) {
  return typeof version === "string" && version.split(".")[0] === PINNED_MAJOR;
}

/** The connection host `client.query` is bound to, or "unknown". pg's `Client`
 *  stores parsed connection config (including a `connectionString`) on
 *  `connectionParameters.host`; a couple of defensive fallbacks cover
 *  structurally-similar objects (e.g. a pool-checked-out client subclass). */
function hostFromClient(client) {
  try {
    const host = client?.connectionParameters?.host ?? client?.host;
    return typeof host === "string" && host ? host : "unknown";
  } catch {
    return "unknown";
  }
}

/** True iff `arg` is a "submittable" (pg-cursor, pg-query-stream, a raw
 *  `Query`) — anything with its own `.submit` function. `client.query` hands
 *  these back unchanged; they are not a request/response Keel can retry. */
function isSubmittable(arg) {
  return arg !== null && typeof arg === "object" && typeof arg.submit === "function";
}

/** The query text from a `client.query(...)` call's arguments, or `null` when
 *  it cannot be determined (an unrecognized call shape — treated as
 *  non-idempotent by `isIdempotentSql(null)` anyway). */
function queryTextFromArgs(args) {
  const first = args[0];
  if (typeof first === "string") return first;
  if (first !== null && typeof first === "object" && typeof first.text === "string") return first.text;
  return null;
}

/** True iff this `client.query(...)` call passed a callback (any position) —
 *  the one shape this pack never wraps (see module docstring). */
function hasCallback(args) {
  return args.length > 0 && typeof args[args.length - 1] === "function";
}

/** Classify a thrown pg/transport error into a core error class. */
export function classifyPgError(err) {
  if (err?.name === "KeelTimeoutError") return "timeout"; // Keel's own soft deadline
  const code = err?.code;
  if (typeof code === "string") {
    if (CONN_ERROR_CODES.test(code)) return "conn";
    if (code.startsWith("08")) return "conn"; // SQLSTATE class 08: connection exception
    if (code === "57P01" || code === "57P02" || code === "57P03") return "conn"; // admin shutdown / crash / cannot connect now
    if (code === "57014") return "timeout"; // query_canceled (server-side statement_timeout)
    if (code === "53300" || code === "53400") return "conn"; // too_many_connections / configuration limit exceeded
  }
  return "other";
}

/**
 * Wrap a `Client.prototype.query` so each plain promise-form call routes
 * through the backend. `deps.backend`/`deps.discovery` override the globals
 * (for tests/embedding).
 */
export function makeWrappedQuery(original, deps = {}) {
  return function keelQuery(...args) {
    const backend = deps.backend ?? getBackend();
    const first = args[0];
    if (!backend || isSubmittable(first) || hasCallback(args)) {
      return original.apply(this, args); // disabled, or a shape we never wrap — pass through untouched
    }
    const text = queryTextFromArgs(args);
    const target = hostFromClient(this);
    const verb = sqlVerb(text) ?? "QUERY";
    const idempotent = isIdempotentSql(text);
    const op = `pg ${verb} ${target}`;
    const req = { v: 1, target, op, idempotent, args_hash: null };
    const timeoutMs = idempotent ? durationMs(backend.layer(target, "timeout")) : null;

    let liveResult;
    let haveResult = false;
    let liveErr;
    const started = performance.now();
    return backend
      .execute(req, async () => {
        try {
          const call = original.apply(this, args);
          liveResult = timeoutMs ? await withSoftTimeout(call, timeoutMs, "pg") : await call;
          haveResult = true;
          return { status: "ok", payload: null };
        } catch (err) {
          liveErr = err;
          return { status: "error", class: classifyPgError(err), message: err?.message ?? String(err) };
        }
      })
      .then((outcome) => {
        (deps.discovery ?? getDiscovery())?.observe(target, outcome, performance.now() - started);
        if (outcome.result === "ok") return attachOutcome(haveResult ? liveResult : outcome.payload, outcome);
        if (liveErr instanceof Error) throw attachOutcome(liveErr, outcome);
        const e = new KeelError(outcome.error?.code ?? "KEEL-E040", outcome.error?.message ?? "keel pg failure");
        throw attachOutcome(e, outcome);
      });
  };
}

/**
 * Patch `ClientClass.prototype.query` in place. Idempotent (a second patch is
 * a no-op) and reversible (returns an uninstall that restores the original).
 */
export function patchClientQuery(ClientClass, deps = {}) {
  const proto = ClientClass?.prototype;
  if (!proto || typeof proto.query !== "function" || proto.query.__keelWrapped) return () => {};
  const original = proto.query;
  const wrapped = makeWrappedQuery(original, deps);
  wrapped.__keelWrapped = true;
  wrapped.__keelOriginal = original;
  proto.query = wrapped;
  return function uninstall() {
    if (proto.query === wrapped) proto.query = original;
  };
}

/** The `pg` adapter pack — the four uniform operations (adapter-pack.md). */
export function pgPack({ cwd = process.cwd() } = {}) {
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
        name: "pg",
        version,
        confidence: isPinned(version) ? "pinned" : "best_effort",
      };
    },
    seams() {
      return [
        {
          patchPoint: "Client.prototype.query",
          upstreamApi: "pg — Client.query(text[, values]) / Client.query({text, values, name, rowMode})",
          whyStable:
            "the single query-dispatch chokepoint every pg call passes through — Pool.query checks out a real Client and calls this same method",
        },
      ];
    },
    targets() {
      return [
        {
          pattern: "<db host>",
          kind: "host",
          idempotencyRule:
            "keyed off the SQL verb of the query text: a bare SELECT is retryable; INSERT/UPDATE/DELETE, WITH (CTEs, possibly data-modifying), DDL, and any unrecognized statement are observed-not-retried (KEEL-E014)",
          argsHashRule: "none (queries are not cached — a result set is not safely replayable)",
        },
      ];
    },
    // host targets inherit [defaults.outbound]; no pg-specific fragment.
    defaults() {
      return {};
    },
  };
}

/**
 * Auto-detect `pg` and patch it (best-effort; never throws). Called by the
 * bootstrap. `pgModule` may be injected (tests); otherwise the module is
 * dynamically imported only when resolvable — absent `pg` is a silent no-op.
 */
export async function installPgPack({ cwd = process.cwd(), pgModule } = {}) {
  try {
    const mod = pgModule ?? (await loadPgModule(cwd));
    if (!mod || typeof mod.Client !== "function") return { active: false };
    const uninstall = patchClientQuery(mod.Client);
    return { active: true, name: "pg", uninstall };
  } catch {
    return { active: false }; // detection/patch is best-effort, never fatal
  }
}

async function loadPgModule(cwd) {
  const resolved = resolveFrom(cwd, MODULE_SPECIFIER);
  if (!resolved) return null;
  return import(pathToFileURL(resolved).href);
}
