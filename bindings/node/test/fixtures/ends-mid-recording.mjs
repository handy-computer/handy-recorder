// Ends the process mid-recording in the way argv[2] says: `exit`
// (process.exit), `throw` (an uncaught exception), `reject` (an unhandled
// rejection), or `signal` (waits for the parent's signal after "ready").
import { SPEECH, fedMic, openFake, sleep } from "./common.mjs";

const mode = process.argv[2];
const fake = fedMic();
let chunks = 0;
const recorder = await openFake(fake, { ...SPEECH, onChunk: () => chunks++ });
recorder.start();
// Recording in earnest: audio has reached JavaScript.
for (let i = 0; i < 500 && chunks === 0; i++) await sleep(10);
console.log(`ready ${chunks}`);
if (mode === "exit") process.exit(7);
if (mode === "throw") setTimeout(() => { throw new Error("uncaught mid-recording"); }, 0);
if (mode === "reject") Promise.reject(new Error("unhandled mid-recording"));
// `signal`: the recording keeps the process alive until the signal.
