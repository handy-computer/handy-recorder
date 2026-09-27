// An onChunk that throws surfaces as an ordinary uncaught exception, not a
// crash in native code.
import { SPEECH, fedMic, openFake } from "./common.mjs";

const fake = fedMic();
const recorder = await openFake(fake, {
  ...SPEECH,
  onChunk: () => {
    throw new Error("boom from onChunk");
  },
});
recorder.start();
