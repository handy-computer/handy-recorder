// A minimal test runner that behaves the same under Node, Bun, and Deno:
// tests run in order, one at a time, each with a timeout; the process exits
// non-zero if any failed. (node:test and bun:test differ across runtimes,
// which is what these tests exist to compare.)

import { spawn } from "node:child_process";
import { fileURLToPath } from "node:url";

const tests = [];

export function test(name, fn, { timeout = 15_000, skip = false } = {}) {
  tests.push({ name, fn, timeout, skip });
}

export async function run() {
  let failed = 0;
  for (const { name, fn, timeout, skip } of tests) {
    if (skip) {
      console.log(`- ${name} (skipped: ${skip})`);
      continue;
    }
    const started = performance.now();
    let timer;
    try {
      await Promise.race([
        fn(),
        new Promise((_, reject) => {
          timer = setTimeout(() => reject(new Error(`timed out after ${timeout} ms`)), timeout);
        }),
      ]);
      console.log(`ok ${name} (${Math.round(performance.now() - started)} ms)`);
    } catch (error) {
      failed++;
      console.log(`FAIL ${name}\n  ${String(error?.stack ?? error).replaceAll("\n", "\n  ")}`);
    } finally {
      clearTimeout(timer);
    }
  }
  console.log(`${tests.length - failed}/${tests.length} passed`);
  process.exit(failed ? 1 : 0);
}

export const sleep = (ms) => new Promise((resolve) => setTimeout(resolve, ms));

/** Blocks the event loop, as a slow render or a synchronous native call does. */
export function busy(ms) {
  const end = performance.now() + ms;
  while (performance.now() < end) {}
}

/** The command that runs this runtime, for fixtures: `[command, ...args]`. */
export function runtimeCommand() {
  if (process.versions.bun) return [process.execPath];
  if (globalThis.Deno) return [process.execPath, "run", "-A"];
  return [process.execPath];
}

export const runtimeName = process.versions.bun
  ? `bun ${process.versions.bun}`
  : globalThis.Deno
    ? `deno ${globalThis.Deno.version.deno}`
    : `node ${process.versions.node}`;

/**
 * Runs `test/fixtures/<name>.mjs` in a new process of this runtime. Resolves
 * with its exit code, signal, output, and how long it ran; kills it after
 * `timeout` ms (`timedOut` is then true). With `signalOnReady`, sends that
 * signal once the fixture prints "ready".
 */
export function fixture(name, { args = [], timeout = 10_000, env = {}, signalOnReady } = {}) {
  const file = fileURLToPath(new URL(`./fixtures/${name}.mjs`, import.meta.url));
  const [command, ...runtimeArgs] = runtimeCommand();
  const started = performance.now();
  return new Promise((resolve, reject) => {
    const child = spawn(command, [...runtimeArgs, file, ...args], {
      env: { ...process.env, ...env },
      stdio: ["ignore", "pipe", "pipe"],
    });
    let stdout = "";
    let stderr = "";
    child.stdout.on("data", (d) => {
      stdout += d;
      if (signalOnReady && stdout.includes("ready")) {
        child.kill(signalOnReady);
        signalOnReady = undefined;
      }
    });
    child.stderr.on("data", (d) => (stderr += d));
    let timedOut = false;
    const timer = setTimeout(() => {
      timedOut = true;
      child.kill("SIGKILL");
    }, timeout);
    child.on("error", reject);
    child.on("close", (code, signal) => {
      clearTimeout(timer);
      resolve({ code, signal, stdout, stderr, timedOut, ms: performance.now() - started });
    });
  });
}

/** Checks `samples` is an unbroken stretch of the fake mic's ramp. */
export function assertRamp(samples, ramp, { channels = 1 } = {}) {
  if (samples.length === 0) throw new Error("no samples");
  const first = Math.round(samples[0] * ramp);
  for (let i = 0; i < samples.length; i++) {
    const frame = Math.floor(i / channels);
    const expected = ((first + frame) % ramp) / ramp;
    if (samples[i] !== expected) {
      throw new Error(`sample ${i} is ${samples[i] * ramp}/${ramp}, expected ${expected * ramp}/${ramp}`);
    }
  }
}
