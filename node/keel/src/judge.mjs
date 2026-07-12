/**
 * Front-end judgments for intercepted HTTP calls. These derivations are the
 * Node twin of the Python adapter judgments (Task 10 items 3–4) and MUST stay
 * in parity with them:
 *
 *   - target:        the URL hostname, unless the host maps to an LLM provider,
 *                    in which case the semantic target `llm:<provider>`.
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
 */

import { createHash } from "node:crypto";

/**
 * Host → LLM provider map. This is a parity contract with the Python front end;
 * adding a host here changes which default pack (`defaults.llm`) applies, so it
 * is deliberately small and conservative. Extend in lockstep across languages.
 */
export const LLM_HOST_PROVIDERS = Object.freeze({
  "api.openai.com": "openai",
  "api.anthropic.com": "anthropic",
  "generativelanguage.googleapis.com": "google-genai",
});

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

export function resolveTarget(hostname) {
  const provider = LLM_HOST_PROVIDERS[hostname];
  return provider ? `llm:${provider}` : hostname;
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

/** Classify a thrown fetch/transport error into a core error class. */
export function classifyThrow(err) {
  const name = err?.name;
  if (name === "AbortError" || name === "TimeoutError") return "timeout";
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
