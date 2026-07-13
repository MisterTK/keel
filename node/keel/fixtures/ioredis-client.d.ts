/**
 * PINNED FIXTURE — mirrors the `Redis.sendCommand` seam of **ioredis@5.x**.
 * The real `ioredis` package is NOT a dependency of Keel; this is the frozen
 * shape the `ioredis` pack patches (`Redis.prototype.sendCommand`) and is
 * contract-tested against. If ioredis bumps this boundary, update this
 * fixture + `PINNED_MAJOR` in `src/packs/ioredis.mjs` and re-certify
 * (adapter-pack contract: version-pinned tests).
 *
 * `test/ioredis.test.mjs` builds a plain JS fake conforming to this shape.
 */

export interface RedisOptions {
  host?: string;
  port?: number;
  /** Unix socket path — an alternative connection identifier to `host`. */
  path?: string;
}

/** The documented public `Command` shape (also relied on by APM/tracing
 *  instrumentation of ioredis): a command name, its arguments, and a promise
 *  that settles exactly once. A retried attempt cannot reuse this instance —
 *  the pack constructs `new command.constructor(command.name, command.args)`
 *  for each retry. */
export interface Command {
  name: string;
  args: (string | Buffer | number)[];
  promise: Promise<unknown>;
}

export interface Redis {
  /** Connection options this instance was constructed with — the pack derives
   *  its target from `options.host` (falling back to `options.path` for a
   *  Unix socket connection). */
  options: RedisOptions;

  /** Commander's single dispatch chokepoint: every generated command method
   *  (`get`, `set`, `mget`, ...) builds a `Command` and returns whatever this
   *  returns — normally `command.promise`. */
  sendCommand(command: Command, stream?: unknown): Promise<unknown>;
}
