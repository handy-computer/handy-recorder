// Runs every test/*.test.mjs, each in its own process, under a runtime:
//
//   node test/run.mjs                    # this Node
//   node test/run.mjs --runtime bun
//   node test/run.mjs --runtime "deno run -A"
//   node test/run.mjs recording          # only files matching "recording"
//
// Loads the test build (`npm run build:test`, with the fake microphone)
// unless HANDY_RECORDER_NATIVE is already set.

import { spawnSync } from "node:child_process";
import { existsSync, readdirSync } from "node:fs";
import { dirname, join } from "node:path";
import { fileURLToPath } from "node:url";

const here = dirname(fileURLToPath(import.meta.url));
const args = process.argv.slice(2);
let runtime = [process.execPath];
const filters = [];
for (let i = 0; i < args.length; i++) {
  if (args[i] === "--runtime") runtime = args[++i].split(" ");
  else filters.push(args[i]);
}

const env = { ...process.env };
if (!env.HANDY_RECORDER_NATIVE) {
  const dir = join(here, "..", "build", "test");
  const builds = existsSync(dir) ? readdirSync(dir).filter((f) => f.endsWith(".node")) : [];
  if (builds.length !== 1) {
    console.error(`expected one test build in ${dir}, found ${builds.length}; run npm run build:test`);
    process.exit(1);
  }
  env.HANDY_RECORDER_NATIVE = join(dir, builds[0]);
}

const files = readdirSync(here)
  .filter((f) => f.endsWith(".test.mjs"))
  .filter((f) => filters.length === 0 || filters.some((filter) => f.includes(filter)))
  .sort();

let failed = 0;
for (const file of files) {
  const [command, ...runtimeArgs] = runtime;
  const result = spawnSync(command, [...runtimeArgs, join(here, file)], {
    env,
    stdio: "inherit",
    timeout: 300_000,
  });
  if (result.status !== 0) {
    failed++;
    console.log(`${file}: exit ${result.status ?? result.signal}${result.error ? ` (${result.error.message})` : ""}`);
  }
}
console.log(failed ? `${failed} of ${files.length} test files failed` : `all ${files.length} test files passed`);
process.exit(failed ? 1 : 0);
