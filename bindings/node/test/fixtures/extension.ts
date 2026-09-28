// A pi-voice-like extension: TypeScript, transpiled by jiti when loaded.
import { __testing, SPEECH, type Recording } from "../../index.js";

export default async function (): Promise<void> {
  const fake = new __testing.native.FakeMic(48_000, 2);
  fake.startFeeding(480, 10);
  let chunks = 0;
  const recorder = await __testing.openFake(fake, { ...SPEECH, onChunk: () => chunks++ });
  recorder.start();
  await new Promise((resolve) => setTimeout(resolve, 300));
  const recording: Recording = await recorder.stop();
  await recorder.close();
  fake.stopFeeding();
  console.log(`extension: ${recording.samples.length} samples, ${chunks} chunks, complete ${recording.complete}`);
}
