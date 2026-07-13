/**
 * A tiny, zero-dependency TOML parser covering exactly the keel.toml subset
 * (contracts/policy.schema.json): table headers with dotted + quoted segments,
 * scalar keys, strings, integers/floats, booleans, single-line arrays, and
 * single-line inline tables. It builds the nested object the backend expects.
 *
 * It intentionally does NOT implement full TOML (no multi-line strings/arrays,
 * no datetimes, no array-of-tables). It is a structural parser only: SEMANTIC
 * validation (durations, schedules, rates, unknown keys, types) is the
 * backend's job (KeelCoreStub.configure → KEEL-E001 with field paths), so the
 * two never diverge. Syntax errors here fail loudly with the line number.
 */

import { existsSync, readFileSync } from "node:fs";
import { join } from "node:path";
import { KeelError } from "./engine.mjs";
import { level0Defaults } from "./defaults.mjs";

function stripComment(line) {
  let inStr = false;
  let quote = "";
  for (let i = 0; i < line.length; i++) {
    const c = line[i];
    if (inStr) {
      if (c === "\\" && quote === '"') i++;
      else if (c === quote) inStr = false;
    } else if (c === '"' || c === "'") {
      inStr = true;
      quote = c;
    } else if (c === "#") {
      return line.slice(0, i);
    }
  }
  return line;
}

/** Read a bracket-free dotted key path, honoring quoted segments. */
function parseKeyPath(s, lineNo) {
  const parts = [];
  let i = 0;
  const n = s.length;
  while (i < n) {
    while (i < n && /\s/.test(s[i])) i++;
    if (i >= n) break;
    if (s[i] === '"' || s[i] === "'") {
      const [str, next] = readString(s, i, lineNo);
      parts.push(str);
      i = next;
    } else {
      let j = i;
      while (j < n && !/[\s.]/.test(s[j])) j++;
      const bare = s.slice(i, j);
      if (!bare) throw new KeelError("KEEL-E001", `keel.toml line ${lineNo}: empty key segment`);
      parts.push(bare);
      i = j;
    }
    while (i < n && /\s/.test(s[i])) i++;
    if (i < n) {
      if (s[i] !== ".")
        throw new KeelError("KEEL-E001", `keel.toml line ${lineNo}: expected '.' in key path`);
      i++;
    }
  }
  if (parts.length === 0)
    throw new KeelError("KEEL-E001", `keel.toml line ${lineNo}: empty key path`);
  return parts;
}

function readString(s, i, lineNo) {
  const quote = s[i];
  let out = "";
  let j = i + 1;
  while (j < s.length) {
    const c = s[j];
    if (quote === '"' && c === "\\") {
      const e = s[j + 1];
      out += e === "n" ? "\n" : e === "t" ? "\t" : e === "r" ? "\r" : e;
      j += 2;
      continue;
    }
    if (c === quote) return [out, j + 1];
    out += c;
    j++;
  }
  throw new KeelError("KEEL-E001", `keel.toml line ${lineNo}: unterminated string`);
}

/** Parse a value starting at index i; returns [value, nextIndex]. */
function readValue(s, i, lineNo) {
  while (i < s.length && /\s/.test(s[i])) i++;
  if (i >= s.length) throw new KeelError("KEEL-E001", `keel.toml line ${lineNo}: missing value`);
  const c = s[i];
  if (c === '"' || c === "'") return readString(s, i, lineNo);
  if (c === "[") return readArray(s, i, lineNo);
  if (c === "{") return readInlineTable(s, i, lineNo);
  // bare scalar: read until , ] } or end
  let j = i;
  while (j < s.length && !",]}".includes(s[j])) j++;
  const raw = s.slice(i, j).trim();
  return [scalar(raw, lineNo), j];
}

function scalar(raw, lineNo) {
  if (raw === "true") return true;
  if (raw === "false") return false;
  if (/^[+-]?\d+$/.test(raw)) return parseInt(raw, 10);
  if (/^[+-]?(\d+\.\d+|\d+)([eE][+-]?\d+)?$/.test(raw)) return Number(raw);
  throw new KeelError("KEEL-E001", `keel.toml line ${lineNo}: unparseable value \`${raw}\``);
}

