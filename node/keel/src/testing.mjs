/**
 * Offline replay for a `keel record` capture — the reusable core that `keel
 * record test`-generated test files import (rather than duplicating
 * matching logic per generated file). See `docs/recording-format.md` /
 * `python/keel/src/keel/testing.py`'s module docstring for the canonical
 * request-matching rule this module implements (both front ends share it
 * exactly):
 *
 *   1. `target` must match exactly.
 *   2. If the live call's `args_hash` is not `null`, it must equal a
 *      recorded call's `args_hash` exactly (byte-for-byte).
 *   3. Otherwise (`args_hash` is `null` on both sides — a non-idempotent
 *      call, which never gets an `args_hash`), the `op` strings must match
 *      instead.
 *   4. Among the recorded calls satisfying 1-3, the FIRST one not yet
 *      consumed is served (recordings are consumed in the order they were
 *      made, so a repeated call to the identical target replays its
 *      recorded repeats in order).
 *   5. No candidate remains → `UnmatchedEffectError`, naming
 *      target/op/args_hash. Replay NEVER silently passes an unrecorded call
 *      through live.
 */

import { readFileSync } from "node:fs";
import { installFetch } from "./fetch.mjs";
import { AsyncEngine } from "./engine.mjs";
import { getBackend, getDiscovery, setRuntime } from "./runtime.mjs";

export class UnmatchedEffectError extends Error {}

function matchKey(target, op, argsHash) {
  return argsHash !== null && argsHash !== undefined ? `${target} h:${argsHash}` : `${target} o:${op}`;
}

/** A parsed `.ndjson` recording: the `meta` header plus its `call` lines, in
 * recorded order. Malformed/foreign lines are skipped; only a missing or
 * non-`meta` header line is fatal (it means this isn't a Keel recording). */
export class Recording {
  constructor(meta, calls) {
    this.meta = meta;
    this.calls = calls;
  }
  static load(path) {
    let text;
    try {
      text = readFileSync(path, "utf8");
    } catch (err) {
      throw new Error(`keel record test: cannot read ${path}: ${err.message ?? err}`);
    }
    const lines = text.split("\n").filter((l) => l.trim().length > 0);
    if (lines.length === 0) {
      throw new Error(`keel record test: ${path} is empty — nothing was recorded`);
    }
    const meta = JSON.parse(lines[0]);
    if (!meta || meta.type !== "meta") {
      throw new Error(`keel record test: ${path} has no meta header — not a Keel recording`);
    }
    const calls = lines
      .slice(1)
      .map((l) => JSON.parse(l))
      .filter((o) => o && o.type === "call");
    return new Recording(meta, calls);
  }
}

/**
 * A `Backend` (see `backend.mjs`) that serves `execute` calls from a
 * `Recording` instead of running real effects. `configure`/`report` are
 * no-ops. `resolver` is the real backend that was active immediately before
 * `installReplay` swapped this one in (see there): `layer`/`resolveTarget`
 * delegate to it when present, since a from-scratch replay has no compiled
 * policy of its own to answer either from (Python parity — see
 * `python/keel/src/keel/testing.py`'s `ReplayBackend`; issue #51 closed the
 * fidelity gap this left between the two languages). With no resolver (e.g.
 * a bare `new ReplayBackend(rec)` built directly, no policy in scope) —
 * `layer` is a harmless no-op (`fetch.mjs` calls `backend.layer(...)`
 * unconditionally, so this must exist even though replay never consults
 * policy) and `resolveTarget` falls back to a bare, never-`configure`d
 * `AsyncEngine`, which reduces to exactly the LLM host map / Vertex regional
 * suffix rule (an empty policy has no `[target]` patterns) — unchanged from
 * before, purely for backward compatibility with direct construction; it is
 * NOT a claim that this reproduces real target resolution. The caller's
 * real effect is NEVER invoked — a match is served purely from the
 * recording, and a miss throws `UnmatchedEffectError` rather than falling
 * through to it.
 */
