# @handy-computer/recorder

Microphone capture for Node.js, Bun, and Deno: the Node binding of [handy-recorder](../../README.md).

Opens a mic at whatever format the OS has it set to and hands you fixed-size chunks of audio at the rate and channel layout you asked for. It tells you when audio was lost or the device broke instead of quietly giving you silence. Nothing blocks the event loop, and a busy event loop delays audio without losing it.

```js
import { Recorder, SPEECH } from "@handy-computer/recorder";

const recorder = await Recorder.open({
  ...SPEECH, // 16 kHz mono, 480-frame chunks
  onChunk: ({ samples }) => meter.push(samples), // optional, in order
  onFailure: (error) => console.error(`microphone failed: ${error.code}`),
});

recorder.start();
// ...
const recording = await recorder.stop(); // after every onChunk of it
if (!recording.complete) console.warn(recording.endReason, recording.droppedFrames);
use(recording.samples); // Float32Array

await recorder.close();
```

- `listInputDevices()` lists devices; pass an `id` as `device`.
- `permissionStatus()` reads the microphone permission without prompting.
- Errors are `RecorderError`s. Match on `code`; `index.d.ts` says what to do for each.
- `open` can take seconds (Bluetooth); `stop` and `close` can too when a device misbehaves. None of them block the event loop.
- An idle open recorder doesn't keep the process alive; a running recording does, until stopped. Close recorders you're done with; each holds its device.

Prebuilt for macOS (arm64, x64; 11+), Windows (x64, arm64), and Linux glibc 2.31+ (x64, arm64). There are no install scripts. On Linux it uses PulseAudio or PipeWire when running, with ALSA as the fallback. Linux musl (Alpine) isn't built yet.

## Developing

Needs Rust and Node 18+.

```sh
npm ci
npm run build          # prebuilds/recorder.<platform>.node, the published build
npm run build:test     # build/test/, with the fake microphone the tests drive
node test/run.mjs                         # every test, under this Node
node test/run.mjs --runtime bun           # ... under Bun
node test/run.mjs --runtime "deno run -A" # ... under Deno
npm run typecheck      # index.d.ts against test/types.ts
```

The tests use a small runner of their own (`test/harness.mjs`) so the same files run unchanged under every runtime. `test/recording.test.mjs` covers recording behavior over the fake microphone. `test/lifecycle.test.mjs` runs processes that exit, get signalled, throw, use worker threads, reload through jiti, or load the package from a compiled Bun binary, as pi does.

`test/smoke.mjs` checks an installed package against real hardware. Set `HANDY_RECORDER_SMOKE_DEVICE` to a device name, or to `default`, to also record a second.

`HANDY_RECORDER_NATIVE` overrides which `.node` file is loaded.
