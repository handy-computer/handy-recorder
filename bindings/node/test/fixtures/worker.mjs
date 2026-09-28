// Recorders inside worker threads: one records and reports back; another is
// terminated mid-recording. The main thread must carry on unharmed.
import { Worker, isMainThread, parentPort, workerData } from "node:worker_threads";
import { SPEECH, fedMic, openFake, record, sleep } from "./common.mjs";

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
  console.log(`main recorded ${(await record(200)).samples.length} samples`);
} else if (workerData === "terminate") {
  const recorder = await openFake(fedMic(), SPEECH);
  recorder.start();
  await sleep(200);
  parentPort.postMessage("recording");
  // The recording keeps this worker alive until it is terminated.
} else {
  const recording = await record(200);
  parentPort.postMessage({ samples: recording.samples.length, complete: recording.complete });
}
