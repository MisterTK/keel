// A bare Node service call using global fetch. It knows nothing about Keel.
// Run plainly it throws on the first 500; run under `keel run` (the Node loader
// intercepts fetch) the 5xx is retried and it prints "service ok".
const url = process.env.KEEL_DEMO_URL;
const resp = await fetch(url);
if (!resp.ok) throw new Error(`HTTP ${resp.status}`);
process.stdout.write("service ok\n");
