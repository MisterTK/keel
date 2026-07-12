/**
 * `mcp:` transport pack (DX spec §4.1: "mcp:<server> — MCP client transports
 * (stdio + HTTP). Per-server timeout, retry, breaker; a hung MCP server degrades
 * gracefully instead of freezing the agent").
 *
 * Seam: `Client.prototype.request` from `@modelcontextprotocol/sdk/client`. That
 * method is the JSON-RPC request/response CORRELATION boundary — the point where
 * one request goes out and its matching response comes back — and it is shared
 * by ALL client transports (stdio, streamable HTTP). A transport's `send()` is
 * fire-and-forget (no response correlation), so it is not a resilience boundary;
 * wrapping `request` is the narrowest stable seam that covers both transports at
 * once. The patch mutates the prototype method (reversible: uninstall restores
 * the original), so `uninstall = remove the package` holds (DX invariant 2).
 *
 * Target = `mcp:<server-name>` where the server name comes from
 * `client.getServerVersion()?.name` (available after `connect()`; falls back to
 * "unknown" for the pre-handshake initialize request). Per-server policy comes
 * from `[target."mcp:<server>"]`, else `[defaults.outbound]` (mcp: is not an
 * `llm:` target). MCP method calls are treated as retryable (idempotent): the
 * transport assigns a fresh JSON-RPC id per attempt, and listing an mcp: server
 * opts its calls into retry — set `retry = { attempts = 1 }` to disable. Calls
 * are not cached (args_hash null) — MCP tool calls can be side-effecting.
 *
 * Timeout: the core does not enforce timeouts, so the pack imposes a per-attempt
 * deadline from the target's `timeout` (both by racing the request AND passing
 * an AbortSignal into the MCP request options, so a cooperative client cancels
 * and a non-cooperative one still unblocks). A hung server therefore times out
 * per policy, retries per policy, and finally raises KEEL-E010 — it degrades
 * gracefully instead of freezing the agent.
 */

import { createRequire } from "node:module";
import { pathToFileURL } from "node:url";
import { join } from "node:path";
import { getBackend, getDiscovery, attachOutcome } from "../runtime.mjs";
import { KeelError } from "../engine.mjs";

const CLIENT_SPECIFIER = "@modelcontextprotocol/sdk/client/index.js";
const PKG_SPECIFIER = "@modelcontextprotocol/sdk/package.json";
const PINNED_VERSION = "1.29.0"; // the SDK version these seam tests certify

function durationMs(v) {
  const m = /^(\d+)(ms|s|m|h)$/.exec(String(v ?? "").trim());
  if (!m) return null;
  const mult = { ms: 1, s: 1000, m: 60000, h: 3600000 }[m[2]];
  return Number(m[1]) * mult;
}

function safeServerName(client) {
  try {
    const name = client?.getServerVersion?.()?.name;
    return typeof name === "string" && name ? name : "unknown";
  } catch {
    return "unknown";
  }
}

/** Classify a thrown MCP/transport error into a core error class. */
export function classifyMcpError(err) {
  const name = err?.name;
  if (name === "TimeoutError" || name === "AbortError") return "timeout";
  const code = err?.code;
  if (code === -32001) return "timeout"; // MCP RequestTimeout JSON-RPC code
  if (typeof code === "string" && /^(ECONNREFUSED|ECONNRESET|EPIPE|ENOTFOUND)$/.test(code))
    return "conn";
  if (/\b(closed|disconnect|connection)\b/i.test(err?.message ?? "")) return "conn";
  return "other";
}

/** Arm a per-attempt deadline: an AbortSignal + a promise that rejects on
 *  timeout. Composes the caller's signal if any. Returns a cleanup. */
function armDeadline(callerSignal, timeoutMs) {
  if (!timeoutMs || timeoutMs <= 0) return { signal: callerSignal, expired: null, cancel() {} };
  const controller = new AbortController();
  const onAbort = () => controller.abort(callerSignal?.reason);
  if (callerSignal) {
    if (callerSignal.aborted) controller.abort(callerSignal.reason);
    else callerSignal.addEventListener("abort", onAbort, { once: true });
  }
  let timer;
  const expired = new Promise((_resolve, reject) => {
    timer = setTimeout(() => {
      const e = new DOMException("Keel MCP timeout", "TimeoutError");
      controller.abort(e);
      reject(e);
    }, timeoutMs);
    if (typeof timer.unref === "function") timer.unref();
  });
  return {
    signal: controller.signal,
    expired,
    cancel() {
      clearTimeout(timer);
      callerSignal?.removeEventListener?.("abort", onAbort);
    },
  };
}

/**
 * Wrap an MCP client `request` method so each JSON-RPC request routes through
 * the backend. `deps.backend` overrides the global backend (for tests/embedding).
 */
