// Recording behavior over the fake microphone: needs the test build
// (`npm run build:test`), loaded through HANDY_RECORDER_NATIVE.

import assert from "node:assert/strict";
import { Recorder, RecorderError } from "../index.js";
import { SPEECH, fedMic, native, openFake, sleep } from "./fixtures/common.mjs";
import { assertRamp, busy, run, runtimeName, test } from "./harness.mjs";

if (!native.FakeMic) throw new Error("this suite needs the test build (npm run build:test)");
console.log(`# recording (${runtimeName})`);

async function rejects(promise, code) {
  const error = await promise.then(
    () => assert.fail(`expected ${code}`),
    (e) => e,
  );
  assert.ok(error instanceof RecorderError, `expected a RecorderError, got ${error}`);
  assert.equal(error.code, code, error.message);
  return error;
}

// `fed` counts frames from just before start() until stop() resolved in
// JavaScript, so it overcounts by whatever the feeder delivered after the
// stream paused; a slow machine's catch-up makes that tens of ms. The
// recording itself may begin early by whatever audio was already queued at
// start, several blocks after a catch-up burst. Loss and duplication show up
// in assertRamp and droppedFrames, not here.
function assertFedDuring(recording, fed) {
  const { length } = recording.samples;
  assert.ok(length <= fed + 16_000 * 0.05 && length >= fed - 16_000 * 0.15, `${length} samples of ${fed} fed`);
}

function throws(fn, code) {
  assert.throws(fn, (e) => e instanceof RecorderError && e.code === code);
}

test("opens and reports what it opened", async () => {
  const fake = fedMic(48_000, 2);
  const recorder = await openFake(fake, SPEECH);
  assert.equal(recorder.info.device.name, "Fake Mic");
  assert.deepEqual(recorder.info.deviceFormat, { sampleRate: 48_000, channels: 2 });
  assert.deepEqual(recorder.info.format, { sampleRate: 16_000, channels: 1 });
  assert.equal(recorder.info.framesPerChunk, 480);
  await recorder.close();
  fake.stopFeeding();
});

test("a recording is complete and in order, and chunks add up to it", async () => {
  const fake = fedMic();
  const chunks = [];
  const recorder = await openFake(fake, { ...SPEECH, onChunk: (chunk) => chunks.push(chunk) });
  const fedBefore = fake.framesFed;
  recorder.start();
  await sleep(500);
  const recording = await recorder.stop();
  const fed = fake.framesFed - fedBefore;
  fake.stopFeeding();

  assert.equal(recording.endReason.kind, "stopCalled");
  assert.equal(recording.droppedFrames, 0);
  assert.equal(recording.complete, true);
  assert.equal(recording.sampleRate, 16_000);
  assertFedDuring(recording, fed);
  assertRamp(recording.samples, fake.ramp);

  // Every chunk is 480 frames but the last, which excludes its padding.
  for (const chunk of chunks.slice(0, -1)) assert.equal(chunk.samples.length, 480);
  assert.ok(chunks.at(-1).samples.length <= 480);
  const joined = new Float32Array(chunks.reduce((n, c) => n + c.samples.length, 0));
  let offset = 0;
  for (const { samples } of chunks) {
    joined.set(samples, offset);
    offset += samples.length;
  }
  assert.deepEqual(joined, recording.samples);
  await recorder.close();
});

test("a blocked event loop loses no audio", async () => {
  const fake = fedMic();
  let chunks = 0;
  const recorder = await openFake(fake, { ...SPEECH, onChunk: () => chunks++ });
  const fedBefore = fake.framesFed;
  recorder.start();
  await sleep(200);
  busy(2_000); // The feeder keeps delivering; the sink must never wait on us.
  await sleep(200);
  const recording = await recorder.stop();
  const fed = fake.framesFed - fedBefore;
  fake.stopFeeding();
  assert.equal(recording.droppedFrames, 0);
  assert.equal(recording.complete, true);
  assertFedDuring(recording, fed);
  assertRamp(recording.samples, fake.ramp);
  assert.equal(chunks, Math.ceil(recording.samples.length / 480));
  await recorder.close();
});

test("the event loop keeps running while a slow device opens", async () => {
  const fake = new native.FakeMic(16_000, 1, 5_000);
  fake.hangNextStart();
  let ticks = 0;
  const ticker = setInterval(() => ticks++, 10);
  const opening = openFake(fake, SPEECH);
  await sleep(300);
  fake.releaseStart();
  const recorder = await opening;
  clearInterval(ticker);
  assert.ok(ticks >= 10, `only ${ticks} ticks while opening`);
  await recorder.close();
});

test("channel selection and pass-through are exact", async () => {
  const fake = fedMic(16_000, 2);
  for (const [channels, expected] of [["all", 2], ["mono", 1], [1, 1]]) {
    const recorder = await openFake(fake, { channels });
    assert.equal(recorder.info.format.channels, expected);
    recorder.start();
    await sleep(200);
    const recording = await recorder.stop();
    assert.equal(recording.channels, expected);
    assertRamp(recording.samples, fake.ramp, { channels: expected });
    await recorder.close();
  }
  fake.stopFeeding();
});

test("collect: false still delivers chunks", async () => {
  const fake = fedMic();
  let frames = 0;
  const recorder = await openFake(fake, {
    ...SPEECH,
    collect: false,
    onChunk: ({ samples }) => (frames += samples.length),
  });
  recorder.start();
  await sleep(200);
  const recording = await recorder.stop();
  fake.stopFeeding();
  assert.equal(recording.samples.length, 0);
  assert.ok(frames > 0);
  await recorder.close();
});

