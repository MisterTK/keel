/**
 * Deployment-shape checks the RUNTIME can make that doctor cannot (WS4, issue
 * #90): a durable-flow journal on storage that will not outlive the
 * instance. One stderr line, once per process, via `log.mjs`'s `emit`.
 *
 * The Python front end's `_deploy.py` is the byte-for-byte twin: same
 * marker precedence, same text template, same JSON field names.
 */

import { existsSync, accessSync, constants } from "node:fs";
import { createRequire } from "node:module";
import { join, resolve } from "node:path";

import { serverlessMarker } from "./packs/llm.mjs";

const DOCKERENV = "/.dockerenv";

/**
 * This package's own version, for the JSON log line's `version` field.
 * Fail-open, same rationale as `bootstrap.mjs`'s `readVersion`.
 */
function readVersion() {
  try {
    return createRequire(import.meta.url)("../package.json").version ?? "unknown";
  } catch {
    return "unknown";
  }
}

const VERSION = readVersion();

export function flowsConfigured(policy) {
  const flows = policy?.flows;
  if (typeof flows !== "object" || flows === null || Array.isArray(flows)) return false;
  const entrypoints = flows.entrypoints;
  const match = flows.match;
  const hasEntrypoints = Array.isArray(entrypoints) && entrypoints.length > 0;
  const hasMatch = typeof match === "object" && match !== null && Object.keys(match).length > 0;
  return hasEntrypoints || hasMatch;
}

/** The journal's on-disk path when it is SQLite, else null (Postgres). */
export function sqliteJournalPath(policy, cwd) {
  const loc = policy?.journal;
  if (loc === undefined || loc === null) {
    return join(cwd, ".keel", "journal.db");
  }
  if (typeof loc === "string" && loc.startsWith("file:")) {
    return resolve(cwd, loc.slice("file:".length));
  }
  return null;
}

export function ephemeralStorageMarker(env, cwd, { dockerenv = DOCKERENV } = {}) {
  const marker = serverlessMarker(env);
  if (marker !== null) return marker;
  if (existsSync(dockerenv)) return "/.dockerenv";
  try {
    accessSync(cwd, constants.W_OK);
  } catch {
    return "read-only cwd";
  }
  return null;
}

export function ephemeralJournalWarning(policy, env, cwd, { dockerenv = DOCKERENV } = {}) {
  if (!flowsConfigured(policy)) return null;
  const journal = sqliteJournalPath(policy, cwd);
  if (journal === null) return null;
  const marker = ephemeralStorageMarker(env, cwd, { dockerenv });
  if (marker === null) return null;
  const text =
    `keel ▸ warning: durable flows are configured but the journal is SQLite at ${journal} ` +
    `on ephemeral storage (${marker}) — flow state will not survive an instance replacement; ` +
    "mount a volume for .keel/ or use a Postgres journal\n";
  const obj = {
    keel: "warning",
    code: "journal-ephemeral-storage",
    journal: String(journal),
    marker,
    severity: "WARNING",
    version: VERSION,
  };
  return [text, obj];
}
