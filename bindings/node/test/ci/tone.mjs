// Writes a steady 440 Hz tone as a WAV for CI to play into a virtual device:
// `node tone.mjs <file> [seconds]` (default 300), with no gaps to record.

import { writeFileSync } from "node:fs";

const [file, seconds = "300"] = process.argv.slice(2);
const rate = 22_050;
const frames = rate * Number(seconds);
const wav = Buffer.alloc(44 + frames * 2);
wav.write("RIFF", 0);
wav.writeUInt32LE(36 + frames * 2, 4);
wav.write("WAVEfmt ", 8);
wav.writeUInt32LE(16, 16); // fmt chunk size
wav.writeUInt16LE(1, 20); // PCM
wav.writeUInt16LE(1, 22); // mono
wav.writeUInt32LE(rate, 24);
wav.writeUInt32LE(rate * 2, 28); // byte rate
wav.writeUInt16LE(2, 32); // block align
wav.writeUInt16LE(16, 34); // bits per sample
wav.write("data", 36);
wav.writeUInt32LE(frames * 2, 40);
for (let i = 0; i < frames; i++) {
  wav.writeInt16LE(Math.round(Math.sin((2 * Math.PI * 440 * i) / rate) * 0.3 * 32767), 44 + i * 2);
}
writeFileSync(file, wav);
