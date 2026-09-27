// Prints a Markdown size report for the packed package: each prebuilt
// binary (raw and gzipped) and the tarball. CI appends it to the job summary.
//
//   node test/ci/size-report.mjs handy-computer-recorder-0.0.0.tgz

import { readdirSync, readFileSync, statSync } from "node:fs";
import { join } from "node:path";
import { fileURLToPath } from "node:url";
import { gzipSync } from "node:zlib";

const tarball = process.argv[2];
const prebuilds = fileURLToPath(new URL("../../prebuilds/", import.meta.url));
const kb = (bytes) => `${(bytes / 1024).toFixed(0)} KB`;

const rows = readdirSync(prebuilds)
  .filter((f) => f.endsWith(".node"))
  .sort()
  .map((file) => {
    const bytes = readFileSync(join(prebuilds, file));
    return { file, raw: bytes.length, gzip: gzipSync(bytes, { level: 9 }).length };
  });

const total = rows.reduce((sum, r) => ({ raw: sum.raw + r.raw, gzip: sum.gzip + r.gzip }), { raw: 0, gzip: 0 });
console.log("| Binary | Size | Gzipped |\n|---|---:|---:|");
for (const r of rows) console.log(`| ${r.file} | ${kb(r.raw)} | ${kb(r.gzip)} |`);
console.log(`| **All ${rows.length}** | **${kb(total.raw)}** | **${kb(total.gzip)}** |`);
if (tarball) console.log(`\nPacked tarball (\`${tarball}\`): **${kb(statSync(tarball).size)}**`);
