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
  "generativelanguage.googleapis.com": "google",
});

const IDEMPOTENT_METHODS = new Set(["GET", "HEAD", "OPTIONS", "PUT", "DELETE"]);
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
