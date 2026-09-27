// Loads the package the way pi loads extensions: through jiti (pi's version,
// `jiti/static`), with its module cache off (every load is a fresh module
// instance) and, as in pi's Bun binary, tryNative off so jiti handles every
// import. Then "reloads" while the first instance is still recording, as an
// extension reload does.
import { createJiti } from "jiti/static";

const jiti = createJiti(import.meta.url, { moduleCache: false, tryNative: false });
const load = () => jiti.import(new URL("../../index.js", import.meta.url).href);
const sleep = (ms) => new Promise((resolve) => setTimeout(resolve, ms));

const first = await load();
const firstFake = new first.__testing.native.FakeMic(16_000, 1);
firstFake.startFeeding(160, 10);
const firstRecorder = await first.__testing.openFake(firstFake, first.SPEECH);
firstRecorder.start();
await sleep(200);

const second = await load();
console.log(`reloaded: new module instance ${second !== first}`);
const secondFake = new second.__testing.native.FakeMic(48_000, 2);
secondFake.startFeeding(480, 10);
const secondRecorder = await second.__testing.openFake(secondFake, second.SPEECH);
secondRecorder.start();
await sleep(200);

const [a, b] = await Promise.all([firstRecorder.stop(), secondRecorder.stop()]);
await Promise.all([firstRecorder.close(), secondRecorder.close()]);
firstFake.stopFeeding();
secondFake.stopFeeding();
console.log(`first ${a.samples.length} complete ${a.complete}; second ${b.samples.length} complete ${b.complete}`);
console.log(`errors are the same class: ${second.RecorderError === first.RecorderError}`);