function readArray(s, i, lineNo) {
  const arr = [];
  i++; // skip [
  while (i < s.length) {
    while (i < s.length && /\s/.test(s[i])) i++;
    if (s[i] === "]") return [arr, i + 1];
    const [v, next] = readValue(s, i, lineNo);
    arr.push(v);
    i = next;
    while (i < s.length && /\s/.test(s[i])) i++;
    if (s[i] === ",") i++;
    else if (s[i] === "]") return [arr, i + 1];
    else if (i < s.length)
      throw new KeelError("KEEL-E001", `keel.toml line ${lineNo}: expected ',' or ']' in array`);
  }
  throw new KeelError("KEEL-E001", `keel.toml line ${lineNo}: unterminated array`);
}

function readInlineTable(s, i, lineNo) {
  const obj = {};
  i++; // skip {
  while (i < s.length) {
    while (i < s.length && /\s/.test(s[i])) i++;
    if (s[i] === "}") return [obj, i + 1];
    // key
    let key;
    if (s[i] === '"' || s[i] === "'") {
      [key, i] = readString(s, i, lineNo);
    } else {
      let j = i;
      while (j < s.length && !/[\s=]/.test(s[j])) j++;
      key = s.slice(i, j);
      i = j;
    }
    while (i < s.length && /\s/.test(s[i])) i++;
    if (s[i] !== "=")
      throw new KeelError("KEEL-E001", `keel.toml line ${lineNo}: expected '=' in inline table`);
    i++;
    const [v, next] = readValue(s, i, lineNo);
    obj[key] = v;
    i = next;
    while (i < s.length && /\s/.test(s[i])) i++;
    if (s[i] === ",") i++;
    else if (s[i] === "}") return [obj, i + 1];
    else if (i < s.length)
      throw new KeelError(
        "KEEL-E001",
        `keel.toml line ${lineNo}: expected ',' or '}' in inline table`
      );
  }
  throw new KeelError("KEEL-E001", `keel.toml line ${lineNo}: unterminated inline table`);
}

function setPath(root, path, value, lineNo) {
  let cur = root;
  for (let k = 0; k < path.length - 1; k++) {
    const seg = path[k];
    if (cur[seg] === undefined) cur[seg] = {};
    else if (cur[seg] === null || typeof cur[seg] !== "object" || Array.isArray(cur[seg]))
      throw new KeelError("KEEL-E001", `keel.toml line ${lineNo}: key path collides with a value`);
    cur = cur[seg];
  }
  cur[path[path.length - 1]] = value;
}

export function parseToml(text) {
  const root = {};
  let table = root;
  const lines = text.split(/\r?\n/);
  for (let idx = 0; idx < lines.length; idx++) {
    const lineNo = idx + 1;
    const line = stripComment(lines[idx]).trim();
    if (!line) continue;
    if (line.startsWith("[")) {
      if (!line.endsWith("]"))
        throw new KeelError("KEEL-E001", `keel.toml line ${lineNo}: unterminated table header`);
      if (line.startsWith("[["))
        throw new KeelError(
          "KEEL-E001",
          `keel.toml line ${lineNo}: array-of-tables is not supported`
        );
      const path = parseKeyPath(line.slice(1, -1).trim(), lineNo);
      table = root;
      for (const seg of path) {
        if (table[seg] === undefined) table[seg] = {};
        table = table[seg];
      }
      continue;
    }
    const eq = firstTopLevelEquals(line);
    if (eq < 0)
      throw new KeelError("KEEL-E001", `keel.toml line ${lineNo}: expected 'key = value'`);
    const keyPath = parseKeyPath(line.slice(0, eq).trim(), lineNo);
    const [value, end] = readValue(line, eq + 1, lineNo);
    const rest = line.slice(end).trim();
    if (rest)
      throw new KeelError(
        "KEEL-E001",
        `keel.toml line ${lineNo}: trailing characters after value \`${rest}\``
      );
    setPath(table, keyPath, value, lineNo);
  }
  return root;
}

