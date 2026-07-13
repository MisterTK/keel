// The eve pack: the adapter-pack four-op contract shape, and the pure
// `transformEveTool` source rewrite (unit-testable without a real `eve`
// install or the ESM loader machinery). Full wiring — the loader actually
// rewriting a fixture tool module and the runtime enforcing
// non-idempotent-by-default — is covered end-to-end in eve.e2e.test.mjs (a
// child process, like loader.e2e.test.mjs, since it exercises live
// module.register + global runtime state).

import test from "node:test";
import assert from "node:assert/strict";
import { evePack, transformEveTool, toolTargetFromPath } from "../src/packs/eve.mjs";

test("evePack implements the adapter-pack four operations", () => {
  const p = evePack({ cwd: "/nonexistent-project" });
  assert.deepEqual(p.detect(), { matched: false }); // eve absent in this repo
  const seams = p.seams();
  assert.equal(seams.length, 1);
  assert.match(seams[0].patchPoint, /defineTool/);
  assert.match(seams[0].upstreamApi, /eve\/tools/);
  assert.match(seams[0].whyStable, /reversible/);
  const targets = p.targets();
  assert.equal(targets.length, 1);
  assert.equal(targets[0].pattern, "tool:<name>");
  assert.equal(targets[0].kind, "tool");
  assert.match(targets[0].idempotencyRule, /non-idempotent by default/);
  assert.match(targets[0].idempotencyRule, /KEEL-E014/);
  assert.equal(targets[0].argsHashRule, "none (tool calls can be side-effecting; never dev-cached)");
  assert.deepEqual(p.defaults(), {}); // tool: inherits [defaults.outbound], no fragment of its own
});

test("toolTargetFromPath derives tool:<name> from the tool file's basename", () => {
  assert.equal(toolTargetFromPath("/proj/agent/tools/get_weather.ts"), "tool:get_weather");
  assert.equal(toolTargetFromPath("/proj/agent/tools/send_email.mjs"), "tool:send_email");
  assert.equal(toolTargetFromPath("charge.tsx"), "tool:charge");
  assert.equal(toolTargetFromPath("/a/b/lookup.cjs"), "tool:lookup");
});

test("transformEveTool rewrites the canonical `import { defineTool } from \"eve/tools\"` form", () => {
  const source = [
    'import { defineTool } from "eve/tools";',
    "import { z } from \"zod\";",
    "",
    "export default defineTool({",
    '  description: "d",',
    "  inputSchema: z.object({}),",
    "  async execute(args) { return args; },",
    "});",
    "",
  ].join("\n");
  const out = transformEveTool(source, "tool:get_weather", "file:///abs/loader-runtime.mjs");
  assert.notEqual(out, null);
  assert.match(out, /import \{ defineTool as __keel\$realDefineTool \} from "eve\/tools";/);
  assert.match(
    out,
    /import \{ wrapEveTool as __keel\$wrapEveTool \} from "file:\/\/\/abs\/loader-runtime\.mjs";/
  );
  assert.match(
    out,
    /const defineTool = \(def\) => __keel\$wrapEveTool\("tool:get_weather", __keel\$realDefineTool, def\);/
  );
  // everything else in the module is byte-identical (untouched call sites,
  // untouched unrelated import).
  assert.match(out, /import \{ z \} from "zod";/);
  assert.match(out, /export default defineTool\(\{/);
});

test("transformEveTool preserves indentation of the original import line", () => {
  const source = '  import { defineTool } from "eve/tools";\nexport default defineTool({});\n';
  const out = transformEveTool(source, "tool:x", "file:///rt.mjs");
  assert.match(out, /^ {2}import \{ defineTool as __keel\$realDefineTool \}/m);
});

test("transformEveTool leaves non-canonical forms untouched (returns null)", () => {
  const forms = [
    // aliased import
    'import { defineTool as dt } from "eve/tools";\nexport default dt({});\n',
    // namespace import
    'import * as eveTools from "eve/tools";\nexport default eveTools.defineTool({});\n',
    // combined with another named import on the same line
    'import { defineTool, something } from "eve/tools";\n',
    // no eve import at all
    'export default { description: "d", async execute() {} };\n',
    // a different package entirely
    'import { defineTool } from "some-other-package";\n',
  ];
  for (const source of forms) {
    assert.equal(transformEveTool(source, "tool:x", "file:///rt.mjs"), null, source);
  }
});
