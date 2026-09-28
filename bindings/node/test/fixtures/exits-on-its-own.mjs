// Nothing the package leaves behind keeps the process alive: a log handler,
// failed and timed-out opens, a timed-out close, many recordings, and an
// open, idle recorder that is never closed. This should exit on its own,
// promptly. Each step prints a line, so a hang shows where it leaked.
import { setLogHandler } from "../../index.js";
import { SPEECH, fedMic, native, openFake, sleep } from "./common.mjs";

setLogHandler(() => {}, { level: "debug" });
console.log("log handler set");

// A fake per step: a timed-out open's stream starts late, once released, and
// the fake runs one stream at a time.
const busy = new native.FakeMic(16_000, 1, 200);
busy.failNextOpen("DeviceBusy");
await openFake(busy, SPEECH).catch((e) => console.log(`open failed: ${e.code}`));

const slow = new native.FakeMic(16_000, 1, 200);
slow.hangNextStart();
await openFake(slow, SPEECH).catch((e) => console.log(`open failed: ${e.code}`));
slow.releaseStart();

const stuck = new native.FakeMic(16_000, 1, 200);
const closing = await openFake(stuck, SPEECH);
stuck.hangTeardown();
await closing.close().catch((e) => console.log(`close failed: ${e.code}`));
stuck.releaseTeardown();

const fed = fedMic();
const recorder = await openFake(fed, SPEECH);
for (let i = 0; i < 20; i++) {
  recorder.start();
  await sleep(20);
  await recorder.stop();
}
fed.stopFeeding();
// The recorder stays open, idle.
console.log("recorded 20 times");
