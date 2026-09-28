// The native library's log, through setLogHandler, over the fake microphone:
// needs the test build (`npm run build:test`).

import assert from "node:assert/strict";
import { setLogHandler } from "../index.js";
import { SPEECH, fedMic, native, openFake, record, sleep } from "./fixtures/common.mjs";
import { busy, run, runtimeName, test } from "./harness.mjs";

if (!native.FakeMic) throw new Error("this suite needs the test build (npm run build:test)");
console.log(`# logging (${runtimeName})`);

/** Collects records, each with the time JavaScript received it. */
function collect(level) {
  const records = [];
  setLogHandler((record) => records.push({ ...record, receivedAt: Date.now() }), level && { level });
  return records;
}

/** Records from a 48 kHz stereo microphone, then lets the last records arrive. */
async function recordAndFlush() {
  await record(200, fedMic(48_000, 2));
  await sleep(20);
}

test("records carry level, target, message, and when they were written", async () => {
  const before = Date.now();
  const records = collect();
  await recordAndFlush();
  setLogHandler(null);
  const opened = records.find((r) => r.message.startsWith("opened Fake Mic"));
  assert.ok(opened, `no "opened" record in ${JSON.stringify(records)}`);
  assert.equal(opened.level, "info");
  assert.match(opened.target, /^handy_recorder::/);
  assert.ok(opened.timeMs >= before - 1 && opened.timeMs <= Date.now(), `timeMs ${opened.timeMs}`);
});

test("the level filters in native code: info by default, debug when asked", async () => {
  const info = collect();
  await recordAndFlush();
  assert.ok(info.length > 0);
  assert.ok(info.every((r) => ["error", "warn", "info"].includes(r.level)), JSON.stringify(info));

  const debug = collect("debug");
  await recordAndFlush();
  setLogHandler(null);
  assert.ok(debug.some((r) => r.level === "debug"), JSON.stringify(debug));
  assert.ok(debug.every((r) => r.level !== "trace"));
});

test("records written while the event loop is blocked keep their own times", async () => {
  const records = collect("debug");
  const fake = fedMic(48_000, 2);
  const recorder = await openFake(fake, SPEECH);
  recorder.start();
  busy(400); // The delivery thread logs the start and the first audio meanwhile.
  const unblocked = Date.now();
  await sleep(20);
  await recorder.stop();
  await recorder.close();
  fake.stopFeeding();
  setLogHandler(null);
  const first = records.find((r) => r.message.startsWith("first audio arrived"));
  assert.ok(first, JSON.stringify(records));
  assert.ok(first.timeMs < unblocked - 100, `written ${unblocked - first.timeMs} ms before the loop resumed`);
  assert.ok(first.receivedAt >= unblocked, "delivered only once the loop resumed");
});

test("null stops the records, and a new handler replaces the old one", async () => {
  const old = collect();
  const replacement = collect();
  await recordAndFlush();
  assert.equal(old.length, 0);
  assert.ok(replacement.length > 0);
  setLogHandler(null);
  const count = replacement.length;
  await recordAndFlush();
  assert.equal(replacement.length, count);
});

test("rejects a bad handler or level", () => {
  assert.throws(() => setLogHandler("console"), TypeError);
  assert.throws(() => setLogHandler(() => {}, { level: "verbose" }), TypeError);
  assert.throws(() => setLogHandler(() => {}, null), TypeError);
});

await run();
