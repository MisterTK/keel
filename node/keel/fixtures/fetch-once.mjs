// Fixture: exactly one intercepted `fetch` against a throwaway local server,
// then a natural event-loop drain (so `beforeExit` fires). Used by the
// console-summary child-process tests; stdout carries only the status line.
import { createServer } from "node:http";

const server = createServer((_req, res) => {
  res.writeHead(200, { "content-type": "text/plain" });
  res.end("ok");
});
await new Promise((resolve) => server.listen(0, "127.0.0.1", resolve));
const { port } = server.address();
const res = await fetch(`http://127.0.0.1:${port}/ping`);
process.stdout.write(`status ${res.status}\n`);
server.close();
