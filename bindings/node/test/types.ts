// Compiled by `npm run typecheck`, never run: the declarations in index.d.ts
// accept the intended uses and reject the wrong ones.
import {
  Recorder,
  RecorderError,
  SPEECH,
  listInputDevices,
  permissionStatus,
  setLogHandler,
  type AudioChunk,
  type InputDevice,
  type LogRecord,
  type Permission,
  type Recording,
} from "@handy-computer/recorder";

export async function dictate(): Promise<Float32Array> {
  const permission: Permission = permissionStatus();
  if (permission === "denied") throw new Error("denied");
  const devices: InputDevice[] = listInputDevices();
  const microphone = devices.find((d) => !d.isMonitor && d.isDefault);

  await using recorder = await Recorder.open({
    ...SPEECH,
    device: microphone?.id,
    onChunk: ({ samples, sampleRate }: AudioChunk) => void [samples.length, sampleRate],
    onFailure: (error) => void error.code,
  });
  recorder.start();
  const recording: Recording = await recorder.stop();
  if (recording.endReason.kind === "recorderFailed") {
    const code: string = recording.endReason.error.code;
    void code;
  }
  return recording.samples;
}

export function classify(error: unknown): string {
  if (error instanceof RecorderError) {
    switch (error.code) {
      case "PermissionDenied":
        return "settings";
      case "DeviceUnavailable":
        return "pick another";
      default:
        return error.detail ?? error.message;
    }
  }
  return "other";
}

export function log(lines: string[]): void {
  setLogHandler(({ level, target, message, timeMs }: LogRecord) => {
    lines.push(`${new Date(timeMs).toISOString()} ${level} ${target} ${message}`);
  }, { level: "debug" });
  setLogHandler(null);
}

// @ts-expect-error: levels are the five names.
setLogHandler(() => {}, { level: "verbose" });
// @ts-expect-error: not constructible; use Recorder.open.
new Recorder();
// @ts-expect-error: channels is "all", "mono", or an index.
void Recorder.open({ channels: "left" });
