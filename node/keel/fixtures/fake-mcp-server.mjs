/**
 * Scripted fake MCP server for tests — a tiny process speaking newline-delimited
 * JSON-RPC 2.0 over stdio (the wire the stdio transport uses). It is stateless
 * and deterministic; the CLIENT drives behavior via each request's params.mode:
 *
 *   mode "hang"  → the server NEVER replies (models a frozen/hung server).
 *   mode "error" → reply with a JSON-RPC error object.
 *   mode "ok"    → reply with a result echoing params (default).
 *
 * This lets one server exercise the hung-server timeout path (mode "hang") and
 * the recovery path in the same fixture without any shared mutable state.
 */

let buffer = "";
process.stdin.setEncoding("utf8");
process.stdin.on("data", (chunk) => {
  buffer += chunk;
  let nl;
  while ((nl = buffer.indexOf("\n")) >= 0) {
    const line = buffer.slice(0, nl);
    buffer = buffer.slice(nl + 1);
    if (line.trim()) handle(line);
  }
});

function handle(line) {
  let msg;
  try {
    msg = JSON.parse(line);
  } catch {
    return; // ignore malformed lines
  }
  const { id, params } = msg;
  const mode = params?.mode ?? "ok";
  if (mode === "hang") return; // never respond
  if (mode === "error") {
    reply({ jsonrpc: "2.0", id, error: { code: -32000, message: "scripted error" } });
    return;
  }
  reply({ jsonrpc: "2.0", id, result: { echo: params ?? null } });
}

function reply(obj) {
  process.stdout.write(JSON.stringify(obj) + "\n");
}
