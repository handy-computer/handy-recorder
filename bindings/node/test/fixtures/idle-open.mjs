// An open, idle recorder that is never closed must not keep the process
// alive: this should exit on its own, promptly.
import { SPEECH, native, openFake } from "./common.mjs";

const fake = new native.FakeMic(16_000, 1);
await openFake(fake, SPEECH);
console.log("opened");
