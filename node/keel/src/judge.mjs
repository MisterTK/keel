/**
 * Front-end judgments for intercepted HTTP calls. These derivations are the
 * Node twin of the Python adapter judgments (Task 10 items 3–4) and MUST stay
 * in parity with them:
 *
 *   - idempotency:   safe/idempotent HTTP methods are retryable; POST/PATCH are
 *                    NOT retryable (Level 0 hard rule) UNLESS an idempotency
 *                    header is present (a POST with an idempotency key is safe).
 *   - args_hash:     a stable sha256 over method+URL(+string body); cache key
 *                    material. Null when the body is a non-serializable stream.
 *   - error class:   response status ≥500 or ==429 → typed `http` error (with
 *                    Retry-After parsed to ms). Every other status (2xx/3xx and
 *                    non-429 4xx) is passed through UNCHANGED as a success —
 *                    Keel never turns a real HTTP response into a failure.
 *                    Thrown transport errors map to conn/timeout/other.
 *
 * Target resolution (the LLM host map, Vertex regional suffix, and
 * `[target]` host/URL-pattern matching, `docs/targeting.md`) is NOT a
 * front-end judgment as of Task 11/SP-1: it is owned by the backend
 * (`backend.resolveTarget(method, host, scheme, port, path)`, proven
 * identical across native/stub/both language front ends by conformance
 * scenarios 36–38) and this module no longer duplicates it.
 */

import { createHash, randomUUID } from "node:crypto";

// The idempotent-method set is a cross-language parity contract with the Python
// twin, so it includes TRACE per the brief's list even though WHATWG fetch
// forbids actually sending a TRACE request — the *judgment* must still match.
const IDEMPOTENT_METHODS = new Set(["GET", "HEAD", "OPTIONS", "PUT", "DELETE", "TRACE"]);
const DEFAULT_IDEMPOTENCY_HEADERS = ["idempotency-key", "x-idempotency-key"];

/** Normalize a fetch(input, init) pair into method + URL + a Headers view. */
export function normalizeRequest(input, init) {
  let url;
  let method;
  let headers;
  let body;
  if (typeof Request !== "undefined" && input instanceof Request) {
    url = input.url;
    method = (init?.method ?? input.method ?? "GET").toUpperCase();
    headers = new Headers(init?.headers ?? input.headers);
    body = init?.body;
  } else {
    url = typeof input === "string" ? input : input?.href ?? String(input);
    method = (init?.method ?? "GET").toUpperCase();
    headers = new Headers(init?.headers);
    body = init?.body;
  }
  return { url, method, headers, body };
}

/**
 * Decide idempotency. `idempotencyHeader` is the target's configured header
 * (from policy `idempotency.header`), if any. A POST/PATCH is retryable only
 * when a recognized idempotency header is actually present on the request.
 */
export function isIdempotent(method, headers, idempotencyHeader) {
  if (IDEMPOTENT_METHODS.has(method)) return true;
  const candidates = idempotencyHeader
    ? [idempotencyHeader.toLowerCase()]
    : DEFAULT_IDEMPOTENCY_HEADERS;
  return candidates.some((h) => headers.has(h));
}

/**
 * Mint one opaque idempotency key (contracts/adapter-pack.md "Idempotency-key
 * injection" rule 2: ONE per logical call, minted before the first attempt).
 * A fresh UUIDv4 in production; `installFetch`'s `mintIdempotencyKey` option
 * substitutes a deterministic source for tests (parity with the Python twin's
 * injectable `new_idempotency_key`).
 */
export function defaultMintIdempotencyKey() {
  return randomUUID();
}

/**
 * The `(target)#(argsHash)` key identifying a Tier 2 effect step — matches
 * `FlowHandle::step_key` in crates/keel-core/src/flow.rs exactly (`"-"` for a
 * missing `argsHash`), so a peek here lands on the journal row a resumed step
 * would occupy. Parity with the Python twin's `_http.step_key`.
 */
export function stepKey(target, argsHash) {
  return `${target}#${argsHash ?? "-"}`;
}

