// Shared by the tests and fixtures: the package, a fed fake microphone, a
// short recording, and a sleep. Needs the test build.
import { __testing, SPEECH } from "../../index.js";

export const { native, openFake } = __testing;
export { SPEECH };
export const sleep = (ms) => new Promise((resolve) => setTimeout(resolve, ms));

// 10 ms blocks, as a real device delivers them.
export function fedMic(sampleRate = 16_000, channels = 1) {
  const fake = new native.FakeMic(sampleRate, channels);
  fake.startFeeding(sampleRate / 100, 10);
  return fake;
}

/** Records `ms` from a fresh fed microphone; resolves with the recording. */
export async function record(ms = 100, fake = fedMic()) {
  const recorder = await openFake(fake, SPEECH);
  recorder.start();
  await sleep(ms);
  const recording = await recorder.stop();
  await recorder.close();
  fake.stopFeeding();
  return recording;
}