export class ReplayBackend {
  #queues = new Map();
  #engine = new AsyncEngine();
  #resolver;
  constructor(recording, resolver = null) {
    this.#resolver = resolver;
    for (const call of recording.calls) {
      const key = matchKey(String(call.target ?? ""), String(call.op ?? ""), call.args_hash ?? null);
      if (!this.#queues.has(key)) this.#queues.set(key, []);
      this.#queues.get(key).push(call);
    }
  }
  configure() {}
  layer(target, key) {
    if (this.#resolver) return this.#resolver.layer(target, key);
    return undefined;
  }
  resolveTarget(method, host, scheme, port, path) {
    if (this.#resolver) return this.#resolver.resolveTarget(method, host, scheme, port, path);
    return this.#engine.resolveTarget(method, host, scheme, port, path);
  }
  get persistent() {
    return false;
  }
  // eslint-disable-next-line require-await -- Backend.execute is async by contract
  async execute(request, _effect) {
    const target = String(request?.target ?? "");
    const op = String(request?.op ?? "");
    const argsHash = request?.args_hash ?? null;
    const queue = this.#queues.get(matchKey(target, op, argsHash));
    if (!queue || queue.length === 0) {
      throw new UnmatchedEffectError(
        `keel record test: no recorded call matches target=${JSON.stringify(target)} ` +
          `op=${JSON.stringify(op)} args_hash=${JSON.stringify(argsHash)} — re-record, or check ` +
          "the code under test for an unrecorded/novel effect"
      );
    }
    const outcome = queue.shift().outcome;
    // A replayed "ok" is served with no live call at all, so it must look
    // like a cache hit to fetch.mjs (which returns the LIVE response object
    // when `from_cache` is falsy — there is none here — and only rebuilds
    // one from `payload` when `from_cache` is true). A recording made from a
    // real, non-cached call has `from_cache: false`; flip it here rather
    // than at capture time, so the recording still reads as "what really
    // happened" on disk.
    if (outcome && typeof outcome === "object" && outcome.result === "ok" && !outcome.from_cache) {
      return { ...outcome, from_cache: true };
    }
    return outcome;
  }
  report() {
    return {};
  }
}

/**
 * Install a `ReplayBackend` for `path`: rewires the global `fetch` seam
 * (`installFetch` captures its backend by closure at install time — see
 * `docs/recording-format.md`'s "Known limitations") and the dynamic runtime
 * (`ts:` function targets). Returns an `uninstall` callable that restores
 * both — call it (or use `withReplay`, which does this for you) when the
 * replay scope ends.
 *
 * Intended usage is a bare `node:test` run that never went through `keel
 * run`/`--import keel/hook` — `installFetch` is idempotent and no-ops on an
 * already-wrapped `fetch`, so calling this a SECOND time inside an
 * already-bootstrapped Keel process will rewire the dynamic runtime but
 * leave the live `fetch` wrapper pointed at the ORIGINAL backend. Generated
 * test glue (`keel record test`) targets the bare-`node:test` case only.
 */
export function installReplay(path) {
  const previousBackend = getBackend();
  const previousDiscovery = getDiscovery();
  // `resolver: previousBackend` — layer/resolveTarget delegate to whatever
  // backend was active before this swap (see ReplayBackend's docstring for
  // why this is required, not optional: issue #51).
  const backend = new ReplayBackend(Recording.load(path), previousBackend);
  const uninstallFetch = installFetch(backend, previousDiscovery ?? null, {});
  setRuntime({ enabled: true, backend, discovery: previousDiscovery });
  return function uninstall() {
    if (typeof uninstallFetch === "function") uninstallFetch();
    setRuntime({ enabled: previousBackend != null, backend: previousBackend, discovery: previousDiscovery });
  };
}

/** Run `fn` with a `ReplayBackend` for `path` installed, restoring the
 * previous runtime afterward (even if `fn` throws) — the `node:test` twin of
 * Python's `replay_fixture`. */
export async function withReplay(path, fn) {
  const uninstall = installReplay(path);
  try {
    return await fn();
  } finally {
    uninstall();
  }
}
