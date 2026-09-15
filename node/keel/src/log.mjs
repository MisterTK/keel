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

/** Sorted keys, no spaces — byte-identical to Python's
 *  `json.dumps(sort_keys=True, separators=(",", ":"))`. */
export function dumpsLine(obj) {
  const sorted = {};
  for (const k of Object.keys(obj).sort()) sorted[k] = obj[k];
  return `${JSON.stringify(sorted)}\n`;
}

/** Write `text` (already newline-terminated) or the JSON form of `obj`. */
export function emit(env, text, obj, proc = process) {
  proc.stderr.write(jsonLogs(env) ? dumpsLine(obj) : text);
}
