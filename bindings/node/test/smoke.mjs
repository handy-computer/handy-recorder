// Checks an installed package (the published build, real backend): it loads,
// the platform calls answer, and errors come back as RecorderErrors. Run from
// a project that depends on @handy-computer/recorder:
//
//   node smoke.mjs
//   HANDY_RECORDER_SMOKE_DEVICE="MacBook Pro Microphone" node smoke.mjs
//
// With HANDY_RECORDER_SMOKE_DEVICE (a device name substring, or "default"),
// also records one second from that device and expects real audio.

import assert from "node:assert/strict";
import { createRequire } from "node:module";

import { Recorder, RecorderError, SPEECH, listInputDevices, permissionStatus, __testing } from "@handy-computer/recorder";

const runtime = process.versions.bun ? `bun ${process.versions.bun}` : globalThis.Deno ? `deno ${Deno.version.deno}` : `node ${process.versions.node}`;
const { version } = createRequire(import.meta.url)("@handy-computer/recorder/package.json");
console.log(`@handy-computer/recorder ${version} on ${runtime}, ${process.platform}-${process.arch}`);

// The published build never carries the fake microphone.
assert.equal(__testing.native.FakeMic, undefined, "a test build was published");

const permission = permissionStatus();
assert.ok(["granted", "denied", "not-determined", "unknown"].includes(permission), permission);
console.log(`permission: ${permission}`);

let devices = [];
try {
  devices = await listInputDevices();
  for (const d of devices) {
    console.log(`device: ${d.name}${d.isDefault ? " (default)" : ""} [${d.backend}] ${d.id}${d.isMonitor ? " monitor" : ""}`);
  }
} catch (error) {
  assert.ok(error instanceof RecorderError, String(error));
  console.log(`listInputDevices: ${error.code}: ${error.message}`);
}

const bogus = await Recorder.open({ device: "handy-recorder-smoke:no-such-device" }).then(
  () => assert.fail("opened a device that does not exist"),
  (error) => error,
);
assert.ok(bogus instanceof RecorderError, String(bogus));
assert.equal(bogus.code, "DeviceUnavailable", bogus.message);
console.log(`bogus device: ${bogus.code}`);

const wanted = process.env.HANDY_RECORDER_SMOKE_DEVICE;
if (wanted) {
  const device =
    wanted === "default" ? undefined : devices.find((d) => d.name.includes(wanted))?.id;
  assert.ok(wanted === "default" || device, `no device matching ${wanted}`);
  let chunks = 0;
  const recorder = await Recorder.open({ ...SPEECH, device, onChunk: () => chunks++ });
  console.log(`recording from ${recorder.info.device.name} at ${recorder.info.deviceFormat.sampleRate} Hz`);
  recorder.start();
  await new Promise((resolve) => setTimeout(resolve, 1_000));
  const recording = await recorder.stop();
  await recorder.close();
  const peak = recording.samples.reduce((max, s) => Math.max(max, Math.abs(s)), 0);
  console.log(
    `recorded ${recording.samples.length} samples in ${chunks} chunks, peak ${peak.toFixed(4)}, ${recording.endReason.kind}, dropped ${recording.droppedFrames}`,
  );
  assert.equal(recording.complete, true);
  assert.ok(recording.samples.length > 12_000, `only ${recording.samples.length} samples`);
  assert.ok(peak > 0, "digital silence: muted, or permission denied?");
}

console.log("smoke ok");
