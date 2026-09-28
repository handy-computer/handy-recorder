// Loads the native addon for this platform from prebuilds/.
//
// HANDY_RECORDER_NATIVE, if set, is the path of a .node file to load instead
// (this package's tests use it for the build with the fake microphone).

import { existsSync, readdirSync } from "node:fs";
import { createRequire } from "node:module";
import { dirname, join } from "node:path";
import { fileURLToPath } from "node:url";

const require = createRequire(import.meta.url);
const here = dirname(fileURLToPath(import.meta.url));

function isMusl() {
  // glibc reports its version; failing that, musl's loader gives it away.
  if (process.report?.getReport?.().header?.glibcVersionRuntime) return false;
  try {
    return readdirSync("/lib").some((name) => name.startsWith("ld-musl-"));
  } catch {
    return false;
  }
}

/** The prebuilt binary's platform suffix, as `napi build --platform` names it. */
function target() {
  const { platform, arch } = process;
  if (platform === "darwin" && (arch === "arm64" || arch === "x64")) return `darwin-${arch}`;
  if (platform === "win32" && (arch === "arm64" || arch === "x64")) return `win32-${arch}-msvc`;
  if (platform === "linux" && (arch === "arm64" || arch === "x64")) {
    return `linux-${arch}-${isMusl() ? "musl" : "gnu"}`;
  }
  return undefined;
}

function load() {
  const override = process.env.HANDY_RECORDER_NATIVE;
  if (override) return require(override);

  const suffix = target();
  const file = suffix && join(here, "prebuilds", `recorder.${suffix}.node`);
  if (!file || !existsSync(file)) {
    throw new Error(
      `@handy-computer/recorder has no prebuilt binary for ${suffix ?? `${process.platform}-${process.arch}`}` +
        " (built for macOS, Windows, and Linux glibc, each x64 and arm64)",
    );
  }
  try {
    return require(file);
  } catch (error) {
    let hint = "";
    if (process.platform === "linux" && /libasound/.test(String(error?.message))) {
      hint = " Install the ALSA library (Debian/Ubuntu: libasound2, Fedora: alsa-lib).";
    }
    const message = String(error?.message).replace(/\.?$/, ".");
    throw new Error(`@handy-computer/recorder could not load ${file}: ${message}${hint}`, {
      cause: error,
    });
  }
}

export default load();
