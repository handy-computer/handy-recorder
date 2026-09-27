// Recorders inside worker threads: one records and reports back; another is
// terminated mid-recording. The main thread must carry on unharmed.
import { Worker, isMainThread, parentPort, workerData } from "node:worker_threads";
import { SPEECH, fedMic, openFake, sleep } from "./common.mjs";

if (isMainThread) {
  const run = (mode) => new Worker(new URL(import.meta.url), { workerData: mode });

  const complete = run("complete");
  const result = await new Promise((resolve, reject) => {
    complete.once("message", resolve);
    complete.once("error", reject);
  });
  console.log(`worker recorded ${result.samples} samples, complete ${result.complete}`);
  await new Promise((resolve) => complete.once("exit", resolve));

  const doomed = run("terminate");
  await new Promise((resolve, reject) => {
    doomed.once("message", resolve);
    doomed.once("error", reject);
  });
  await doomed.terminate();
  console.log("terminated a worker mid-recording");

  // The main thread still records.
  const fake = fedMic();
  const recorder = await openFake(fake, SPEECH);
  recorder.start();
  await sleep(200);
  const recording = await recorder.stop();
  await recorder.close();
  fake.stopFeeding();
  console.log(`main recorded ${recording.samples.length} samples`);
} else {
  const fake = fedMic();
  const recorder = await openFake(fake, SPEECH);
  recorder.start();
  await sleep(200);
  if (workerData === "terminate") {
    parentPort.postMessage("recording");
    await sleep(60_000);
  }
  const recording = await recorder.stop();
  await recorder.close();
  fake.stopFeeding();
  parentPort.postMessage({ samples: recording.samples.length, complete: recording.complete });
}
