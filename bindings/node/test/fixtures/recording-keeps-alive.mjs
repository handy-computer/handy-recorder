// A running recording keeps the process alive until it is stopped, and a
// pending stop keeps it alive until it resolves.
import { SPEECH, fedMic, openFake } from "./common.mjs";

const fake = fedMic();
const recorder = await openFake(fake, SPEECH);
recorder.start();
setTimeout(() => {
  recorder.stop().then(async (recording) => {
    console.log(`samples ${recording.samples.length}`);
    await recorder.close();
    fake.stopFeeding();
    console.log("closed");
  });
}, 300);
