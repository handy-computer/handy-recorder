// A log handler, with an idle open recorder, must not keep the process
// alive: this should exit on its own, promptly.
import { setLogHandler } from "../../index.js";
import { SPEECH, native, openFake } from "./common.mjs";

setLogHandler((record) => console.log(`log ${record.level} ${record.message}`), { level: "debug" });
await openFake(new native.FakeMic(16_000, 1), SPEECH);
console.log("opened");
