// A callback that throws surfaces as an ordinary uncaught exception, not a
// crash in native code: `chunk` (onChunk) or `log` (the log handler).
import { setLogHandler } from "../../index.js";
import { SPEECH, fedMic, openFake } from "./common.mjs";

if (process.argv[2] === "log") {
  setLogHandler(() => {
    throw new Error("boom from the log handler");
  });
  await openFake(fedMic(), SPEECH);
} else {
  const recorder = await openFake(fedMic(), {
    ...SPEECH,
    onChunk: () => {
      throw new Error("boom from onChunk");
    },
  });
  recorder.start();
}
