// Process lifecycle, each case in a process of its own (test/fixtures/):
// exiting, signals, uncaught errors, worker threads, and loading through
// jiti with reloads, as pi loads extensions. Needs the test build.

import assert from "node:assert/strict";
import { spawnSync } from "node:child_process";
import { mkdtempSync, rmSync } from "node:fs";
import { tmpdir } from "node:os";
import { join } from "node:path";
import { fileURLToPath } from "node:url";
import { fixture, run, runtimeName, test } from "./harness.mjs";

console.log(`# lifecycle (${runtimeName})`);
const windows = process.platform === "win32";

function ok(result) {
  assert.equal(result.timedOut, false, `hung; output:\n${result.stdout}${result.stderr}`);
  assert.equal(result.code, 0, `exit ${result.code ?? result.signal}; output:\n${result.stdout}${result.stderr}`);
}

// A crash in native code shows up as one of these, never as an exit code.
function notCrashed(result) {
  assert.ok(
    !["SIGSEGV", "SIGBUS", "SIGABRT", "SIGILL"].includes(result.signal),
    `crashed with ${result.signal}; output:\n${result.stdout}${result.stderr}`,
  );
}

test("an idle open recorder does not keep the process alive", async () => {
  const result = await fixture("idle-open");
  ok(result);
  assert.match(result.stdout, /opened/);
  assert.ok(result.ms < 5_000, `took ${Math.round(result.ms)} ms to exit`);
});

test("a recording keeps the process alive until stopped", async () => {
  const result = await fixture("recording-keeps-alive");
  ok(result);
  assert.match(result.stdout, /samples [1-9]\d*/);
  assert.match(result.stdout, /closed/);
});

test("process.exit mid-recording exits promptly", async () => {
  const result = await fixture("ends-mid-recording", { args: ["exit"] });
  notCrashed(result);
  assert.equal(result.timedOut, false);
  assert.equal(result.code, 7, result.stderr);
});

test("an uncaught exception mid-recording exits promptly", async () => {
  const result = await fixture("ends-mid-recording", { args: ["throw"] });
  notCrashed(result);
  assert.equal(result.timedOut, false);
  assert.notEqual(result.code, 0);
  assert.match(result.stderr, /uncaught mid-recording/);
});

test("an unhandled rejection mid-recording exits promptly", async () => {
  const result = await fixture("ends-mid-recording", { args: ["reject"] });
  notCrashed(result);
  assert.equal(result.timedOut, false);
  assert.notEqual(result.code, 0);
  assert.match(result.stderr, /unhandled mid-recording/);
});

for (const signal of ["SIGINT", "SIGTERM"]) {
  test(`${signal} mid-recording ends the process`, async () => {
    const result = await fixture("ends-mid-recording", { args: ["signal"], signalOnReady: signal });
    notCrashed(result);
    assert.equal(result.timedOut, false, "hung after the signal");
    assert.match(result.stdout, /ready [1-9]/);
  }, { skip: windows && "no POSIX signals on Windows" });
}

test("a throwing onChunk is an ordinary uncaught exception", async () => {
  const result = await fixture("chunk-throws");
  notCrashed(result);
  assert.equal(result.timedOut, false);
  assert.notEqual(result.code, 0);
  assert.match(result.stderr, /boom from onChunk/);
});

test("worker threads: record, and terminate one mid-recording", async () => {
  const result = await fixture("worker", { timeout: 20_000 });
  ok(result);
  assert.match(result.stdout, /worker recorded [1-9]\d* samples, complete true/);
  assert.match(result.stdout, /terminated a worker mid-recording/);
  assert.match(result.stdout, /main recorded [1-9]\d* samples/);
});

test("loaded through jiti, reloaded while recording (as pi does)", async () => {
  const result = await fixture("jiti", { timeout: 20_000 });
  ok(result);
  assert.match(result.stdout, /reloaded: new module instance true/);
  assert.match(result.stdout, /first [1-9]\d* complete true; second [1-9]\d* complete true/);
});

// pi also ships as a `bun build --compile` binary, which loads extensions
// from disk through jiti. Needs `bun` on PATH; runs under every runtime.
const bun = spawnSync("bun", ["--version"], { encoding: "utf8" });
test("loaded by a compiled Bun binary through jiti (pi's binary)", async () => {
  const dir = mkdtempSync(join(tmpdir(), "handy-recorder-pi-host-"));
  try {
    const host = join(dir, process.platform === "win32" ? "pi-host.exe" : "pi-host");
    const fixtures = fileURLToPath(new URL("./fixtures/", import.meta.url));
    const build = spawnSync("bun", ["build", "--compile", join(fixtures, "pi-host.mjs"), "--outfile", host], {
      encoding: "utf8",
    });
    assert.equal(build.status, 0, build.stderr);
    const result = spawnSync(host, [join(fixtures, "extension.ts")], { encoding: "utf8", timeout: 20_000 });
    assert.equal(result.status, 0, `${result.signal ?? ""}\n${result.stdout}${result.stderr}`);
    assert.match(result.stdout, /host: compiled true/);
    assert.match(result.stdout, /extension: [1-9]\d* samples, [1-9]\d* chunks, complete true/);
  } finally {
    rmSync(dir, { recursive: true, force: true });
  }
}, { timeout: 60_000, skip: bun.status !== 0 && "bun is not on PATH" });

await run();
