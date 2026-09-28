// @handy-computer/recorder: microphone capture for Node.js and Bun.
//
// The native addon does the work on threads of its own and reports back
// through one ordered event channel per recorder; this wrapper turns those
// events into promises and callbacks. See index.d.ts for the API.

import native from "./native.js";

/** An error from the recorder. `code` is the library's `ErrorKind`. */
export class RecorderError extends Error {
  constructor(info) {
    super(info.message);
    this.name = "RecorderError";
    this.code = info.code;
    this.device = info.device;
    this.elapsedMs = info.elapsedMs;
    this.detail = info.detail;
  }
}

/** 16 kHz mono in 30 ms chunks: the usual speech-recognition input. */
export const SPEECH = Object.freeze({ sampleRate: 16_000, channels: "mono", framesPerChunk: 480 });

export async function listInputDevices() {
  const { devices, error } = await native.listInputDevices();
  if (error) throw new RecorderError(error);
  return devices;
}

export function permissionStatus() {
  return native.permissionStatus();
}

// The native event channel never keeps the event loop alive, so an idle
// open recorder doesn't stop the process from exiting. While an operation is
// pending, or a recording runs, this timer does.
const keepAlive = {
  holds: 0,
  timer: undefined,
  acquire() {
    if (this.holds++ === 0) this.timer = setInterval(() => {}, 2 ** 31 - 1);
  },
  release() {
    if (--this.holds === 0) {
      clearInterval(this.timer);
      this.timer = undefined;
    }
  },
};

// A throw from application code (onChunk, onFailure, a log handler) must not unwind into
// the native dispatcher; rethrow it as an ordinary uncaught exception.
function rethrowLater(error) {
  queueMicrotask(() => {
    throw error;
  });
}

const LOG_LEVELS = ["error", "warn", "info", "debug", "trace"];

export function setLogHandler(handler, options = {}) {
  if (handler !== null && typeof handler !== "function") {
    throw new TypeError("handler must be a function or null");
  }
  if (options === null || typeof options !== "object") {
    throw new TypeError("options must be an object");
  }
  const { level = "info" } = options;
  if (!LOG_LEVELS.includes(level)) {
    throw new TypeError(`level must be one of ${LOG_LEVELS.join(", ")}; got ${level}`);
  }
  const dispatch =
    handler &&
    ((record) => {
      try {
        handler(record);
      } catch (error) {
        rethrowLater(error);
      }
    });
  native.setLogHandler(dispatch, level);
}

function misuse(code, message) {
  return new RecorderError({ code, message });
}

function positiveInteger(options, name) {
  const value = options[name];
  if (value === undefined) return undefined;
  if (!Number.isInteger(value) || value <= 0 || value > 0xffff_ffff) {
    throw new TypeError(`${name} must be a positive integer; got ${value}`);
  }
  return value;
}

function nativeConfig(options) {
  if (options === null || typeof options !== "object") {
    throw new TypeError("options must be an object");
  }
  const { device, channels = "all", takeHeadset, collect, onChunk, onFailure } = options;
  if (device !== undefined && typeof device !== "string") {
    throw new TypeError("device must be an InputDevice id (a string)");
  }
  if (channels !== "all" && channels !== "mono" && !(Number.isInteger(channels) && channels >= 0)) {
    throw new TypeError(`channels must be "all", "mono", or a channel index; got ${channels}`);
  }
  for (const [name, value] of [["takeHeadset", takeHeadset], ["collect", collect]]) {
    if (value !== undefined && typeof value !== "boolean") throw new TypeError(`${name} must be a boolean`);
  }
  for (const [name, value] of [["onChunk", onChunk], ["onFailure", onFailure]]) {
    if (value !== undefined && typeof value !== "function") throw new TypeError(`${name} must be a function`);
  }
  return {
    device,
    sampleRate: positiveInteger(options, "sampleRate"),
    channels: String(channels),
    framesPerChunk: positiveInteger(options, "framesPerChunk"),
    takeHeadset,
    collect,
    chunks: onChunk !== undefined,
  };
}

const constructing = Symbol("Recorder");

// Opens a recorder. `create(config, dispatch)` returns the native recorder:
// `native.openRecorder` normally, a FakeMic's `open` in this package's tests.
function openWith(options, create) {
  let config;
  try {
    config = nativeConfig(options);
  } catch (error) {
    return Promise.reject(error);
  }
  return new Promise((resolve, reject) => {
    const recorder = new Recorder(constructing, options, { resolve, reject });
    keepAlive.acquire();
    try {
      recorder._attach(create(config, (event) => recorder._dispatch(event)));
    } catch (error) {
      keepAlive.release();
      reject(error);
    }
  });
}

/**
 * An open microphone. `start` and `stop` bracket each recording; `close`
 * releases the device.
 */
export class Recorder {
  #native;
  #info;
  #onChunk;
  #onFailure;
  /** opening, open, recording, stopping, closing, or closed. */
  #state = "opening";
  /** Keep-alive holds this recorder owns: the pending operation, a recording. */
  #holds = 0;
  #opening;
  #stopping;
  #closing;
  #failure;

  /** Use `Recorder.open`. */
  constructor(token, options, opening) {
    if (token !== constructing) throw new TypeError("use Recorder.open() to open a recorder");
    this.#onChunk = options.onChunk;
    this.#onFailure = options.onFailure;
    this.#opening = opening;
    this.#holds = 1; // openWith's
  }

