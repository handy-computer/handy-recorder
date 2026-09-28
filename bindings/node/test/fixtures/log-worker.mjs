// A worker sets the process's log handler, then exits. The main thread's
// recorder logs into the dead environment, harmlessly, and a handler it sets
// afterwards receives records.
import { Worker, isMainThread, parentPort } from "node:worker_threads";
import { setLogHandler } from "../../index.js";
import { record, sleep } from "./common.mjs";

if (isMainThread) {
  const worker = new Worker(new URL(import.meta.url));
  const worked = await new Promise((resolve, reject) => {
    worker.once("message", resolve);
    worker.once("error", reject);
  });
  await new Promise((resolve) => worker.once("exit", resolve));
  console.log(`worker logged ${worked} records`);

  console.log(`main recorded ${(await record()).samples.length} samples with the worker's handler gone`);
  let records = 0;
  setLogHandler(() => records++);
  await record();
  await sleep(20);
  console.log(`main logged ${records} records`);
} else {
  let records = 0;
  setLogHandler(() => records++);
  await record();
  await sleep(20);
  parentPort.postMessage(records);
}