/**
 * The idempotency key to INJECT for this call, or `null` to inject nothing
 * (contracts/adapter-pack.md "Idempotency-key injection"):
 *
 *   - `null` when the method is already idempotent (injection only ever
 *     targets an unsafe method — rule 1), no `idempotency.header` is
 *     configured for the target, or the caller already supplied the
 *     configured header — a caller-supplied key always wins; never
 *     overwritten.
 *   - otherwise `recordedKey` when given (a Tier 2 resume's key, journaled
 *     with the crashed step — rule 3 — reused verbatim so the re-execution is
 *     deduplicable on the provider side), else a freshly minted key (`mint`,
 *     defaulting to `defaultMintIdempotencyKey`) — stable across Tier 1
 *     retries because the caller mints/injects it once, before the first
 *     attempt, and reuses the same `headers` object on every retry.
 *
 * The returned key must never feed `argsHash` (rule 5): it is not part of the
 * caller's arguments, and folding it in would fence Tier 2 replay.
 */
export function resolveIdempotencyInjection(
  method,
  headers,
  idempotencyHeader,
  mint = defaultMintIdempotencyKey,
  recordedKey = null,
) {
  if (!idempotencyHeader || IDEMPOTENT_METHODS.has(method)) return null;
  if (headers.has(idempotencyHeader.toLowerCase())) return null;
  return recordedKey ?? mint();
}

export function argsHash(method, url, body) {
  const h = createHash("sha256");
  h.update(method);
  h.update("\n");
  h.update(url);
  if (typeof body === "string") {
    h.update("\n");
    h.update(body);
  } else if (body instanceof URLSearchParams) {
    h.update("\n");
    h.update(body.toString());
  } else if (body != null && !(typeof body === "object")) {
    h.update("\n");
    h.update(String(body));
  }
  // Streams/Blobs/FormData are not read (would consume the body); the hash then
  // covers method+URL only, which is correct for cache-key purposes here.
  return h.digest("hex");
}

/** Stable JSON with sorted object keys (no whitespace), so two equivalent bodies
 *  hash identically regardless of key order/spacing. Cache-key material only. */
function stableStringify(value) {
  return JSON.stringify(value, (_key, v) => {
    if (v && typeof v === "object" && !Array.isArray(v)) {
      const sorted = {};
      for (const k of Object.keys(v).sort()) sorted[k] = v[k];
      return sorted;
    }
    return v;
  });
}

/** Canonical string for a request body used as a dev-cache key, or null when
 *  there is no replayable body. Mirrors the Python twin's `_canonical_json`: a
 *  buffered JSON body is key-sorted + whitespace-free (so two equivalent prompts
 *  share a cache key even when the client serialized them differently); a
 *  non-JSON buffered body is hashed verbatim; a stream/Blob/FormData body is not
 *  replayable → null. */
function canonicalBody(body) {
  let raw;
  if (typeof body === "string") raw = body;
  else if (body instanceof ArrayBuffer) raw = Buffer.from(new Uint8Array(body)).toString("utf8");
  else if (ArrayBuffer.isView(body))
    raw = Buffer.from(body.buffer, body.byteOffset, body.byteLength).toString("utf8");
  else return null; // None / ReadableStream / Blob / FormData: not cache-replayable
  try {
    return stableStringify(JSON.parse(raw));
  } catch {
    return raw; // not JSON: hash the raw bytes/string verbatim
  }
}

/**
 * Cache-key material for one intercepted call, or null to disable caching for
 * it. The Node twin of the Python `derive_args_hash` — the two front ends MUST
 * agree on which calls are cacheable:
 *
 *   - idempotent GET   → sha256(method + url [+ buffered body]).
 *   - LLM POST (llm:*) → the documented dev-cache exception: sha256 over
 *     (method, url, canonicalized JSON body). This enables dev-loop REPLAY of an
 *     identical prompt; it does NOT make the POST retryable — idempotency is a
 *     separate judgment, still false for a bare POST (a cache LOOKUP needs no
 *     idempotency; a RETRY does). A streaming/unbuffered body yields null.
 *   - everything else  → null.
 */