test("an empty recording", async () => {
  const fake = new native.FakeMic(16_000, 1);
  const recorder = await openFake(fake, SPEECH);
  recorder.start();
  const recording = await recorder.stop();
  assert.equal(recording.samples.length, 0);
  assert.ok(recording.samples instanceof Float32Array);
  await recorder.close();
});

test("misuse: double start, stop when idle, use after close", async () => {
  const fake = fedMic();
  const recorder = await openFake(fake, SPEECH);
  await rejects(recorder.stop(), "NotRecording");
  recorder.start();
  throws(() => recorder.start(), "AlreadyRecording");
  const stopping = recorder.stop();
  throws(() => recorder.start(), "AlreadyRecording"); // while stopping
  await stopping;
  await recorder.close();
  await recorder.close(); // idempotent
  throws(() => recorder.start(), "RecorderClosed");
  await rejects(recorder.stop(), "NotRecording");
  fake.stopFeeding();
});

test("options are validated", async () => {
  const fake = new native.FakeMic(16_000, 1);
  for (const options of [
    { sampleRate: -1 },
    { sampleRate: 1.5 },
    { framesPerChunk: 0 },
    { channels: "left" },
    { channels: -1 },
    { device: 3 },
    { onChunk: "no" },
    { collect: "yes" },
  ]) {
    await assert.rejects(openFake(fake, options), TypeError, JSON.stringify(options));
  }
  await rejects(openFake(fake, { sampleRate: 10 }), "UnsupportedFormat");
  await rejects(openFake(fake, { channels: 5 }), "InvalidChannel");
  assert.throws(() => new Recorder(), TypeError);
});

test("open failures reject with the library's error", async () => {
  const fake = new native.FakeMic(16_000, 1);
  for (const [platform, code] of [
    ["DeviceBusy", "DeviceBusy"],
    ["DeviceNotAvailable", "DeviceUnavailable"],
    ["PermissionDenied", "PermissionDenied"],
    ["Other", "Backend"],
  ]) {
    fake.failNextOpen(platform);
    const error = await rejects(openFake(fake, SPEECH), code);
    assert.match(error.detail, /reported by the fake microphone/);
  }
  await rejects(openFake(fake, { device: "nope" }), "DeviceUnavailable");
});

test("open times out on a device that never starts", async () => {
  const fake = new native.FakeMic(16_000, 1, 300);
  fake.hangNextStart();
  await rejects(openFake(fake, SPEECH), "OpenTimedOut");
  fake.releaseStart();
});

test("a device lost mid-recording: failure event, audio kept, recorder stays failed", async () => {
  const fake = fedMic();
  const failures = [];
  const recorder = await openFake(fake, { ...SPEECH, onFailure: (e) => failures.push(e) });
  recorder.start();
  await sleep(300);
  fake.stopFeeding();
  assert.equal(fake.reportError("DeviceNotAvailable"), true);
  for (let i = 0; i < 100 && failures.length === 0; i++) await sleep(10);
  assert.equal(failures.length, 1);
  assert.equal(failures[0].code, "DeviceLost");
  assert.equal(recorder.failure, failures[0]);
  assert.equal(failures[0].device.name, "Fake Mic");
  assert.equal(typeof failures[0].elapsedMs, "number");

  const recording = await recorder.stop();
  assert.equal(recording.endReason.kind, "recorderFailed");
  assert.equal(recording.endReason.error.code, "DeviceLost");
  assert.equal(recording.complete, false);
  assert.ok(recording.samples.length > 0);
  assertRamp(recording.samples, fake.ramp);
  throws(() => recorder.start(), "DeviceLost");
  await recorder.close();
});

test("close discards an unstopped recording", async () => {
  const fake = fedMic();
  const recorder = await openFake(fake, SPEECH);
  recorder.start();
  await sleep(100);
  await recorder.close();
  assert.equal(recorder.isClosed, true);
  assert.equal(fake.isStreaming, false);
  fake.stopFeeding();
});

test("close during stop waits for the stop, and every chunk still arrives", async () => {
  const fake = fedMic();
  let frames = 0;
  const recorder = await openFake(fake, { ...SPEECH, onChunk: ({ samples }) => (frames += samples.length) });
  recorder.start();
  await sleep(100);
  busy(100); // Queue up chunks the close() below must not drop.
  const stopping = recorder.stop();
  const closing = recorder.close();
  const recording = await stopping;
  assert.ok(recording.samples.length > 0);
  assert.equal(frames, recording.samples.length);
  await closing;
  assert.equal(fake.isStreaming, false);
  fake.stopFeeding();
});

test("close times out on a device that never tears down", async () => {
  const fake = new native.FakeMic(16_000, 1, 300);
  const recorder = await openFake(fake, SPEECH);
  fake.hangTeardown();
  await rejects(recorder.close(), "CloseTimedOut");
  assert.equal(recorder.isClosed, true);
  fake.releaseTeardown();
});

test("asyncDispose closes", async () => {
  const fake = new native.FakeMic(16_000, 1);
  const recorder = await openFake(fake, SPEECH);
  const dispose = Symbol.asyncDispose ?? Symbol.for("Symbol.asyncDispose");
  await recorder[dispose]();
  assert.equal(recorder.isClosed, true);
});

test("several recorders at once", async () => {
  const fakes = [fedMic(), fedMic(48_000, 2), fedMic(44_100, 1)];
  const recorders = await Promise.all(fakes.map((fake) => openFake(fake, SPEECH)));
  for (const recorder of recorders) recorder.start();
  await sleep(300);
  const recordings = await Promise.all(recorders.map((r) => r.stop()));
  for (const recording of recordings) {
    assert.equal(recording.complete, true);
    assert.ok(recording.samples.length > 0);
  }
  assertRamp(recordings[0].samples, fakes[0].ramp);
  await Promise.all(recorders.map((r) => r.close()));
  for (const fake of fakes) fake.stopFeeding();
});

await run();