function firstTopLevelEquals(line) {
  let inStr = false;
  let quote = "";
  for (let i = 0; i < line.length; i++) {
    const c = line[i];
    if (inStr) {
      if (c === "\\" && quote === '"') i++;
      else if (c === quote) inStr = false;
    } else if (c === '"' || c === "'") {
      inStr = true;
      quote = c;
    } else if (c === "=") return i;
  }
  return -1;
}

/** Load keel.toml from `cwd`, or the Level 0 embedded pack if absent. */
export function loadPolicy(cwd = process.cwd()) {
  const path = join(cwd, "keel.toml");
  if (!existsSync(path)) return { policy: level0Defaults(), source: "defaults" };
  let text;
  try {
    text = readFileSync(path, "utf8");
  } catch (e) {
    // A present policy file that cannot be read is a loud failure, never a
    // silent fall-back to defaults (that would be a surprise, and surprise is
    // a P0 in Level 0).
    throw new KeelError(
      "KEEL-E001",
      `keel.toml is present but could not be read: ${e.message}. Fix the file's permissions/path, or remove it to fall back to Level 0 defaults.`
    );
  }
  const policy = parseToml(text); // throws KeelError E001 with line number on bad syntax
  return { policy, source: "keel.toml" };
}

/**
 * Function-target globs declared in policy. v0.1 rule (documented in README):
 * a `ts:<pathGlob>#<exportName>` key wraps the named function export of any
 * module whose path (relative to cwd) or basename matches `<pathGlob>`.
 */
export function extractFunctionTargets(policy) {
  const out = [];
  const targets = policy?.target;
  if (targets === null || typeof targets !== "object") return out;
  for (const key of Object.keys(targets)) {
    if (!key.startsWith("ts:")) continue;
    const body = key.slice(3);
    const hash = body.indexOf("#");
    if (hash < 0) {
      out.push({ target: key, glob: body, fn: null, skipped: "no #exportName; cannot wrap" });
      continue;
    }
    out.push({ target: key, glob: body.slice(0, hash), fn: body.slice(hash + 1) });
  }
  return out;
}

/**
 * Tier 2 `[flows] entrypoints` declared in policy (architecture-spec §4.1's
 * example: `flows = ["py:pipeline.ingest:main", "ts:jobs/nightly.ts#run"]`).
 * Mirrors `python/keel/src/keel/_policy.py`'s `extract_flow_entrypoints`,
 * adapted to Node's file-path identity space: the Node front end owns only
 * `ts:` entries (the frozen `entrypointRef` grammar `^(py|ts|rs):[^\s]+$`
 * covers `py`/`rs` too, but those belong to other front ends and are skipped
 * here — `ts:` is the SAME prefix `extractFunctionTargets` uses for every
 * `.mjs`/`.js`/`.ts` module, per that function's own convention).
 *
 * Grammar: `ts:<pathGlob>#<exportName>` — the SAME `module#export` shape as a
 * `[target."ts:…"]` function target, so one glob dialect (`loader.mjs`'s
 * `globToRegExp`) covers both. `<exportName>` must be concrete (no `*`): the
 * flow body must be a specific, named export. Malformed or non-`ts:` entries
 * are skipped, not guessed — designating a flow is an explicit assertion.
 */
export function extractFlowEntrypoints(policy) {
  const flows = policy?.flows;
  if (flows === null || typeof flows !== "object") return [];
  const entrypoints = flows.entrypoints;
  if (!Array.isArray(entrypoints)) return [];
  const out = [];
  for (const raw of entrypoints) {
    if (typeof raw !== "string" || !raw.startsWith("ts:")) continue;
    const body = raw.slice(3);
    const hash = body.indexOf("#");
    if (hash < 0) continue; // needs #exportName; a bare module is never guessed
    const glob = body.slice(0, hash);
    const fn = body.slice(hash + 1);
    if (!glob || !fn || fn.includes("*")) continue; // the flow body must be concrete
    out.push({ raw, glob, fn });
  }
  return out;
}
