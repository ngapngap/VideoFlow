import { spawnSync } from "node:child_process";
import fs from "node:fs";
import path from "node:path";

function collectTestFiles(dir) {
  const out = [];
  if (!fs.existsSync(dir)) return out;
  for (const entry of fs.readdirSync(dir, { withFileTypes: true })) {
    const full = path.join(dir, entry.name);
    if (entry.isDirectory()) {
      out.push(...collectTestFiles(full));
    } else if (entry.isFile() && entry.name.endsWith(".test.js")) {
      out.push(full);
    }
  }
  return out.sort();
}

const tscBin = path.resolve("node_modules/typescript/bin/tsc");
const compileRes = spawnSync(
  process.execPath,
  [tscBin, "-p", "tests/tsconfig.test.json"],
  { stdio: "inherit" }
);
if (compileRes.status !== 0) {
  process.exit(compileRes.status ?? 1);
}

const compiledDir = path.resolve("node_modules/.cache/vf-tests/tests");
const files = collectTestFiles(compiledDir);
if (files.length === 0) {
  console.error(`No compiled .test.js files found in ${compiledDir}`);
  process.exit(1);
}

const testRes = spawnSync(
  process.execPath,
  ["--test", ...files],
  { stdio: "inherit" }
);
process.exit(testRes.status ?? 1);
