// Shared by fixtures: the package, a fed fake microphone, and a sleep.
import { __testing, SPEECH } from "../../index.js";

export const { native, openFake } = __testing;
export { SPEECH };
export const sleep = (ms) => new Promise((resolve) => setTimeout(resolve, ms));

export function fedMic(sampleRate = 16_000, channels = 1) {
  const fake = new native.FakeMic(sampleRate, channels);
  fake.startFeeding(sampleRate / 100, 10);
  return fake;
}
