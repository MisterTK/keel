// The embedded Level 0 pack must equal the frozen contracts/defaults.toml.
// This both guards against drift and exercises the TOML parser on the real
// contract file. Skips gracefully when run from a packaged install where
// contracts/ is not present.

import test from "node:test";
import assert from "node:assert/strict";
import { existsSync, readFileSync } from "node:fs";
import { fileURLToPath } from "node:url";
import { parseToml } from "../src/policy.mjs";
import { level0Defaults } from "../src/defaults.mjs";

const contractPath = fileURLToPath(new URL("../../../contracts/defaults.toml", import.meta.url));

test("embedded Level 0 defaults match contracts/defaults.toml", { skip: !existsSync(contractPath) }, () => {
  const parsed = parseToml(readFileSync(contractPath, "utf8"));
  assert.deepEqual(parsed, level0Defaults());
});
