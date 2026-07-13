/**
 * `tool:` semantic target pack + the wrap API framework packs build on
 * (DX spec §4.1: "tool:<name> — agent tool invocations, wrapped at the
 * framework's own boundary so retries happen *below* the LLM loop (a failed
 * tool call is retried without burning tokens on a new LLM turn)").
 *
 * The Node twin of `python/keel/src/keel/packs/tool.py` — the two front ends
 * must make the same judgments for the same wrap declaration:
 *
 *   - target:      `tool:<name>`, validated against the frozen targetKey
 *                  grammar (contracts/policy.schema.json). Exact-match keys
 *                  only — the grammar has no glob form for tool: targets.
 *   - defaults:    contracts/defaults.toml defines NO [defaults.tool], so the
 *                  pack ships no fragment of its own (`defaults() == {}`,
 *                  mirroring the mcp pack) and a tool: target inherits the
 *                  generic profile via the backend's resolution: exact
 *                  [target."tool:<name>"] first, then [defaults.outbound].
 *                  Under the default non-idempotent judgment that inherited
 *                  profile is effectively the generic non-idempotent one: the
 *                  outbound retry layer is inert (observed, not retried —
 *                  KEEL-E014), nothing is cached, and the breaker still
 *                  protects the target (repeated failures fail fast, E012).
 *   - idempotency: a tool runs arbitrary side-effecting code, so a tool call
 *                  is NEVER auto-retried (Level 0 hard rule, dx-spec §1).
 *                  `wrapTool(name, fn, { idempotent: true })` is the opt-in,
 *                  made AT THE WRAP SITE by the framework pack / tool author —
 *                  the safety judgment lives in the adapter
 *                  (contracts/adapter-pack.md). There is deliberately no
 *                  keel.toml knob to flip it in v0.1: the frozen schema has
 *                  none and we do not invent policy surface (the same v0.1
 *                  decision as the mcp pack's tools/call rule).
 *   - args_hash:   sha256 over the JSON-stringified args for a
 *                  declared-idempotent tool (cache-key material; caching still
 *                  needs an explicit [target] cache ttl); null otherwise — a
 *                  side-effecting tool is never served from cache, even under
 *                  a misconfigured [target] cache layer.
 *   - error class: TimeoutError → `timeout`; AbortError → `cancelled` (caller
 *                  cancellation, immediately terminal, KEEL-E015); the mcp
 *                  pack's conn error-code set → `conn`; anything else →
 *                  `other`, which is NOT in the default retry.on — a plain
 *                  tool bug propagates unchanged unless the target's policy
 *                  adds "other".
 *
 * Timeouts: the per-attempt wall-clock timeout is armed by the CORE, only for
 * idempotent requests and only where the effect actually awaits (the native
 * async path). The wrapper injects no deadline of its own — same contract as
 * the ts: function wrapper (loader-runtime.mjs).
 *
 * Live objects stay side-band exactly as loader-runtime.mjs documents: the
 * live return value is delivered on the success path (identity preserved),
 * the ORIGINAL error re-throws on terminal failure (DX invariant 5), and only
 * a cache HIT returns the round-tripped JSON payload.
 */

import { createHash } from "node:crypto";
import { getBackend, getDiscovery, attachOutcome } from "../runtime.mjs";
import { KeelError } from "../engine.mjs";

// The <name> part of the frozen targetKey grammar for semantic targets
// (contracts/policy.schema.json: `^(llm|tool|mcp):[A-Za-z0-9_][A-Za-z0-9_.-]*$`).
const TOOL_NAME_RE = /^[A-Za-z0-9_][A-Za-z0-9_.-]*$/;

/** True iff `name` can appear as `tool:<name>` in keel.toml. Packs that
 *  auto-wrap every framework-registered tool should check-and-skip (noting the
 *  skip for doctor) rather than let `wrapTool` throw mid-bootstrap — "if a
 *  call site cannot be wrapped safely, do nothing and note it". */
export function isValidToolName(name) {
  return typeof name === "string" && TOOL_NAME_RE.test(name);
}

/** The policy target key for a tool name, validated against the frozen target
 *  grammar. An invalid name could never be a keel.toml key, so wrapping it
 *  would create an unroutable target — config-shaped misuse, KEEL-E001. */
export function toolTarget(name) {
  if (!isValidToolName(name)) {
    throw new KeelError(
      "KEEL-E001",
      `invalid tool name ${JSON.stringify(name)}: a tool: target must match ` +
        "[A-Za-z0-9_][A-Za-z0-9_.-]* (contracts/policy.schema.json targetKey); " +
        "rename the tool or skip wrapping it"
    );
  }
  return `tool:${name}`;
}

/**
 * Classify a thrown tool error into a core error class. TimeoutError →
 * `timeout` and the conn error-code set → `conn` are both in the default
 * retry.on, so a declared-idempotent tool retries transient infrastructure
 * failures out of the box (the tool: analogue of the mcp pack's classifier).
 * A caller's AbortController fires `AbortError` → `cancelled`: immediately
 * terminal, so an aborted tool call propagates at once (parity with the
 * Python twin, whose CancelledError escapes the wrapper as a BaseException).
 */
export function classifyToolError(err) {
  const name = err?.name;
  if (name === "TimeoutError") return "timeout";
  if (name === "AbortError") return "cancelled";
  const code = err?.code;
  if (typeof code === "string" && /^(ECONNREFUSED|ECONNRESET|EPIPE|ENOTFOUND)$/.test(code))
    return "conn";
  return "other";
}

