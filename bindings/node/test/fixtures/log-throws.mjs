// A log handler that throws surfaces as an ordinary uncaught exception, not
// a crash in native code.
import { setLogHandler } from "../../index.js";
import { SPEECH, fedMic, openFake } from "./common.mjs";

setLogHandler(() => {
  throw new Error("boom from the log handler");
});
await openFake(fedMic(), SPEECH);