export function deriveArgsHash(target, method, url, body) {
  if (method === "GET") return argsHash(method, url, body);
  if (method === "POST" && target.startsWith("llm:")) {
    const canon = canonicalBody(body);
    return canon === null ? null : argsHash(method, url, canon);
  }
  return null;
}

/** Parse a Retry-After header value to milliseconds, or undefined. */
export function parseRetryAfter(value, nowMs = Date.now()) {
  if (value == null) return undefined;
  const s = String(value).trim();
  if (/^\d+$/.test(s)) return Number(s) * 1000;
  const when = Date.parse(s);
  if (Number.isFinite(when)) {
    const delta = when - nowMs;
    return delta > 0 ? delta : 0;
  }
  return undefined;
}

/** Classify a thrown fetch/transport error into a core error class.
 *
 * Keel's OWN per-attempt deadline aborts the request with a `DOMException` named
 * "TimeoutError" (see `fetch.mjs`), so that is a `timeout` (in the default
 * retry.on → retried). A CALLER's `AbortController` fires an `AbortError`: that
 * is user cancellation, class `cancelled` (excluded from the default retry.on →
 * immediately terminal, KEEL-E015). Distinguishing them means a stop button
 * propagates at once with the original AbortError, instead of grinding through
 * the whole backoff schedule and mis-recording a retried timeout — matching the
 * Python twin, whose `CancelledError` (a BaseException) escapes retries too. */
export function classifyThrow(err) {
  const name = err?.name;
  if (name === "TimeoutError") return "timeout"; // Keel's own deadline abort
  if (name === "AbortError") return "cancelled"; // caller-initiated cancellation
  if (name === "TypeError") return "conn"; // fetch network failures throw TypeError
  return "other";
}

/** True when a response status should be treated as a transient typed error. */
export function isTransientStatus(status) {
  return status === 429 || status >= 500;
}

// --- response (de)serialization for the core payload -------------------------
//
// The core `payload` MUST be JSON (contracts/core_api.rs: `payload: Value`) — the
// stub tolerates opaque objects, the real native core does not. So a `Response`
// never crosses the boundary as the payload: we send a JSON ENVELOPE and keep the
// live `Response` side-band. On the live success path the front end returns the
// held live object (byte-transparent); only a CACHE HIT (in-process or, under the
// persistent journal, across runs) rebuilds a `Response` from the envelope.

/** Marker key identifying a serialized HTTP response envelope. */
export const HTTP_ENVELOPE_MARK = "__keel_http__";

/**
 * A JSON-serializable envelope of a `Response` for the core payload. `withBody`
 * (only for cacheable calls) clones the response and buffers the body base64 so
 * a cache hit can rebuild it; otherwise it's a cheap status/headers envelope and
 * the body is never read (the live response is returned on the success path).
 */
export async function responseEnvelope(resp, { withBody = false } = {}) {
  const headers = [];
  for (const [k, v] of resp.headers) headers.push([k, v]);
  const env = { [HTTP_ENVELOPE_MARK]: 1, status: resp.status, status_text: resp.statusText, headers };
  if (withBody) {
    try {
      const buf = await resp.clone().arrayBuffer();
      env.body_b64 = Buffer.from(buf).toString("base64");
    } catch {
      // Unbuffered/streaming body: leave it out (such calls are not replayable;
      // the live response is still returned byte-transparently on success).
    }
  }
  return env;
}

// Statuses that WHATWG forbids a body on; rebuild them with a null body.
const NULL_BODY_STATUS = new Set([101, 204, 205, 304]);

/** Rebuild a `Response` from an envelope (cache-hit replay). */
export function rebuildResponse(env) {
  const status = env?.status ?? 200;
  const headers = Array.isArray(env?.headers) ? env.headers : [];
  const bytes = env?.body_b64 != null ? Buffer.from(env.body_b64, "base64") : null;
  const body = NULL_BODY_STATUS.has(status) || !bytes || bytes.length === 0 ? null : bytes;
  return new Response(body, { status, statusText: env?.status_text ?? "", headers });
}