// Cache-key material only (mirrors loader-runtime.mjs hashArgs); null disables
// caching for the call.
function hashArgs(args) {
  try {
    return createHash("sha256").update(JSON.stringify(args)).digest("hex");
  } catch {
    return null;
  }
}

// A JSON-safe view for the core payload (mirrors loader-runtime.mjs jsonSafe):
// the live value is delivered side-band, so this only gates the cache STORE.
function jsonSafe(v) {
  try {
    JSON.stringify(v);
    return v;
  } catch {
    return null;
  }
}

/**
 * Wrap a tool callable as the `tool:<name>` policy target — the small API
 * framework packs build on. Each call routes through the backend's `execute`
 * under `tool:<name>` (policy: exact target, then [defaults.outbound]) and is
 * recorded in discovery. The wrapper is async (`fn` may be sync or async).
 *
 * `idempotent` defaults to false: a tool call is observed, never retried
 * (KEEL-E014 on a would-be-retryable error). Pass `idempotent: true` ONLY for
 * a tool that is safe to re-invoke (a read); that declaration is the wrap
 * site's assertion, exactly as listing a `ts:` target is the user's.
 * `deps.backend` / `deps.discovery` override the globals (tests/embedding),
 * mirroring the mcp pack's `makeWrappedRequest`.
 */
export function wrapTool(name, fn, { idempotent = false, backend: depBackend, discovery: depDiscovery } = {}) {
  const target = toolTarget(name);
  if (typeof fn !== "function") {
    throw new KeelError("KEEL-E001", `wrapTool(${JSON.stringify(name)}): fn must be a function`);
  }
  const op = `tool ${name} ${fn.name || "?"}`;
  const wrapped = async function (...args) {
    const backend = depBackend ?? getBackend();
    if (!backend) return fn.apply(this, args); // disabled / uninstalled: transparent
    const request = {
      v: 1,
      target,
      op,
      idempotent,
      // A side-effecting tool must never be served from cache, even under a
      // misconfigured [target] cache — args_hash null disables it wholesale.
      args_hash: idempotent ? hashArgs(args) : null,
    };
    const self = this;
    const started = performance.now();
    // Live result / error held side-band so the core payload stays JSON (the
    // native core cannot round-trip a live object; see module docs).
    let liveResult;
    let haveResult = false;
    let liveErr;
    const outcome = await backend.execute(request, async () => {
      try {
        liveResult = await fn.apply(self, args);
        haveResult = true;
        liveErr = undefined;
        return { status: "ok", payload: jsonSafe(liveResult) };
      } catch (err) {
        liveErr = err;
        return { status: "error", class: classifyToolError(err), message: err?.message ?? String(err) };
      }
    });
    (depDiscovery ?? getDiscovery())?.observe(target, outcome, performance.now() - started);
    if (outcome.result === "ok") {
      // Live call → the real return value, unchanged (attachOutcome is
      // non-enumerable, mcp-pack precedent); cache hit → the replayed (JSON)
      // payload — no live call to return.
      return attachOutcome(haveResult && !outcome.from_cache ? liveResult : outcome.payload, outcome);
    }
    if (liveErr instanceof Error) throw attachOutcome(liveErr, outcome);
    if (liveErr !== undefined) throw liveErr;
    // No side-band original (e.g. a breaker fast-fail, KEEL-E012): surface the
    // core's own error, still carrying the outcome.
    const e = new KeelError(outcome.error?.code ?? "KEEL-E040", outcome.error?.message ?? `keel: tool ${name} failed`);
    throw attachOutcome(e, outcome);
  };
  Object.defineProperty(wrapped, "name", { value: fn.name || `tool:${name}`, configurable: true });
  wrapped.__keelWrapped = true;
  wrapped.__keelTarget = target;
  return wrapped;
}

/** The `tool:` adapter pack — the four uniform operations (adapter-pack.md).
 *  A semantic pack like `llm`: it always "matches" (no external library
 *  version to pin → confidence `pinned`) and owns no seam — targets are
 *  produced by `wrapTool` at each framework pack's own tool-execution
 *  boundary. */
export const toolPack = Object.freeze({
  detect() {
    return { matched: true, name: "tool", confidence: "pinned" };
  },
  seams() {
    // No seam of its own: the tool-execution seam belongs to the framework
    // pack that calls wrapTool (it declares the patch point + stability).
    return [];
  },
  targets() {
    return [
      {
        pattern: "tool:<name>",
        kind: "tool",
        idempotencyRule:
          "a tool call is non-idempotent by default — observed, not retried " +
          "(KEEL-E014) — unless the wrapping pack declares idempotent: true at " +
          "the wrap site (the safety judgment lives in the adapter); no " +
          "keel.toml knob flips it in v0.1",
        argsHashRule:
          "sha256 over the JSON-stringified call args for a declared-idempotent " +
          "tool (cache-key material; caching still needs an explicit [target] " +
          "cache ttl); null otherwise — a side-effecting tool is never served " +
          "from cache",
      },
    ];
  },
  // Empty: contracts/defaults.toml defines no [defaults.tool], so tool:
  // targets inherit [defaults.outbound] via the backend's target resolution
  // (mirrors the mcp pack).
  defaults() {
    return {};
  },
});
