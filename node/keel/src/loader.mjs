/**
 * ESM loader hooks (run off-thread via module.register) that wrap `ts:` function
 * targets. Registered by the bootstrap with the resolved function targets, the
 * project cwd, and the absolute URL of loader-runtime.mjs.
 *
 * The `load` hook rewrites matching modules' named function exports so they
 * route through the backend on the MAIN thread. v0.1 supports exactly the
 * canonical named-export function forms — a documented simplification
 * (README §Function targets):
 *
 *     export function NAME(...) {...}
 *     export async function NAME(...) {...}
 *
 * Rewrite (byte-identical body, only the declaration head changes + appended
 * export):
 *     [async ]function __keel$NAME(...) {...}
 *     export const NAME = __keel$wrap("ts:glob#NAME", __keel$NAME);
 *
 * NOT supported in v0.1 (left untouched, so behavior is unchanged): arrow/const
 * exports, `export { NAME }` lists, `export default`, class methods. The wrapper
 * is a live-binding `const`, so a call to the export DURING the defining
 * module's own top-level evaluation would hit the temporal dead zone; targets
 * are meant to be entry functions called by importers, not during self-eval.
 *
 * The same hook also drives the eve pack's `tool:<name>` rewrite (packs/eve.mjs
 * `transformEveTool`) — a second, independent source-text transform for eve's
 * `export default defineTool({...})` tool-module convention, armed whenever
 * eve is detected at bootstrap (`data.eveEnabled`), regardless of whether any
 * `ts:` targets are configured.
 */

import { transformEveTool, toolTargetFromPath } from "./packs/eve.mjs";

let FUNCTION_TARGETS = [];
let CWD = process.cwd();
let RUNTIME_URL = "";
let EVE_ENABLED = false;

export async function initialize(data) {
  FUNCTION_TARGETS = data?.functionTargets?.filter((t) => t.fn) ?? [];
  CWD = data?.cwd ?? process.cwd();
  RUNTIME_URL = data?.runtimeUrl ?? "";
  EVE_ENABLED = Boolean(data?.eveEnabled);
}

export async function load(url, context, nextLoad) {
  const result = await nextLoad(url, context);
  if (result.format !== "module" || result.source == null) return result;
  const source = toStringSource(result.source);

  if (FUNCTION_TARGETS.length > 0) {
    const names = matchingExportNames(url);
    if (names.size > 0) {
      const { code, wrapped } = transform(source, names, RUNTIME_URL);
      if (wrapped.length > 0) return { ...result, source: code };
    }
  }

  if (EVE_ENABLED) {
    const rewritten = transformEveTool(source, toolTargetFromPath(urlToPath(url)), RUNTIME_URL);
    if (rewritten != null) return { ...result, source: rewritten };
  }

  return result;
}

/** `file:` URL → filesystem path; any other URL scheme passes through as-is. */
function urlToPath(url) {
  try {
    return url.startsWith("file:") ? new URL(url).pathname : url;
  } catch {
    return url;
  }
}

function toStringSource(source) {
  if (typeof source === "string") return source;
  return Buffer.isBuffer(source) ? source.toString("utf8") : new TextDecoder().decode(source);
}

function matchingExportNames(url) {
  const names = new Map(); // exportName -> target string
  let rel;
  let base;
  try {
    const path = url.startsWith("file:") ? new URL(url).pathname : url;
    rel = relativePosix(CWD, path);
    base = path.slice(path.lastIndexOf("/") + 1);
  } catch {
    return names;
  }
  for (const t of FUNCTION_TARGETS) {
    const re = globToRegExp(t.glob);
    if (re.test(rel) || re.test(base)) names.set(t.fn, t.target);
  }
  return names;
}

function relativePosix(fromDir, toPath) {
  const a = fromDir.replace(/\\/g, "/").replace(/\/+$/, "").split("/");
  const b = toPath.replace(/\\/g, "/").split("/");
  let i = 0;
  while (i < a.length && i < b.length && a[i] === b[i]) i++;
  const up = a.slice(i).map(() => "..");
  return [...up, ...b.slice(i)].join("/");
}

function globToRegExp(glob) {
  let re = "^";
  for (let i = 0; i < glob.length; i++) {
    const c = glob[i];
    if (c === "*") {
      if (glob[i + 1] === "*") {
        re += ".*";
        i++;
      } else re += "[^/]*";
    } else if (c === "?") re += ".";
    else re += c.replace(/[.+^${}()|[\]\\]/g, "\\$&");
  }
  return new RegExp(re + "$");
}

function transform(source, names, runtimeUrl) {
  const wrapped = [];
  let out = source;
  for (const [name, target] of names) {
    const decl = new RegExp(
      `(^|\\n)([ \\t]*)export[ \\t]+(async[ \\t]+)?function[ \\t]+${escapeId(name)}\\b`
    );
    if (!decl.test(out)) continue; // unsupported export form — leave untouched
    out = out.replace(decl, (_m, p1, p2, p3) => `${p1}${p2}${p3 ?? ""}function __keel$${name}`);
    out += `\nexport const ${name} = __keel$wrap(${JSON.stringify(target)}, __keel$${name});\n`;
    wrapped.push(name);
  }
  if (wrapped.length > 0)
    out = `import { wrapExport as __keel$wrap } from ${JSON.stringify(runtimeUrl)};\n` + out;
  return { code: out, wrapped };
}

function escapeId(name) {
  return name.replace(/[.*+?^${}()|[\]\\]/g, "\\$&");
}
