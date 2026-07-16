// KEEL_ENABLE-gated preload: `NODE_OPTIONS="--import keelrun/register"`.
// The .env-parity twin of the Python wheel's keelrun_activate.pth — an idle
// install costs one env check; activation failures never take down the host
// (one stderr line, app continues unwrapped). `keelrun/hook` remains the
// unconditional preload that `keel run` itself injects.
const TRUTHY = new Set(["1", "true", "yes"]);
if (TRUTHY.has((process.env.KEEL_ENABLE ?? "").trim().toLowerCase())) {
  try {
    await import("./hook.mjs");
  } catch (err) {
    process.stderr.write(`keel ▸ auto-activation failed (${err?.message ?? err}); continuing without keel\n`);
  }
}
