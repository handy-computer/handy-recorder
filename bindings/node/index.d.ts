/** An input device, from `listInputDevices`. */
export interface InputDevice {
  /** Pass as `RecorderOptions.device` to open this device. */
  id: string;
  name: string;
  /** Distinguishes devices with the same name: 0 for the first, 1 for the second. */
  occurrence: number;
  backend: string;
  isDefault: boolean;
  /** Whether `id` survives a restart or replug. False means best effort. */
  idIsStable: boolean;
  /** Channels at the device's OS format. Absent where reading it would open the device (ALSA). */
  channels?: number;
  /** A PulseAudio monitor source ("Monitor of ..."), not a microphone. */
  isMonitor: boolean;
}

export type Permission = "granted" | "denied" | "not-determined" | "unknown";

/**
 * What went wrong, and so what to do:
 *
 * - `DeviceUnavailable`, `DeviceBusy`, `OpenTimedOut`: try again, or pick another device.
 * - `PermissionDenied`: send the user to the system's privacy settings, then open a new recorder.
 * - `UnsupportedFormat`, `InvalidChannel`: change the options.
 * - `NoAudio`, `DeviceLost`, `StreamInvalidated`, `Stalled`, `Backend`: open a new recorder; this one stays failed.
 * - `AlreadyRecording`, `NotRecording`, `RecorderClosed`, `SinkStalled`: a bug in the application.
 * - `Processing`: a bug in handy-recorder.
 * - `CloseTimedOut`: nothing to recover; the platform may hold the device until the process exits.
 * - Anything else: treat as a failed recorder.
 */
export type ErrorCode =
  | "DeviceUnavailable"
  | "DeviceBusy"
  | "PermissionDenied"
  | "UnsupportedFormat"
  | "InvalidChannel"
  | "OpenTimedOut"
  | "NoAudio"
  | "DeviceLost"
  | "StreamInvalidated"
  | "Stalled"
  | "SinkStalled"
  | "Backend"
  | "Processing"
  | "AlreadyRecording"
  | "NotRecording"
  | "StopFromSink"
  | "CloseTimedOut"
  | "RecorderClosed";

export class RecorderError extends Error {
  readonly name: "RecorderError";
  /** Match on this; don't match on `message` or `detail`. */
  readonly code: ErrorCode | (string & {});
  /** The device involved, if one had been resolved. */
  readonly device?: InputDevice;
  /** How long the stream had been running. */
  readonly elapsedMs?: number;
  /** The platform's own message, verbatim. */
  readonly detail?: string;
}

/** A chunk of audio, delivered to `onChunk`. */
export interface AudioChunk {
  /** Interleaved samples: `framesPerChunk * channels`, fewer in a recording's last chunk. */
  samples: Float32Array;
  sampleRate: number;
  channels: number;
}

export interface RecorderOptions {
  /** An `InputDevice.id`. Default: the system default device. */
  device?: string;
  /** The rate you receive. Default: the device's, with no resampling. */
  sampleRate?: number;
  /** All device channels interleaved, their average, or one (zero-based). Default `"all"`. */
  channels?: "all" | "mono" | number;
  /** Frames per chunk. Default about 10 ms. */
  framesPerChunk?: number;
  /** Moves a Bluetooth headset (AirPods) to this Mac while recording. macOS only. */
  takeHeadset?: boolean;
  /** Keep the whole recording for `stop()`. Default true. */
  collect?: boolean;
  /**
   * Called with each chunk of a recording, in order, on the JavaScript
   * thread. A busy event loop delays chunks; it never loses them.
   */
  onChunk?: (chunk: AudioChunk) => void;
  /**
   * Called when the recorder fails (device lost, stalled, permission
   * revoked). `stop()` still returns what was captured; then open a new recorder.
   */
  onFailure?: (error: RecorderError) => void;
}

export type EndReason =
  | { kind: "stopCalled" }
  /** The recorder failed; the audio up to the failure is included. */
  | { kind: "recorderFailed"; error: RecorderError }
  | { kind: "sinkPanicked"; message: string };

export interface Recording {
  /** The recording's interleaved samples (empty with `collect: false`). */
  samples: Float32Array;
  sampleRate: number;
  channels: number;
  endReason: EndReason;
  /** Frames lost because the ring was full. */
  droppedFrames: number;
  /** `endReason` is `stopCalled` and nothing was dropped. */
  complete: boolean;
}

export interface Format {
  sampleRate: number;
  channels: number;
}

export interface RecorderInfo {
  device: InputDevice;
  /** The format the device runs at. */
  deviceFormat: Format;
  /** The format you receive. */
  format: Format;
  framesPerChunk: number;
}

/**
 * An open microphone. `start()` and `stop()` bracket each recording;
 * `close()` releases the device. An idle open recorder doesn't keep the
 * process alive; a running recording does, until stopped.
 */
export class Recorder implements AsyncDisposable {
  private constructor();
  /**
   * Opens a device at its OS format. Can take seconds (Bluetooth); never
   * blocks the event loop.
   */
  static open(options?: RecorderOptions): Promise<Recorder>;
  readonly info: RecorderInfo;
  readonly isRecording: boolean;
  readonly isClosed: boolean;
  /** The error the recorder failed with, if it has. It stays failed. */
  readonly failure: RecorderError | undefined;
  /** Starts a recording. Never waits. Throws a `RecorderError`. */
  start(): void;
  /** Ends the recording. Resolves after every `onChunk` of it. */
  stop(): Promise<Recording>;
  /** Releases the device. An unstopped recording is discarded. */
  close(): Promise<void>;
  [Symbol.asyncDispose](): Promise<void>;
}

/** 16 kHz mono in 30 ms chunks: the usual speech-recognition input. */
export const SPEECH: Readonly<{ sampleRate: 16000; channels: "mono"; framesPerChunk: 480 }>;

/** Lists input devices. Throws a `RecorderError`. */
export function listInputDevices(): InputDevice[];

/** Microphone permission. A synchronous read; never prompts. */
export function permissionStatus(): Permission;

export type LogLevel = "error" | "warn" | "info" | "debug" | "trace";

/** A line from the native library's log, delivered to `setLogHandler`'s handler. */
export interface LogRecord {
  level: LogLevel;
  /** The Rust module that wrote it, e.g. `handy_recorder::capture::engine`. */
  target: string;
  message: string;
  /**
   * When it was written, in ms since the epoch (like `Date.now()`), not when
   * it reached JavaScript: records written while the event loop is blocked
   * arrive afterwards with their own times.
   */
  timeMs: number;
}

/**
 * Sends the native library's log records at `level` and more severe (default
 * `"info"`) to `handler`, in order, on the JavaScript thread. One handler per
 * process: a later call replaces it, and `null` turns logging off. Up to 1024
 * records wait for a busy event loop; past that they're dropped, and a `warn`
 * record says how many. Never keeps the process alive.
 */
export function setLogHandler(
  handler: ((record: LogRecord) => void) | null,
  options?: { level?: LogLevel },
): void;
