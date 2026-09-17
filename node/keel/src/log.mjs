/**
 * `KEEL_LOG_FORMAT=json`: Keel's three deployment-evidence lines — the
 * activation banner, the exit summary, and the activation refusal — each
 * become one JSON object (sorted keys, no spaces) so container log pipelines
 * index their fields (field report 2026-09-15, F10). Nothing else converts:
 * other `keel ▸ …` prose Keel can write at runtime (a pack's unwrapped-tool
 * warning, a `KEEL_SIM_PLAN` read failure, …) stays prose. Text is the
 * default; there is no auto-detect (byte-identity tests and `keel run` piping
 * depend on the text form).
 *
 * The Python front end's `keel/_log.py` is the byte-for-byte twin: same
 * recognized value, same separators, same key ordering. Keep every emitted
 * value a string, an int or null — `JSON.stringify` and Python's
 * `json.dumps(ensure_ascii=False)` agree on exactly those, and nothing here
 * needs more.
 */

/**
 * Whether this process emits JSON lines. Exactly one recognized value;
 * anything else (including `1`/`true`) silently keeps the text form.
 */
export function jsonLogs(env = process.env) {
  return String(env?.KEEL_LOG_FORMAT ?? "").trim().toLowerCase() === "json";
}

function isPlainObject(v) {
  if (v === null || typeof v !== "object" || Array.isArray(v)) return false;
  const proto = Object.getPrototypeOf(v);
  return proto === Object.prototype || proto === null;
}

/**
 * Recursively sort plain-object keys; arrays keep their order, primitives
 * pass through unchanged. Python's `json.dumps(sort_keys=True)` already
 * sorts nested objects — this is `dumpsLine`'s twin for that behavior, so a
 * value like `unprotected_by_target` (a nested target -> count object)
 * serializes identically in both languages (#96).
 */
function sortKeys(value) {
  if (Array.isArray(value)) return value.map(sortKeys);
  // A Date, Map, or class instance is NOT ours to rebuild — rebuilding it
  // would drop its toJSON and serialise its own enumerable keys instead
  // (#103). Only plain objects get key-sorted.
  if (!isPlainObject(value)) return value;
  const out = {};
  for (const k of Object.keys(value).sort()) out[k] = sortKeys(value[k]);
  return out;
}

/** Sorted keys, no spaces — byte-identical to Python's
 *  `json.dumps(sort_keys=True, separators=(",", ":"))`. */
export function dumpsLine(obj) {
  return `${JSON.stringify(sortKeys(obj))}\n`;
}

/** Write `text` (already newline-terminated) or the JSON form of `obj`. */
export function emit(env, text, obj, proc = process) {
  proc.stderr.write(jsonLogs(env) ? dumpsLine(obj) : text);
}
