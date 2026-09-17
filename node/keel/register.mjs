// KEEL_ENABLE-gated preload: `NODE_OPTIONS="--import keelrun/register"`.
// The .env-parity twin of the Python wheel's keelrun_activate.pth — an idle
// install costs one env check; activation failures never take down the host
// (one stderr line, app continues unwrapped). `keelrun/hook` remains the
// unconditional preload that `keel run` itself injects.
import { createRequire } from "node:module";

const TRUTHY = new Set(["1", "true", "yes"]);

/**
 * Report an activation failure: Keel is OFF and the host keeps serving.
 *
 * This is the SECOND of Keel's two activation refusals, and #130's defect
 * applies to it in full: under `KEEL_LOG_FORMAT=json` an unstructured,
 * unranked prose line is indistinguishable from a healthy start in any
 * `severity>=ERROR` view. It reaches CCR-11's most likely rollout accident —
 * a `keel.toml` carrying `until.absent` read by a Keel too old to know the
 * key is KEEL-E001 here, which lands the whole process unprotected — so it
 * carries the same `severity: "ERROR"` the `policy-missing-at-keel-cwd`
 * refusal does.
 *
 * The text form is byte-identical to what it has always been, and
 * `python/keel/src/keel/_auto.py`'s `_refusal` is the twin: same code, same
 * keys, same severity. Emitting is itself wrapped, because the failure being
 * reported may be the very import that would have supplied `emit` — the
 * fail-open contract outranks the structured form.
 */
async function refusal(err) {
  const text = `keel ▸ auto-activation failed (${err?.message ?? err}); continuing without keel\n`;
  try {
    const { emit } = await import("./src/log.mjs");
    let version = "unknown";
    try {
      version = createRequire(import.meta.url)("./package.json").version ?? "unknown";
    } catch {
      // Same fail-open degrade as bootstrap.mjs's readVersion: one field
      // goes to "unknown" rather than swallowing the refusal itself.
    }
    emit(process.env, text, {
      keel: "error",
      code: "activation-failed",
      keel_cwd: process.env.KEEL_CWD || null,
      message: text.slice("keel ▸ ".length).replace(/\n$/, ""),
      severity: "ERROR",
      version,
    });
  } catch {
    process.stderr.write(text);
  }
}

if (TRUTHY.has((process.env.KEEL_ENABLE ?? "").trim().toLowerCase())) {
  try {
    await import("./hook.mjs");
  } catch (err) {
    await refusal(err);
  }
}
