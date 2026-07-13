// The published package vendors node/keel-core-stub/index.mjs (npm pack cannot
// ship files above the package root); stub semantics are conformance-frozen, so
// the vendored copy must stay byte-identical to its source. Refresh with
// scripts/sync-vendored.sh. Skips when the source is absent (installed package
// outside the repo checkout).

import test from "node:test";
import assert from "node:assert/strict";
import { existsSync, readFileSync } from "node:fs";
import { fileURLToPath } from "node:url";

const vendored = fileURLToPath(
  new URL("../src/vendor/keel-core-stub/index.mjs", import.meta.url),
);
const source = fileURLToPath(new URL("../../keel-core-stub/index.mjs", import.meta.url));

test("vendored keel-core-stub is byte-identical to its source", (t) => {
  if (!existsSync(source)) {
    t.skip("node/keel-core-stub not present (installed package, not a repo checkout)");
    return;
  }
  assert.equal(
    readFileSync(vendored, "utf8"),
    readFileSync(source, "utf8"),
    "src/vendor/keel-core-stub/index.mjs drifted from node/keel-core-stub/index.mjs; " +
      "run scripts/sync-vendored.sh",
  );
});
