/**
 * `keel record run` capture: a transparent tee over the runtime `Backend`
 * that appends every intercepted effect's request/outcome envelope to a
 * recording file, without changing the wrapped call's behavior.
 *
 * See `docs/recording-format.md` for the full (non-contract) line format,
 * the capture-seam rationale, and documented v1 limitations. In short: this
 * module never touches `contracts/` — the recording lives entirely at the
 * front-end boundary between an adapter (`fetch`, `ts:` function wrappers,
 * …) and the `Backend` shape (`backend.mjs`).
 */

import { mkdirSync, appendFileSync } from "node:fs";
import { dirname } from "node:path";

export const RECORDING_VERSION = 1;

/**
 * Header names redacted from a recorded outcome's HTTP response envelope
 * (`payload.headers`) before the line is written. Extend per-run with
 * `KEEL_RECORD_REDACT_HEADERS` (comma list, merged with these defaults) —
 * see `redactHeadersFromEnv`.
 */
export const DEFAULT_REDACT_HEADERS = new Set([
  "authorization",
  "proxy-authorization",
  "cookie",
  "set-cookie",
  "x-api-key",
  "api-key",
  "x-auth-token",
  "x-goog-api-key",
]);

const TRUTHY = new Set(["1", "true", "yes"]);
function isTruthy(v) {
  return TRUTHY.has(String(v ?? "").trim().toLowerCase());
}

/** The active redact set: the defaults plus any comma-separated names in
 * `KEEL_RECORD_REDACT_HEADERS` (never a replacement of the defaults). */
export function redactHeadersFromEnv(env) {
  const extra = String(env.KEEL_RECORD_REDACT_HEADERS ?? "")
    .split(",")
    .map((s) => s.trim().toLowerCase())
    .filter(Boolean);
  return new Set([...DEFAULT_REDACT_HEADERS, ...extra]);
}

/** Redact header VALUES (never keys) in an HTTP response envelope's
 * `headers` list (`[[name, value], ...]`, see `judge.mjs`'s
 * `responseEnvelope`). Any other payload shape (a `ts:` function's JSON
 * return value, or an envelope with no `headers`) is returned unchanged. */
function redactPayload(payload, redact) {
  if (payload === null || typeof payload !== "object" || !Array.isArray(payload.headers)) {
    return payload;
  }
  return {
    ...payload,
    headers: payload.headers.map(([k, v]) => [
      k,
      typeof k === "string" && redact.has(k.toLowerCase()) ? "[REDACTED]" : v,
    ]),
  };
}

/** Whether `outcome`'s payload carries an actual captured body: a non-empty
 * `body_b64` for an HTTP envelope, or any non-null payload for a `ts:`
 * function target. Purely informational (`keel record list`) — never
 * affects matching. */
function hasBody(outcome) {
  const payload = outcome && typeof outcome === "object" ? outcome.payload : undefined;
  if (payload === null || payload === undefined) return false;
  if (typeof payload !== "object") return true;
  return typeof payload.body_b64 === "string" || !("__keel_http__" in payload);
}

/** A single-writer append-only NDJSON file. Never throws into the caller's
 * real effect — a write failure degrades to dropping that one line rather
 * than breaking the program being recorded (best-effort observability, same
 * posture as the Tier 1 event sink). */
class JsonlWriter {
  #path;
  #seq = 0;
  constructor(path) {
    this.#path = path;
    mkdirSync(dirname(path), { recursive: true });
  }
  writeMeta({ id, language, target, args, redactedHeaders }) {
    this.#write({
      v: RECORDING_VERSION,
      type: "meta",
      id,
      language,
      target,
      args,
      started_at_ms: Date.now(),
      redacted_headers: [...redactedHeaders].sort(),
    });
  }
  writeCall({ target, op, idempotent, argsHash, outcome, latencyMs }) {
    this.#seq += 1;
    this.#write({
      v: RECORDING_VERSION,
      type: "call",
      seq: this.#seq,
      target,
      op,
      idempotent: Boolean(idempotent),
      args_hash: argsHash ?? null,
      attempts: outcome && typeof outcome === "object" ? (outcome.attempts ?? null) : null,
      latency_ms: latencyMs,
      body_captured: hasBody(outcome),
      outcome,
    });
  }
  #write(obj) {
    let text;
    try {
      text = JSON.stringify(obj);
    } catch {
      text = JSON.stringify({
        v: RECORDING_VERSION,
        type: "error",
        note: "unserializable record dropped",
      });
    }
    try {
      appendFileSync(this.#path, `${text}\n`, "utf8");
    } catch {
      /* best-effort — recording never breaks the program being recorded */
    }
  }
}

/**
 * A transparent tee over `inner` (a `Backend`): `execute` is forwarded
 * unchanged and its outcome is appended to a recording; every other member
 * (`report`, `layer`, `persistent`) delegates straight through. Recording is
 * a pure observer — it never alters what the wrapped call receives.
 */
export class RecordingBackend {
  #inner;
  #writer;
  #redact;
  constructor(inner, writer, redact) {
    this.#inner = inner;
    this.#writer = writer;
    this.#redact = redact;
  }
  configure(policy) {
    return this.#inner.configure(policy);
  }
  async execute(request, effect) {
    const started = performance.now();
    const outcome = await this.#inner.execute(request, effect);
    this.#record(request, outcome, performance.now() - started);
    return outcome;
  }
  report() {
    return this.#inner.report();
  }
  get persistent() {
    return this.#inner.persistent === true;
  }
  layer(target, key) {
    return this.#inner.layer(target, key);
  }
  flushEvents() {
    this.#inner.flushEvents?.();
  }
  #record(request, outcome, elapsedMs) {
    if (request === null || typeof request !== "object") return;
    if (outcome === null || typeof outcome !== "object") return;
    const redactedOutcome =
      "payload" in outcome ? { ...outcome, payload: redactPayload(outcome.payload, this.#redact) } : outcome;
    this.#writer.writeCall({
      target: String(request.target ?? ""),
      op: String(request.op ?? ""),
      idempotent: Boolean(request.idempotent),
      argsHash: request.args_hash ?? null,
      outcome: redactedOutcome,
      latencyMs: Math.round(elapsedMs),
    });
  }
}

/**
 * Wrap `backend` for `keel record run`: writes the `meta` header
 * immediately, then returns the tee to install as the process's runtime
 * backend (in place of `backend` in every seam the bootstrap wires —
 * `installFetch`, `setRuntime`, framework packs). Also prints the one-line
 * "recording to …" banner (suppressed by `KEEL_QUIET`, matching
 * `bootstrap.mjs`'s own banner).
 */
export function installRecording(backend, { path, target, args, env }) {
  const redact = redactHeadersFromEnv(env);
  const writer = new JsonlWriter(path);
  const id = basenameNoExt(path);
  writer.writeMeta({ id, language: "node", target, args: [...args], redactedHeaders: redact });
  if (!isTruthy(env.KEEL_QUIET)) {
    process.stderr.write(`keel ▸ recording to ${path} — \`keel record list\` to inspect\n`);
  }
  return new RecordingBackend(backend, writer, redact);
}

function basenameNoExt(path) {
  const base = path.split(/[/\\]/).pop() ?? path;
  const dot = base.lastIndexOf(".");
  return dot > 0 ? base.slice(0, dot) : base;
}
