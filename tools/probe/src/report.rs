//! The results file: everything a run prints, the library's debug log, and
//! a header describing the machine, so one file per run says what happened.

use std::{
    fs::{self, File},
    io::Write,
    path::PathBuf,
    process::Command,
    sync::{Mutex, OnceLock},
    time::{SystemTime, UNIX_EPOCH},
};

static FILE: OnceLock<Mutex<File>> = OnceLock::new();

/// Prints a line and appends it to the results file.
#[macro_export]
macro_rules! say {
    () => { $crate::report::line("") };
    ($($arg:tt)*) => { $crate::report::line(&format!($($arg)*)) };
}

pub fn line(text: &str) {
    println!("{text}");
    append(text);
}

/// Appends to the results file only.
pub fn append(text: &str) {
    if let Some(file) = FILE.get() {
        let _ = writeln!(file.lock().unwrap(), "{text}");
    }
}

/// Creates `results/<utc time>-<os>-<probe>.txt` and installs the logger.
pub fn start(probe: &str) -> Option<PathBuf> {
    let dir = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("results");
    let _ = fs::create_dir_all(&dir);
    let path = dir.join(format!("{}-{}-{probe}.txt", timestamp(true), std::env::consts::OS));
    let file = File::create(&path).ok()?;
    let _ = FILE.set(Mutex::new(file));
    Some(path)
}

pub fn install_logger() {
    static LOGGER: Logger = Logger;
    let _ = log::set_logger(&LOGGER);
    log::set_max_level(log::LevelFilter::Debug);
}

/// The library's log: all of it (debug and up) to the results file; warnings
/// and errors also to stderr, unless PROBE_VERBOSE is set, which shows all.
struct Logger;

impl log::Log for Logger {
    fn enabled(&self, metadata: &log::Metadata) -> bool {
        metadata.target().starts_with("handy_recorder") && metadata.level() <= log::Level::Debug
    }

    fn log(&self, record: &log::Record) {
        if !self.enabled(record.metadata()) {
            return;
        }
        let text = format!(
            "[{} {:<5} {}] {}",
            timestamp(false),
            record.level(),
            record.target(),
            record.args()
        );
        if record.level() <= log::Level::Warn || std::env::var_os("PROBE_VERBOSE").is_some() {
            eprintln!("{text}");
        }
        append(&text);
    }

    fn flush(&self) {}
}

/// UTC, as `20260924-051822` (for file names) or `05:18:22.123Z`.
pub fn timestamp(for_file: bool) -> String {
    let now = SystemTime::now().duration_since(UNIX_EPOCH).unwrap_or_default();
    let secs = now.as_secs();
    let (y, m, d) = civil_from_days((secs / 86_400) as i64);
    let (hh, mm, ss) = (secs / 3600 % 24, secs / 60 % 60, secs % 60);
    if for_file {
        format!("{y:04}{m:02}{d:02}-{hh:02}{mm:02}{ss:02}")
    } else {
        format!("{hh:02}:{mm:02}:{ss:02}.{:03}Z", now.subsec_millis())
    }
}

/// Days since 1970-01-01 to (year, month, day), proleptic Gregorian.
fn civil_from_days(z: i64) -> (i64, u32, u32) {
    let z = z + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z.rem_euclid(146_097);
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = (doy - (153 * mp + 2) / 5 + 1) as u32;
    let m = if mp < 10 { mp + 3 } else { mp - 9 } as u32;
    (yoe + era * 400 + i64::from(m <= 2), m, d)
}

fn command(program: &str, args: &[&str]) -> Option<String> {
    let output = Command::new(program).args(args).output().ok()?;
    output
        .status
        .success()
        .then(|| String::from_utf8_lossy(&output.stdout).trim().to_owned())
}

/// A description of the machine: OS, version, audio server, and library
/// commit. Best effort: anything unavailable is left out.
pub fn machine() -> Vec<String> {
    let mut lines = vec![
        format!("time: {}", timestamp(false)),
        format!(
            "os: {} {}",
            std::env::consts::OS,
            std::env::consts::ARCH
        ),
        format!("handy-recorder commit: {}", env!("PROBE_GIT_COMMIT")),
    ];
    match std::env::consts::OS {
        "macos" => {
            if let Some(v) = command("sw_vers", &["-productVersion"]) {
                lines.push(format!("macOS {v}"));
            }
            if let Some(model) = command("sysctl", &["-n", "hw.model"]) {
                lines.push(format!("model: {model}"));
            }
        }
        "windows" => {
            if let Some(v) = command("cmd", &["/C", "ver"]) {
                lines.push(format!("windows: {v}"));
            }
        }
        "linux" => {
            if let Ok(release) = fs::read_to_string("/etc/os-release")
                && let Some(name) = release.lines().find_map(|l| l.strip_prefix("PRETTY_NAME="))
            {
                lines.push(format!("distribution: {}", name.trim_matches('"')));
            }
            if let Some(kernel) = command("uname", &["-r"]) {
                lines.push(format!("kernel: {kernel}"));
            }
            match command("pactl", &["info"]) {
                Some(info) => {
                    for line in info.lines().filter(|l| {
                        l.starts_with("Server Name") || l.starts_with("Server Version")
                            || l.starts_with("Default Source")
                    }) {
                        lines.push(format!("sound server: {line}"));
                    }
                }
                None => lines.push("sound server: pactl unavailable (no PulseAudio/pipewire-pulse?)".into()),
            }
        }
        _ => {}
    }
    lines
}