  static open(options = {}) {
    return openWith(options, (config, dispatch) => native.openRecorder(config, dispatch));
  }

  /** What was opened: the device, its format, and the format you receive. */
  get info() {
    return this.#info;
  }

  get isRecording() {
    return this.#state === "recording";
  }

  get isClosed() {
    return this.#state === "closed";
  }

  /** The error the recorder failed with, if it has. It stays failed. */
  get failure() {
    return this.#failure;
  }

  /** Starts a recording. Never waits. */
  start() {
    if (this.#state === "closing" || this.#state === "closed") {
      throw misuse("RecorderClosed", "the recorder is closed");
    }
    // The library refuses too, but only until the stop thread finishes.
    if (this.#state === "stopping") {
      throw misuse("AlreadyRecording", "the previous recording is still stopping");
    }
    const error = this.#native.start();
    if (error) throw new RecorderError(error);
    this.#state = "recording";
    this.#hold();
  }

  /**
   * Ends the recording. Resolves after every `onChunk` of it, even if the
   * recorder failed meanwhile (see `endReason`).
   */
  stop() {
    if (this.#state !== "recording") {
      return Promise.reject(misuse("NotRecording", "there is no recording to stop"));
    }
    this.#state = "stopping";
    const stopping = {};
    stopping.promise = new Promise((resolve, reject) => Object.assign(stopping, { resolve, reject }));
    this.#stopping = stopping;
    this.#native.stop();
    return stopping.promise;
  }

  /** Releases the device. An unstopped recording is discarded. */
  close() {
    if (this.#state === "closed") return Promise.resolve();
    if (this.#closing) return this.#closing.promise;
    const closing = {};
    closing.promise = new Promise((resolve, reject) => Object.assign(closing, { resolve, reject }));
    this.#closing = closing;
    const pendingStop = this.#state === "stopping" ? this.#stopping.promise : undefined;
    this.#state = "closing";
    // Close after a pending stop, never alongside it.
    Promise.resolve(pendingStop)
      .catch(() => {})
      .then(() => {
        this.#hold();
        if (!this.#native.close()) this._dispatch({ kind: "closed" });
      });
    return closing.promise;
  }

  async [Symbol.asyncDispose ?? Symbol.for("Symbol.asyncDispose")]() {
    await this.close();
  }

  /** @internal */
  _attach(nativeRecorder) {
    this.#native = nativeRecorder;
  }

  /** @internal Every native event arrives here, in order. */
  _dispatch(event) {
    try {
      this.#handle(event);
    } catch (error) {
      rethrowLater(error);
    }
  }

  #hold() {
    this.#holds++;
    keepAlive.acquire();
  }

  #release() {
    if (this.#holds > 0) {
      this.#holds--;
      keepAlive.release();
    }
  }

  #releaseAll() {
    while (this.#holds > 0) this.#release();
  }

  #handle(event) {
    switch (event.kind) {
      case "opened": {
        this.#info = event.info;
        this.#state = "open";
        this.#release();
        this.#opening.resolve(this);
        this.#opening = undefined;
        return;
      }
      case "openFailed": {
        this.#state = "closed";
        this.#releaseAll();
        this.#opening.reject(new RecorderError(event.error));
        this.#opening = undefined;
        return;
      }
      case "chunk": {
        // A recording's chunks, until its stop resolves (even if close()
        // came meanwhile); none from a recording close() discarded.
        if (this.#state !== "recording" && !this.#stopping) return;
        const { sampleRate, channels } = this.#info.format;
        this.#onChunk?.({ samples: event.samples, sampleRate, channels });
        return;
      }
      case "failed": {
        // Reported from the library's own thread, so it can trail `closed`.
        if (this.#state === "closed") return;
        this.#failure = new RecorderError(event.error);
        this.#onFailure?.(this.#failure);
        return;
      }
      case "stopped":
      case "stopFailed": {
        const stopping = this.#stopping;
        this.#stopping = undefined;
        if (this.#state === "stopping") this.#state = "open";
        this.#release(); // the recording's
        if (event.kind === "stopFailed") {
          stopping.reject(new RecorderError(event.error));
          return;
        }
        const { sampleRate, channels } = this.#info.format;
        const droppedFrames = event.droppedFrames;
        let endReason;
        if (event.endReason === "recorderFailed") {
          endReason = { kind: "recorderFailed", error: new RecorderError(event.error) };
        } else if (event.endReason === "sinkPanicked") {
          endReason = { kind: "sinkPanicked", message: event.panicMessage };
        } else {
          endReason = { kind: "stopCalled" };
        }
        stopping.resolve({
          samples: event.samples,
          sampleRate,
          channels,
          endReason,
          droppedFrames,
          complete: endReason.kind === "stopCalled" && droppedFrames === 0,
        });
        return;
      }
      case "closed": {
        this.#state = "closed";
        this.#releaseAll();
        const closing = this.#closing;
        if (event.error) closing.reject(new RecorderError(event.error));
        else closing.resolve();
        return;
      }
    }
  }
}

// Not public API: this package's tests open recorders on a fake microphone
// (a build with the `test-backend` feature).
export const __testing = {
  native,
  openFake(fake, options = {}) {
    return openWith(options, (config, dispatch) => fake.open(config, dispatch));
  },
};
