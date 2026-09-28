// Nothing the package leaves behind keeps the process alive: a log handler,
// failed and timed-out opens, a timed-out close, many recordings, and an
// open, idle recorder that is never closed. This should exit on its own,
// promptly. Each step prints a line, so a hang shows where it leaked.
import { setLogHandler } from "../../index.js";
import { SPEECH, fedMic, native, openFake, sleep } from "./common.mjs";

setLogHandler(() => {}, { level: "debug" });
console.log("log handler set");

const fake = new native.FakeMic(16_000, 1, 200);
fake.failNextOpen("DeviceBusy");
await openFake(fake, SPEECH).catch((e) => console.log(`open failed: ${e.code}`));

fake.hangNextStart();
await openFake(fake, SPEECH).catch((e) => console.log(`open failed: ${e.code}`));
fake.releaseStart();

const closing = await openFake(fake, SPEECH);
fake.hangTeardown();
await closing.close().catch((e) => console.log(`close failed: ${e.code}`));
fake.releaseTeardown();

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
