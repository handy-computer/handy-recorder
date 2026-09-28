// A running recording keeps the process alive until it is stopped, and a
// pending stop keeps it alive until it resolves. No timer runs meanwhile:
// the recording stops itself from onChunk.
import { SPEECH, fedMic, openFake } from "./common.mjs";

const fake = fedMic();
let chunks = 0;
const recorder = await openFake(fake, {
  ...SPEECH,
  onChunk: () => {
    if (++chunks !== 20) return;
    recorder.stop().then(async (recording) => {
      console.log(`samples ${recording.samples.length}`);
      await recorder.close();
      fake.stopFeeding();
      console.log("closed");
    });
  },
});
recorder.start();