export function makeWrappedRequest(original, deps = {}) {
  return async function keelMcpRequest(request, resultSchema, options) {
    const backend = deps.backend ?? getBackend();
    if (!backend) return original.call(this, request, resultSchema, options); // disabled: pass-through
    const server = safeServerName(this);
    const target = `mcp:${server}`;
    const method = request?.method ?? "?";
    const op = `mcp ${server} ${method}`;
    const req = { v: 1, target, op, idempotent: true, args_hash: null };
    const timeoutMs = durationMs(backend.layer(target, "timeout"));

    const outcome = await backend.execute(req, async () => {
      const deadline = armDeadline(options?.signal, timeoutMs);
      const attemptOptions = deadline.signal ? { ...options, signal: deadline.signal } : options;
      try {
        const call = original.call(this, request, resultSchema, attemptOptions);
        const result = deadline.expired ? await Promise.race([call, deadline.expired]) : await call;
        return { status: "ok", payload: result };
      } catch (err) {
        return {
          status: "error",
          class: classifyMcpError(err),
          message: err?.message ?? String(err),
          original: err,
        };
      } finally {
        deadline.cancel();
      }
    });

    (deps.discovery ?? getDiscovery())?.observe(target, null);
    if (outcome.result === "ok") return outcome.payload;
    const orig = outcome.error?.original;
    if (orig instanceof Error) throw attachOutcome(orig, outcome);
    const e = new KeelError(outcome.error?.code ?? "KEEL-E040", outcome.error?.message ?? "keel MCP failure");
    throw attachOutcome(e, outcome);
  };
}

/**
 * Patch `ClientClass.prototype.request` in place. Idempotent (a second patch is
 * a no-op) and reversible (returns an uninstall that restores the original).
 */
export function patchClientRequest(ClientClass, deps = {}) {
  const proto = ClientClass?.prototype;
  if (!proto || typeof proto.request !== "function" || proto.request.__keelWrapped) return () => {};
  const original = proto.request;
  const wrapped = makeWrappedRequest(original, deps);
  wrapped.__keelWrapped = true;
  wrapped.__keelOriginal = original;
  proto.request = wrapped;
  return function uninstall() {
    if (proto.request === wrapped) proto.request = original;
  };
}

function resolveFrom(cwd, specifier) {
  // Resolve first from the user's project, then from Keel's own deps.
  try {
    return createRequire(join(cwd, "package.json")).resolve(specifier);
  } catch {
    try {
      return createRequire(import.meta.url).resolve(specifier);
    } catch {
      return null;
    }
  }
}

/** The `mcp:` adapter pack — the four uniform operations (adapter-pack.md). */
export function mcpPack({ cwd = process.cwd() } = {}) {
  return {
    detect() {
      const pkgPath = resolveFrom(cwd, PKG_SPECIFIER);
      if (!pkgPath) return { matched: false };
      let version;
      try {
        version = createRequire(import.meta.url)(pkgPath)?.version;
      } catch {
        /* version unknown */
      }
      return {
        matched: true,
        name: "@modelcontextprotocol/sdk",
        version,
        confidence: version === PINNED_VERSION ? "pinned" : "best_effort",
      };
    },
    seams() {
      return [
        {
          patchPoint: "Client.prototype.request",
          upstreamApi: "@modelcontextprotocol/sdk/client/index.js — Client.request (Protocol request/response)",
          whyStable:
            "the JSON-RPC request/response correlation boundary shared by all client transports (stdio, streamable HTTP)",
        },
      ];
    },
    targets() {
      return [
        {
          pattern: "mcp:<server>",
          kind: "mcp",
          idempotencyRule:
            "MCP method calls are retryable (fresh JSON-RPC id per attempt); listing an mcp: server opts its calls into retry — retry = { attempts = 1 } to disable",
          argsHashRule: "none (MCP calls are not cached — potentially side-effecting)",
        },
      ];
    },
    // mcp: targets inherit [defaults.outbound]; no extra fragment of their own.
    defaults() {
      return {};
    },
  };
}

/**
 * Auto-detect the MCP client SDK and patch it (best-effort; never throws). Called
 * by the bootstrap. `clientModule` may be injected (tests); otherwise the SDK is
 * dynamically imported only when resolvable — an absent SDK is a silent no-op.
 */
export async function installMcpPack({ cwd = process.cwd(), clientModule } = {}) {
  try {
    const mod = clientModule ?? (await loadClientModule(cwd));
    if (!mod || typeof mod.Client !== "function") return { active: false };
    const uninstall = patchClientRequest(mod.Client);
    return { active: true, name: "@modelcontextprotocol/sdk", uninstall };
  } catch {
    return { active: false }; // detection/patch is best-effort, never fatal
  }
}

async function loadClientModule(cwd) {
  const resolved = resolveFrom(cwd, CLIENT_SPECIFIER);
  if (!resolved) return null;
  return import(pathToFileURL(resolved).href);
}
