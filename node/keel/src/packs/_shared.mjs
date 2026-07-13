/**
 * Tiny helpers shared by every framework/library pack (`mcp.mjs`, `pg.mjs`,
 * `ioredis.mjs`, `mysql2.mjs`, ...). Not itself a pack — no `detect`/`seams`/
 * `targets`/`defaults` surface.
 */

import { createRequire } from "node:module";
import { join } from "node:path";

/**
 * Resolve `specifier` first from the user's project (`cwd/package.json`), so
 * pack detection reflects what the app actually has installed, then from
 * Keel's own dependency graph as a fallback. Returns the resolved absolute
 * path, or `null` when neither resolves. Never throws.
 */
export function resolveFrom(cwd, specifier) {
  try {
    return createRequire(join(cwd, "package.json")).resolve(specifier);
  } catch {
    try {
      return createRequire(import.meta.url).resolve(specifier);
    } catch {
      return null;
    }
  }
}

/** Parse a policy duration string (`"200ms"`, `"30s"`, `"5m"`, `"1h"`) to
 *  milliseconds, or `null` if unparseable/absent. */
export function durationMs(v) {
  const m = /^(\d+)(ms|s|m|h)$/.exec(String(v ?? "").trim());
  if (!m) return null;
  const mult = { ms: 1, s: 1000, m: 60000, h: 3600000 }[m[2]];
  return Number(m[1]) * mult;
}

/**
 * Race `promise` against a `ms` timer, for library seams with no cooperative
 * cancellation hook (pg, ioredis, mysql2 — unlike fetch/mcp, which pass an
 * `AbortSignal` into the call). On timeout the returned promise rejects with a
 * `KeelTimeoutError`; `promise`'s eventual settlement is still observed (a
 * no-op catch) so an abandoned attempt never becomes an unhandled rejection —
 * but it is NOT cancelled: the underlying call keeps running until its own
 * result/error arrives, then is silently discarded. This is a documented
 * SOFT timeout ("stop waiting"), not a true cancel. `label` names the caller
 * in the error message (e.g. "pg", "ioredis", "mysql2"). */
export function withSoftTimeout(promise, ms, label = "keel") {
  if (!ms || ms <= 0) return promise;
  promise.catch(() => {}); // never let an abandoned attempt surface an unhandled rejection
  let timer;
  const timeout = new Promise((_resolve, reject) => {
    timer = setTimeout(() => {
      reject(Object.assign(new Error(`Keel ${label} soft timeout`), { name: "KeelTimeoutError" }));
    }, ms);
    if (typeof timer.unref === "function") timer.unref();
  });
  return Promise.race([promise, timeout]).finally(() => clearTimeout(timer));
}

/** Connection-level Node/network error codes shared across pg/ioredis/mysql2
 *  clients — a thrown error carrying one of these `.code` values is always a
 *  `conn` class, regardless of which library raised it. */
export const CONN_ERROR_CODES =
  /^(ECONNREFUSED|ECONNRESET|EPIPE|ENOTFOUND|EHOSTUNREACH|ETIMEDOUT)$/;
