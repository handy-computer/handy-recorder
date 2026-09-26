//! Hardware probes for handy-recorder. Each probe drives the public API
//! against a real device, asks the operator to act where it needs to
//! ("disconnect now"), and prints PASS, FAIL, or INFO with what it measured.
//!
//! Usage: handy-recorder-probe <probe|auto|all> [--device <id>] [--secs <n>] [--take-headset]
//! Run with no arguments for the list of probes; README.md has the matrix.
//! Every run writes `results/<time>-<os>-<probe>.txt` with the machine
//! description, all output, and the library's debug log.

#[macro_use]
mod report;

use std::{
    env,
    io::{self, BufRead, Write},
    process::{Child, ExitCode, Stdio},
    sync::{
        atomic::{AtomicBool, Ordering},
        mpsc,
    },
    thread,
    time::{Duration, Instant, SystemTime},
};

use handy_recorder::{
    AudioChunk, EndReason, Error, ErrorKind, Permission, Recorder, RecorderConfig, Sink, Stopped,
    list_input_devices, permission_status,
};

const RATE: f64 = 16_000.0;
/// How long to wait for the operator's action to produce a failure.
const ACTION_WAIT: Duration = Duration::from_secs(30);

const PROBES: &[(&str, &str, bool)] = &[
    // (name, description, interactive)
    ("list", "permission status and input devices", false),
    (
        "baseline",
        "record --secs (default 3): real audio, complete, the right length",
        false,
    ),
    (
        "warm-cycles",
        "30 quick recordings on one open recorder",
        false,
    ),
    (
        "two-clients",
        "two recorders on one device at once; closing one leaves the other working",
        false,
    ),
    (
        "second-process",
        "another process records; this one opens, records and closes meanwhile",
        false,
    ),
    (
        "virtual-disconnect",
        "Linux: remove a virtual PulseAudio source under a recording and an idle recorder",
        false,
    ),
    (
        "disconnect",
        "unplug or turn off the device under a recording and an idle recorder; reconnect",
        true,
    ),
    (
        "bluetooth-handoff",
        "move a shared headset to the phone under a recording and an idle recorder; bring it back",
        true,
    ),
    (
        "meeting-app",
        "recorders kept open across a real call (Meet, Zoom), and new ones during and after it",
        true,
    ),
    (
        "sleep",
        "sleep and wake with an idle recorder, a recording one, and a raw CPAL stream",
        true,
    ),
    (
        "audio-service-restart",
        "restart the OS audio service during a recording; then reopen in the same process",
        true,
    ),
    (
        "default-change",
        "change the system default input during a recording",
        true,
    ),
    (
        "format-change",
        "macOS/Windows: change the device's sample rate during a recording",
        true,
    ),
    (
        "permission",
        "what happens with microphone access denied",
        true,
    ),
    (
        "soak",
        "one recorder open for --secs (default 1800), a 5 s recording every 30 s (long; no action)",
        true,
    ),
];

fn main() -> ExitCode {
    report::install_logger();
    let args: Vec<String> = env::args().skip(1).collect();
    let Some(probe) = args.first() else {
        usage();
        return ExitCode::FAILURE;
    };
    let opts = Opts::parse(&args[1..]);
    let names: Vec<&str> = match probe.as_str() {
        "all" => PROBES.iter().map(|p| p.0).collect(),
        "auto" => PROBES.iter().filter(|p| !p.2).map(|p| p.0).collect(),
        name => match PROBES.iter().find(|p| p.0 == name) {
            Some(p) => vec![p.0],
            None => {
                usage();
                return ExitCode::FAILURE;
            }
        },
    };

    if !opts.no_results {
        if let Some(path) = report::start(probe) {
            println!("results: {}", path.display());
        }
        for line in report::machine() {
            say!("{line}");
        }
        say!("permission: {:?}", permission_status());
        match list_input_devices() {
            Ok(devices) => {
                for d in devices {
                    say!(
                        "device: {}{} [{}] id={} stable={} channels={:?}{}",
                        d.name,
                        if d.is_default { " (default)" } else { "" },
                        d.backend,
                        d.id,
                        d.id_is_stable,
                        d.channels,
                        if d.is_monitor { " monitor" } else { "" }
                    );
                }
            }
            Err(e) => say!("devices: {e}"),
        }
    }

    let mut results = Vec::new();
    for name in names {
        let interactive = PROBES.iter().find(|p| p.0 == name).unwrap().2;
        if probe == "all"
            && interactive
            && !ask_yes(&format!("Run the interactive probe `{name}`?"))
        {
            results.push((name, Outcome::Info("skipped".into())));
            continue;
        }
        say!("\n=== {name} ===");
        let outcome = run(name, &opts);
        say!("{outcome}");
        results.push((name, outcome));
    }

    say!("\n=== summary ===");
    let mut failed = false;
    for (name, outcome) in &results {
        failed |= matches!(outcome, Outcome::Fail(_));
        say!("{:<22} {outcome}", name);
    }
    if failed {
        ExitCode::FAILURE
    } else {
        ExitCode::SUCCESS
    }
}

fn usage() {
    eprintln!(
        "usage: handy-recorder-probe <probe|auto|all> [--device <id>] [--secs <n>] [--take-headset]\n"
    );
    for (name, description, interactive) in PROBES {
        eprintln!(
            "  {name:<22} {description}{}",
            if *interactive { " (interactive)" } else { "" }
        );
    }
    eprintln!("  {:<22} every non-interactive probe", "auto");
    eprintln!(
        "  {:<22} everything, asking before each interactive probe",
        "all"
    );
}

#[derive(Default)]
struct Opts {
    device: Option<String>,
    secs: Option<u64>,
    /// Set for the probe's own child processes, whose output the parent
    /// records.
    no_results: bool,
}

impl Opts {
    fn parse(args: &[String]) -> Self {
        let mut opts = Opts::default();
        let mut it = args.iter();
        while let Some(arg) = it.next() {
            match arg.as_str() {
                "--device" => opts.device = it.next().cloned(),
                "--secs" => opts.secs = it.next().and_then(|s| s.parse().ok()),
                "--take-headset" => TAKE_HEADSET.store(true, Ordering::Relaxed),
                "--no-results" => opts.no_results = true,
                other => eprintln!("ignoring unknown argument {other:?}"),
            }
        }
        opts
    }
}

enum Outcome {
    Pass(String),
    Fail(String),
    Info(String),
}

impl std::fmt::Display for Outcome {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Outcome::Pass(s) => write!(f, "PASS  {s}"),
            Outcome::Fail(s) => write!(f, "FAIL  {s}"),
            Outcome::Info(s) => write!(f, "INFO  {s}"),
        }
    }
}

fn run(name: &str, opts: &Opts) -> Outcome {
    let result = match name {
        "list" => probe_list(),
        "baseline" => probe_baseline(opts),
        "warm-cycles" => probe_warm_cycles(opts),
        "two-clients" => probe_two_clients(opts),
        "second-process" => probe_second_process(opts),
        "virtual-disconnect" => probe_virtual_disconnect(),
        "disconnect" => probe_disconnect(opts, false),
        "bluetooth-handoff" => probe_disconnect(opts, true),
        "meeting-app" => probe_meeting_app(opts),
        "sleep" => probe_sleep(opts),
        "audio-service-restart" => probe_service_restart(opts),
        "default-change" => probe_default_change(),
        "format-change" => probe_format_change(opts),
        "permission" => probe_permission(opts),
        "soak" => probe_soak(opts),
        _ => unreachable!(),
    };
    result.unwrap_or_else(|error| Outcome::Fail(format!("unexpected error: {error}")))
}

// ---- helpers --------------------------------------------------------------

/// Measures what it receives: level over time and first-chunk latency.
struct ProbeSink {
    started: Instant,
    frames: usize,
    peak: f32,
    sum_sq: f64,
    first_chunk_after: Option<Duration>,
    /// Peak per 100 ms of audio, to see where audio stopped or went silent.
    timeline: Vec<f32>,
    ready: Option<mpsc::Sender<()>>,
}

impl ProbeSink {
    fn new() -> Self {
        Self {
            started: Instant::now(),
            frames: 0,
            peak: 0.0,
            sum_sq: 0.0,
            first_chunk_after: None,
            timeline: Vec::new(),
            ready: None,
        }
    }

    fn seconds(&self) -> f64 {
        self.frames as f64 / RATE
    }

    fn dbfs(&self) -> f64 {
        if self.frames == 0 {
            return f64::NEG_INFINITY;
        }
        let rms = (self.sum_sq / self.frames as f64).sqrt();
        20.0 * rms.max(1e-12).log10()
    }

    /// Seconds of exact digital silence at the end: the stale-stream symptom.
    fn trailing_zero_seconds(&self) -> f64 {
        self.timeline
            .iter()
            .rev()
            .take_while(|&&p| p == 0.0)
            .count() as f64
            * 0.1
    }

    /// The longest run of exact digital silence anywhere, in seconds.
    fn longest_zero_seconds(&self) -> f64 {
        let mut longest = 0;
        let mut run = 0;
        for &p in &self.timeline {
            run = if p == 0.0 { run + 1 } else { 0 };
            longest = longest.max(run);
        }
        longest as f64 * 0.1
    }

    fn describe(&self) -> String {
        format!(
            "{:.2} s of audio, {:.1} dBFS, peak {:.4}, first chunk after {:?}",
            self.seconds(),
            self.dbfs(),
            self.peak,
            self.first_chunk_after.unwrap_or_default()
        )
    }
}

impl Sink for ProbeSink {
    fn process_chunk(&mut self, chunk: AudioChunk<'_>) {
        if self.first_chunk_after.is_none() {
            self.first_chunk_after = Some(self.started.elapsed());
            if let Some(ready) = self.ready.take() {
                let _ = ready.send(());
            }
        }
        let real = &chunk.samples[..chunk.valid_frames * chunk.channels as usize];
        for &s in real {
            if self.frames.is_multiple_of(1600) {
                self.timeline.push(0.0);
            }
            let bucket = self.timeline.last_mut().unwrap();
            *bucket = bucket.max(s.abs());
            self.peak = self.peak.max(s.abs());
            self.sum_sq += (s as f64) * (s as f64);
            self.frames += 1;
        }
    }
}

struct Opened {
    recorder: Recorder<ProbeSink>,
    failures: mpsc::Receiver<(Instant, Error)>,
}

impl Opened {
    fn failure(&self) -> Option<Error> {
        self.failures.try_recv().ok().map(|(_, e)| e)
    }
}

/// `--take-headset`: every recorder this process opens sets
/// `RecorderConfig::take_headset`.
static TAKE_HEADSET: AtomicBool = AtomicBool::new(false);

fn open(device: Option<&str>) -> Result<Opened, Error> {
    let (tx, failures) = mpsc::channel();
    let started = Instant::now();
    let recorder = Recorder::open_with_failure_handler(
        RecorderConfig {
            device: device.map(str::to_owned),
            take_headset: TAKE_HEADSET.load(Ordering::Relaxed),
            ..RecorderConfig::speech()
        },
        move |error| {
            let _ = tx.send((Instant::now(), error));
        },
    )?;
    let info = recorder.info();
    say!(
        "opened {} [{}] in {:?}: device {} Hz {} ch, delivering {} Hz {} ch",
        info.device.name,
        info.device.id,
        started.elapsed(),
        info.device_format.sample_rate,
        info.device_format.channels,
        info.format.sample_rate,
        info.format.channels
    );
    Ok(Opened { recorder, failures })
}

/// Opens, or says why not: for the extra recorders a probe can do without
/// (raw ALSA devices are exclusive).
fn try_open(device: Option<&str>, role: &str) -> Option<Opened> {
    open(device).inspect_err(|e| say!("  no {role}: {e}")).ok()
}

/// Opens a new recorder on `device`, retrying for 15 s (a replugged device
/// or a restarted service takes a moment to return), and records 2 s.
/// Leading digital silence is tolerated: some devices deliver zeros for a
/// second or two after a replug.
fn reopen(device: Option<&str>) -> (bool, String) {
    let deadline = Instant::now() + Duration::from_secs(15);
    let opened = loop {
        match open(device) {
            Ok(opened) => break opened,
            Err(e) if Instant::now() < deadline => {
                say!("  reopen failed ({e}); retrying");
                thread::sleep(Duration::from_secs(1));
            }
            Err(e) => return (false, format!("could not reopen: {e}")),
        }
    };
    let stopped = record(&opened, 2.0);
    judge(stopped, opened.failure(), 1.8, f64::INFINITY)
}

fn record(opened: &Opened, secs: f64) -> Result<Stopped<ProbeSink>, Error> {
    opened.recorder.start(ProbeSink::new())?;
    thread::sleep(Duration::from_secs_f64(secs));
    opened.recorder.stop()
}

/// Records `secs` and judges it strictly.
fn check(opened: &Opened, secs: f64) -> (bool, String) {
    let stopped = record(opened, secs);
    judge(stopped, opened.failure(), secs * 0.9, 0.5)
}

/// Starts a recording and waits up to 15 s for its first audio.
fn start_live(opened: &Opened) -> Result<bool, Error> {
    let (tx, rx) = mpsc::channel();
    let sink = ProbeSink {
        ready: Some(tx),
        ..ProbeSink::new()
    };
    opened.recorder.start(sink).map_err(Error::from)?;
    Ok(rx.recv_timeout(Duration::from_secs(15)).is_ok())
}

/// A recording is good when it is complete, at least `min_secs` long, has
/// real audio with no run of digital silence as long as `max_gap`, and the
/// recorder did not fail.
fn judge(
    stopped: Result<Stopped<ProbeSink>, Error>,
    failure: Option<Error>,
    min_secs: f64,
    max_gap: f64,
) -> (bool, String) {
    let s = match stopped {
        Ok(s) => s,
        Err(e) => return (false, format!("error: {e}")),
    };
    let gap = s.sink.longest_zero_seconds();
    let mut line = s.sink.describe();
    if gap >= 0.5 {
        line += &format!(", {gap:.1} s of digital silence");
    }
    if s.sink.peak == 0.0 {
        line += ", all digital silence";
    }
    if let Some(e) = end_error(&s).or(failure.as_ref()) {
        line += &format!(", recorder failed: {e}");
    }
    if s.dropped_frames > 0 {
        line += &format!(", {} frames dropped", s.dropped_frames);
    }
    let good = s.is_complete()
        && failure.is_none()
        && s.sink.peak > 0.0
        && gap < max_gap
        && s.sink.seconds() >= min_secs;
    (good, line)
}

fn end_error(stopped: &Stopped<ProbeSink>) -> Option<&Error> {
    match &stopped.end_reason {
        EndReason::RecorderFailed(error) => Some(error),
        _ => None,
    }
}

/// A probe's results, one line per step; the probe fails if any step did.
#[derive(Default)]
struct Steps {
    lines: Vec<String>,
    failed: bool,
}

impl Steps {
    fn add(&mut self, label: &str, (good, line): (bool, String)) {
        say!("  {label}: {line}");
        self.failed |= !good;
        self.lines.push(format!(
            "{label}: {}{line}",
            if good { "" } else { "PROBLEM " }
        ));
    }

    fn outcome(self) -> Outcome {
        let summary = self.lines.join("; ");
        if self.failed {
            Outcome::Fail(summary)
        } else {
            Outcome::Pass(summary)
        }
    }
}

fn prompt(message: &str) {
    print!("\n>>> {message}\n    Press Enter to continue. ");
    let _ = io::stdout().flush();
    report::append(&format!("\n>>> {message}"));
    let _ = io::stdin().lock().lines().next();
    report::append(&format!(
        "[{} operator pressed Enter]",
        report::timestamp(false)
    ));
}

fn read_line() -> String {
    io::stdin()
        .lock()
        .lines()
        .next()
        .and_then(Result::ok)
        .unwrap_or_default()
}

fn ask_yes(question: &str) -> bool {
    print!("{question} [Y/n] ");
    let _ = io::stdout().flush();
    let yes = !read_line().trim().eq_ignore_ascii_case("n");
    report::append(&format!("{question} -> {}", if yes { "yes" } else { "no" }));
    yes
}

/// The device to probe: --device, or a choice from the list.
fn choose_device(opts: &Opts, purpose: &str) -> Result<Option<String>, Error> {
    if opts.device.is_some() {
        return Ok(opts.device.clone());
    }
    let devices = list_input_devices()?;
    say!("Input devices ({purpose}):");
    for (i, d) in devices.iter().enumerate() {
        say!(
            "  {i}) {}{} [{}]",
            d.name,
            if d.is_default { " (default)" } else { "" },
            d.id
        );
    }
    print!("Choose a number, or Enter for the default: ");
    let _ = io::stdout().flush();
    let line = read_line();
    report::append(&format!("chose: {:?}", line.trim()));
    Ok(line
        .trim()
        .parse::<usize>()
        .ok()
        .and_then(|i| devices.get(i))
        .map(|d| d.id.clone()))
}

/// Per-platform wording for operator actions.
fn how(action: &str) -> &'static str {
    match (action, env::consts::OS) {
        ("sleep", "macos") => "Apple menu > Sleep, or close the lid",
        ("sleep", "windows") => "Start > Power > Sleep",
        ("sleep", _) => "`systemctl suspend`",
        ("default", "macos") => "System Settings > Sound > Input",
        ("default", "windows") => "Settings > System > Sound > Input, choose another device",
        ("default", _) => {
            "your desktop's sound settings, or `pactl set-default-source <name>` (`pactl list short sources`)"
        }
        ("permission", "macos") => {
            "System Settings > Privacy & Security > Microphone, turn off this terminal"
        }
        ("permission", "windows") => {
            "Settings > Privacy & security > Microphone: turn off \"Microphone access\", \"Let apps access your microphone\", or \"Let desktop apps access your microphone\" (try each alone)"
        }
        ("permission", _) => {
            "Linux has no per-app microphone permission outside sandboxes (Flatpak portals); skip this probe"
        }
        ("format", "macos") => {
            "Audio MIDI Setup (Applications > Utilities), select the device, change Format"
        }
        ("format", "windows") => {
            "Control Panel > Sound (`mmsys.cpl`) > Recording > the device > Properties > Advanced > Default Format, then Apply"
        }
        ("service", "macos") => "in another terminal: `sudo killall coreaudiod`",
        ("service", "windows") => {
            "in an administrator PowerShell: `Restart-Service audiosrv -Force`"
        }
        ("service", _) => {
            "PipeWire: `systemctl --user restart pipewire pipewire-pulse wireplumber`; PulseAudio: `pulseaudio -k`"
        }
        _ => "",
    }
}

/// Starts `baseline` in a new process: a fresh CoreAudio/WASAPI/PulseAudio
/// client, as when an application starts.
fn spawn_baseline(device: Option<&str>, secs: u64) -> io::Result<Child> {
    let mut child = std::process::Command::new(env::current_exe()?);
    child.args(["baseline", "--no-results", "--secs", &secs.to_string()]);
    if let Some(device) = device {
        child.args(["--device", device]);
    }
    child.stdout(Stdio::piped()).spawn()
}

/// Whether a `spawn_baseline` process passed, and its result line with the
/// device format it opened.
fn baseline_result(child: io::Result<Child>) -> (bool, String) {
    let output = match child.and_then(Child::wait_with_output) {
        Ok(output) => output,
        Err(e) => return (false, format!("could not run a new process: {e}")),
    };
    let text = String::from_utf8_lossy(&output.stdout);
    let result = text
        .lines()
        .find(|l| l.starts_with("PASS") || l.starts_with("FAIL"))
        .or_else(|| text.lines().rev().find(|l| !l.trim().is_empty()))
        .unwrap_or("no output")
        .to_owned();
    let format = text
        .lines()
        .find_map(|l| {
            l.split_once(": device ")
                .map(|(_, f)| format!(" (device {f})"))
        })
        .unwrap_or_default();
    (result.starts_with("PASS"), format!("{result}{format}"))
}

fn is_device_failure(kind: ErrorKind) -> bool {
    matches!(
        kind,
        ErrorKind::DeviceLost
            | ErrorKind::StreamInvalidated
            | ErrorKind::Stalled
            | ErrorKind::Backend
    )
}

/// A failed recorder must refuse the next `start` with its failure.
fn refuses_start(opened: &Opened, kind: ErrorKind) -> Option<String> {
    match opened.recorder.start(ProbeSink::new()) {
        Err(e) if e.error.kind() == kind => None,
        Err(e) => Some(format!("start returned a different error: {}", e.error)),
        Ok(()) => {
            let _ = opened.recorder.stop();
            Some("start succeeded on a failed recorder".into())
        }
    }
}

/// What `observe_loss` requires of the recorders.
#[derive(Clone, Copy)]
enum Report {
    /// A device failure, or real audio throughout.
    Optional,
    /// A device failure.
    Required,
    /// This failure.
    Exactly(ErrorKind),
}

/// What losing the device (unplug, handoff, service restart) did to a
/// recording recorder and, if there is one, an idle recorder on the same
/// device. A reported failure must keep the audio before it and refuse the
/// next `start`. A stream that carries on with trailing digital silence and
/// no failure (a stale stream) always fails; one that carries on with real
/// audio fails unless the report is `Optional`.
fn observe_loss(
    recording: &Opened,
    idle: Option<&Opened>,
    asked: Instant,
    wait: Duration,
    expect: Report,
    steps: &mut Steps,
) {
    let must_report = !matches!(expect, Report::Optional);
    let failure = recording.failures.recv_timeout(wait).ok();
    let stopped = recording.recorder.stop();
    let result = match (failure, stopped) {
        (Some((at, error)), Ok(s)) => {
            let mut problems = Vec::new();
            let expected = match expect {
                Report::Exactly(kind) => error.kind() == kind,
                _ => is_device_failure(error.kind()),
            };
            if !expected {
                problems.push(format!("unexpected kind {:?}", error.kind()));
            }
            if error.device().is_none() || error.elapsed().is_none() {
                problems.push("the error lacks device or elapsed context".into());
            }
            if end_error(&s).map(Error::kind) != Some(error.kind()) {
                problems.push(format!("end reason {:?} does not carry it", s.end_reason));
            }
            if s.sink.frames == 0 {
                problems.push("the audio before it was not kept".into());
            }
            problems.extend(refuses_start(recording, error.kind()));
            let line = format!(
                "{:?} after {:.1} s, {:.2} s of audio kept: {error}",
                error.kind(),
                (at - asked).as_secs_f64(),
                s.sink.seconds()
            );
            (
                problems.is_empty(),
                [vec![line], problems].concat().join(", "),
            )
        }
        (Some((_, error)), Err(e)) => (false, format!("{error}, but stop failed: {e}")),
        (None, Ok(s)) => {
            let silent = s.sink.trailing_zero_seconds();
            let line = format!(
                "no failure reported within {:.0} s; {}; trailing digital silence {silent:.1} s",
                wait.as_secs_f64(),
                s.sink.describe()
            );
            if silent >= 1.0 {
                (false, format!("stale stream: {line}"))
            } else {
                (!must_report, line)
            }
        }
        (None, Err(e)) => (false, format!("no failure reported, and stop failed: {e}")),
    };
    steps.add("recording", result);

    let Some(idle) = idle else { return };
    let wait = (asked + wait)
        .saturating_duration_since(Instant::now())
        .max(Duration::from_secs(5));
    let result = match idle.failures.recv_timeout(wait) {
        Ok((at, error)) => {
            let refused = refuses_start(idle, error.kind());
            let line = format!(
                "{:?} after {:.1} s{}",
                error.kind(),
                (at - asked).as_secs_f64(),
                refused.as_ref().map_or(String::new(), |r| format!(", {r}"))
            );
            (refused.is_none(), line)
        }
        Err(_) => {
            let (good, line) = check(idle, 1.0);
            (
                good && !must_report,
                format!("no failure reported; a recording afterwards: {line}"),
            )
        }
    };
    steps.add("idle", result);
}

// ---- probes that need no operator -----------------------------------------

fn probe_list() -> Result<Outcome, Error> {
    // The header every run prints lists the devices.
    let devices = list_input_devices()?;
    Ok(if devices.is_empty() {
        Outcome::Fail("no input devices".into())
    } else {
        Outcome::Info(format!(
            "permission {:?}, {} devices",
            permission_status(),
            devices.len()
        ))
    })
}

fn probe_baseline(opts: &Opts) -> Result<Outcome, Error> {
    let secs = opts.secs.unwrap_or(3) as f64;
    let opened = open(opts.device.as_deref())?;
    say!("recording {secs} s (speak if you like)...");
    let (good, line) = check(&opened, secs);
    opened.recorder.close()?;
    Ok(if good {
        Outcome::Pass(line)
    } else {
        Outcome::Fail(line)
    })
}

fn probe_warm_cycles(opts: &Opts) -> Result<Outcome, Error> {
    let opened = open(opts.device.as_deref())?;
    // Let a slow device start before the quick cycles.
    record(&opened, 1.0)?;
    let cycles = opts.secs.unwrap_or(30) as usize;
    let mut shortest = f64::MAX;
    let mut longest: f64 = 0.0;
    for cycle in 0..cycles {
        let hold = 0.05 + (cycle % 5) as f64 * 0.05;
        let stopped = record(&opened, hold)?;
        if !stopped.is_complete() || stopped.sink.frames == 0 {
            return Ok(Outcome::Fail(format!(
                "cycle {cycle} ({hold:.2} s): {:?}, {}",
                stopped.end_reason,
                stopped.sink.describe()
            )));
        }
        let ratio = stopped.sink.seconds() / hold;
        shortest = shortest.min(ratio);
        longest = longest.max(ratio);
    }
    if let Some(error) = opened.failure() {
        return Ok(Outcome::Fail(format!("recorder failed: {error}")));
    }
    Ok(Outcome::Pass(format!(
        "{cycles} recordings of 50-250 ms, all complete; audio/hold ratio {shortest:.2}..{longest:.2}"
    )))
}

fn probe_two_clients(opts: &Opts) -> Result<Outcome, Error> {
    let a = open(opts.device.as_deref())?;
    let b = open(opts.device.as_deref())?;
    a.recorder.start(ProbeSink::new())?;
    b.recorder.start(ProbeSink::new())?;
    thread::sleep(Duration::from_secs(2));
    let mut steps = Steps::default();
    steps.add("A", judge(a.recorder.stop(), a.failure(), 1.75, 0.5));
    steps.add("B", judge(b.recorder.stop(), b.failure(), 1.75, 0.5));
    a.recorder.close()?;
    steps.add("B after closing A", check(&b, 1.0));
    Ok(steps.outcome())
}

fn probe_second_process(opts: &Opts) -> Result<Outcome, Error> {
    let device = opts.device.as_deref();
    let child = spawn_baseline(device, 8);
    say!("another process is recording for 8 s; opening here in 3 s...");
    thread::sleep(Duration::from_secs(3));
    let mut steps = Steps::default();
    match open(device) {
        Ok(opened) => {
            steps.add("this process", check(&opened, 3.0));
            opened.recorder.close()?;
        }
        Err(e) => steps.add("this process", (false, format!("open failed: {e}"))),
    }
    steps.add("the other process", baseline_result(child));
    Ok(steps.outcome())
}

// ---- virtual disconnect ---------------------------------------------------

/// A virtual PulseAudio source the probe creates and removes: an unplug
/// without hardware. PipeWire's PulseAudio server and PulseAudio move a
/// stream whose source goes away to another source, with no error, so this
/// is the case the library must detect itself.
struct VirtualSource {
    name: String,
    module: Option<String>,
}

impl VirtualSource {
    fn create() -> Result<Self, String> {
        let name = format!("handy_probe_{}", std::process::id());
        // PipeWire has no module-null-source; a null sink of the source
        // class is its equivalent. PulseAudio has the module.
        let attempts = [
            vec![
                "module-null-sink".to_owned(),
                format!("sink_name={name}"),
                "media.class=Audio/Source/Virtual".to_owned(),
            ],
            vec![
                "module-null-source".to_owned(),
                format!("source_name={name}"),
            ],
        ];
        for args in attempts {
            let Some(module) = pactl(&["load-module"], &args)? else {
                continue;
            };
            let mut source = Self {
                name: name.clone(),
                module: Some(module),
            };
            let listed = pactl(&["list", "short", "sources"], &[])?.unwrap_or_default();
            if listed
                .lines()
                .any(|l| l.split('\t').nth(1) == Some(name.as_str()))
            {
                return Ok(source);
            }
            source.remove()?;
        }
        Err("pactl could not create a virtual source".into())
    }

    fn remove(&mut self) -> Result<(), String> {
        if let Some(module) = self.module.take() {
            pactl(&["unload-module", &module], &[])?
                .ok_or_else(|| format!("pactl could not unload module {module}"))?;
        }
        Ok(())
    }

    /// Opens a recorder on the source once the library lists it.
    fn open(&self) -> Result<Opened, String> {
        let id = format!("pulseaudio:{}", self.name);
        let deadline = Instant::now() + Duration::from_secs(5);
        while !list_input_devices()
            .map_err(|e| e.to_string())?
            .iter()
            .any(|d| d.id == id)
        {
            if Instant::now() > deadline {
                return Err(format!("{id} never appeared in list_input_devices"));
            }
            thread::sleep(Duration::from_millis(100));
        }
        open(Some(&id)).map_err(|e| format!("open {id}: {e}"))
    }
}

impl Drop for VirtualSource {
    fn drop(&mut self) {
        let _ = self.remove();
    }
}

/// Runs pactl. `Ok(None)` when it ran and failed; `Err` when it could not
/// run at all.
fn pactl(command: &[&str], args: &[String]) -> Result<Option<String>, String> {
    let output = std::process::Command::new("pactl")
        .args(command)
        .args(args)
        .output()
        .map_err(|e| format!("cannot run pactl: {e}"))?;
    Ok(output
        .status
        .success()
        .then(|| String::from_utf8_lossy(&output.stdout).trim().to_owned()))
}

fn probe_virtual_disconnect() -> Result<Outcome, Error> {
    if env::consts::OS != "linux" {
        return Ok(Outcome::Info("Linux only (PulseAudio or PipeWire)".into()));
    }
    let backend = list_input_devices()?.first().map(|d| d.backend.clone());
    if backend.as_deref() != Some("PulseAudio") {
        return Ok(Outcome::Info(format!(
            "needs the PulseAudio host; this process uses {backend:?}"
        )));
    }
    // A pactl problem is the environment's, not the library's.
    let mut source = match VirtualSource::create() {
        Ok(source) => source,
        Err(e) => return Ok(Outcome::Info(e)),
    };
    let recording = match source.open() {
        Ok(opened) => opened,
        Err(e) => return Ok(Outcome::Fail(e)),
    };
    let mut steps = Steps::default();
    let idle = source
        .open()
        .inspect_err(|e| steps.add("idle", (false, e.clone())))
        .ok();
    if !start_live(&recording)? {
        return Ok(Outcome::Fail("no audio from the virtual source".into()));
    }
    thread::sleep(Duration::from_millis(500));
    let removed = Instant::now();
    if let Err(e) = source.remove() {
        return Ok(Outcome::Info(e));
    }
    observe_loss(
        &recording,
        idle.as_ref(),
        removed,
        Duration::from_secs(10),
        Report::Exactly(ErrorKind::DeviceLost),
        &mut steps,
    );
    Ok(steps.outcome())
}

// ---- probes that need an operator -----------------------------------------

/// The device going away under a recording and an idle recorder: unplugged
/// or turned off, or (`handoff`) a headset shared with a phone moving to the
/// phone. Then the device comes back and a new recorder must work.
fn probe_disconnect(opts: &Opts, handoff: bool) -> Result<Outcome, Error> {
    let device = choose_device(
        opts,
        if handoff {
            "choose the Bluetooth headset"
        } else {
            "choose the device you will disconnect"
        },
    )?;
    if handoff {
        prompt(
            "Make sure the headset is connected to this computer and worn (play a moment of audio here if needed).",
        );
    }
    let recording = open(device.as_deref())?;
    let name = recording.recorder.info().device.name.clone();
    let idle = try_open(device.as_deref(), "idle recorder");
    if !start_live(&recording)? {
        return Ok(Outcome::Fail("no audio before the disconnect".into()));
    }
    thread::sleep(Duration::from_secs(1));
    if handoff {
        say!(
            "\n>>> Move {name} to your phone NOW (play something on the phone, or pick it there), keep speaking."
        );
    } else {
        say!(
            "\n>>> Disconnect {name} NOW (unplug it, turn it off, put AirPods in their case, or disable it in the system's sound settings)."
        );
    }
    say!(
        "    Waiting up to {} s for the recorder to report it...",
        ACTION_WAIT.as_secs()
    );
    let mut steps = Steps::default();
    observe_loss(
        &recording,
        idle.as_ref(),
        Instant::now(),
        ACTION_WAIT,
        if handoff {
            Report::Optional
        } else {
            Report::Required
        },
        &mut steps,
    );
    drop((recording, idle));
    prompt(&if handoff {
        format!(
            "Bring {name} back to this computer (select it in the sound settings), then continue."
        )
    } else {
        format!("Reconnect {name}, wait until the system shows it, then continue. Keep speaking.")
    });
    steps.add(
        "after reconnecting, a new recorder",
        reopen(device.as_deref()),
    );
    Ok(steps.outcome())
}

/// A recorder kept open across a probe the way an application keeps one;
/// reopened if it fails.
struct Warm {
    device: Option<String>,
    opened: Option<Opened>,
}

impl Warm {
    fn record(&mut self, secs: f64, during: Option<&str>) -> Result<(bool, String), Error> {
        if self.opened.is_none() {
            say!("  reopening a recorder after its failure");
            self.opened = Some(open(self.device.as_deref())?);
        }
        let opened = self.opened.as_ref().unwrap();
        let stopped = match opened.recorder.start(ProbeSink::new()) {
            Err(e) => Err(e.error),
            Ok(()) => {
                if let Some(action) = during {
                    prompt(action);
                }
                thread::sleep(Duration::from_secs_f64(secs));
                opened.recorder.stop()
            }
        };
        let failure = opened.failure();
        if failure.is_some() {
            self.opened = None;
        }
        Ok(judge(stopped, failure, secs - 0.3, 0.5))
    }
}

/// Opens a new recorder and records 3 s.
fn new_recorder(device: Option<&str>) -> (bool, String) {
    match open(device) {
        Ok(opened) => {
            let rate = opened.recorder.info().device_format.sample_rate;
            let (good, line) = check(&opened, 3.0);
            (good, format!("{line} (device at {rate} Hz)"))
        }
        Err(e) => (false, format!("open failed: {e}")),
    }
}

/// Two recorders kept open across a real call: A records while the call
/// starts and while it ends; B stays idle through both. Plus a new process
/// and a new recorder during the call, and a new recorder after it.
fn probe_meeting_app(opts: &Opts) -> Result<Outcome, Error> {
    let device = choose_device(opts, "choose the microphone the call will use")?;
    let mut a = Warm {
        device: device.clone(),
        opened: Some(open(device.as_deref())?),
    };
    let mut b = Warm {
        device: device.clone(),
        opened: Some(open(device.as_deref())?),
    };
    prompt(
        "Get a call ready in Google Meet (or Zoom, Teams, FaceTime) using this microphone, but do not start it \
         and do not open its preview screen yet (the preview already uses the microphone). In Meet, \
         \"New meeting > Start an instant meeting\" skips the preview; check its microphone afterwards. \
         Keep talking during every recording from here on.",
    );
    let mut steps = Steps::default();
    steps.add("before the call, A", a.record(3.0, None)?);
    steps.add("before the call, B", b.record(3.0, None)?);
    steps.add(
        "call started while A records",
        a.record(
            3.0,
            Some(
                "Start the call now. Continue once you are in it and the call app's microphone meter moves when you talk.",
            ),
        )?,
    );
    // The case that matters most: an app started while the call holds the
    // microphone.
    steps.add(
        "in the call, new process",
        baseline_result(spawn_baseline(device.as_deref(), 5)),
    );
    steps.add("in the call, A", a.record(2.0, None)?);
    steps.add(
        "in the call, B (idle as the call started)",
        b.record(2.0, None)?,
    );
    steps.add("in the call, new recorder", new_recorder(device.as_deref()));
    let call_ok = ask_yes(
        "Did the call keep hearing you the whole time (meter moving, no mic warning or dropout)?",
    );
    steps.add(
        "call app",
        (call_ok, if call_ok { "ok" } else { "disrupted" }.into()),
    );
    steps.add(
        "call ended while A records",
        a.record(
            3.0,
            Some("Leave the call now, keep talking, then continue."),
        )?,
    );
    steps.add("after the call, A", a.record(3.0, None)?);
    steps.add(
        "after the call, B (idle as the call ended)",
        b.record(3.0, None)?,
    );
    steps.add(
        "after the call, new recorder",
        new_recorder(device.as_deref()),
    );
    Ok(steps.outcome())
}

/// One sleep, three observers: an idle recorder must survive it (no watchdog
/// false positive); a recording must end with a failure (`Stalled` or a
/// device loss), never carry on with a gap; and a raw CPAL stream records
/// what the platform itself does.
fn probe_sleep(opts: &Opts) -> Result<Outcome, Error> {
    let idle = open(opts.device.as_deref())?;
    let device = idle.recorder.info().device.id.clone();
    record(&idle, 1.0)?;
    let recording = try_open(Some(device.as_str()), "recording recorder");
    let mut raw = raw::Stream::start(&device)
        .inspect_err(|e| say!("  no raw stream: {e}"))
        .ok();
    let started = SystemTime::now();
    if let Some(r) = &recording {
        r.recorder.start(ProbeSink::new()).map_err(Error::from)?;
    }
    if let Some(raw) = &mut raw {
        raw.mark();
    }
    prompt(&format!(
        "Put the machine to sleep for at least 30 s ({}), wake it, then come back here.",
        how("sleep"),
    ));
    if let Some(raw) = &mut raw {
        raw.mark();
    }
    let mut steps = Steps::default();

    if let Some(r) = recording {
        let failure = r.failure();
        let result = match r.recorder.stop() {
            Err(e) => (false, format!("stop failed: {e}")),
            Ok(s) => {
                let wall = started.elapsed().unwrap_or_default().as_secs_f64();
                let missing = wall - s.sink.seconds();
                let detail = format!(
                    "{} over {wall:.1} s of wall time ({missing:.1} s missing)",
                    s.sink.describe()
                );
                match failure.as_ref().or(end_error(&s)) {
                    Some(e) => (true, format!("ended: {e}; {detail}")),
                    None if missing > 5.0 => {
                        (false, format!("no failure reported, but a gap: {detail}"))
                    }
                    None => (true, format!("no gap: {detail}")),
                }
            }
        };
        steps.add("recording", result);
    }

    let result = match idle.failure() {
        Some(e)
            if matches!(
                e.kind(),
                ErrorKind::Stalled | ErrorKind::NoAudio | ErrorKind::SinkStalled
            ) =>
        {
            (false, format!("watchdog false positive: {e}"))
        }
        Some(e) => (true, format!("the platform reported: {e}")),
        None => check(&idle, 2.0),
    };
    steps.add("idle", result);

    if let Some(raw) = raw {
        steps.add("raw CPAL", (true, raw.finish()));
    }
    Ok(steps.outcome())
}

fn probe_service_restart(opts: &Opts) -> Result<Outcome, Error> {
    let opened = open(opts.device.as_deref())?;
    if !start_live(&opened)? {
        return Ok(Outcome::Fail("no audio before the restart".into()));
    }
    let asked = Instant::now();
    prompt(&format!(
        "Restart the audio service now ({}). Wait until it is back (a few seconds), speak, then come back here.",
        how("service")
    ));
    let mut steps = Steps::default();
    observe_loss(
        &opened,
        None,
        asked,
        Duration::from_secs(5),
        Report::Optional,
        &mut steps,
    );
    drop(opened);
    steps.add(
        "a new recorder in this process",
        reopen(opts.device.as_deref()),
    );
    Ok(steps.outcome())
}

fn probe_default_change() -> Result<Outcome, Error> {
    if list_input_devices()?.len() < 2 {
        return Ok(Outcome::Info("needs two input devices".into()));
    }
    let opened = open(None)?;
    let name = opened.recorder.info().device.name.clone();
    opened
        .recorder
        .start(ProbeSink::new())
        .map_err(Error::from)?;
    prompt(&format!(
        "Change the system default input away from {name} now ({}), then speak for a few seconds.",
        how("default")
    ));
    let failure = opened.failure();
    let audio = opened
        .recorder
        .stop()
        .map_or_else(|e| e.to_string(), |s| s.sink.describe());
    let new_default = list_input_devices()?
        .into_iter()
        .find(|d| d.is_default)
        .map(|d| d.name);
    Ok(Outcome::Info(format!(
        "recorder opened on {name}; default is now {new_default:?}; failure: {}; recording: {audio}",
        failure.map_or("none".into(), |e| e.to_string())
    )))
}

/// The device's sample rate changed under a running stream. The library
/// resamples from the rate it saw at open, so a stream that carried on at
/// the new rate without a report would deliver audio at the wrong speed.
/// It must fail the recorder, or the platform must convert.
fn probe_format_change(opts: &Opts) -> Result<Outcome, Error> {
    if !matches!(env::consts::OS, "macos" | "windows") {
        return Ok(Outcome::Info(
            "macOS and Windows only: on Linux the sound server converts, and a raw ALSA device's rate cannot change while it is open"
                .into(),
        ));
    }
    let device = choose_device(opts, "choose the device whose sample rate you will change")?;
    let opened = open(device.as_deref())?;
    let info = opened.recorder.info().clone();
    let before = info.device_format.sample_rate;
    if !start_live(&opened)? {
        return Ok(Outcome::Fail("no audio before the change".into()));
    }
    prompt(&format!(
        "Change {}'s sample rate away from {before} Hz now ({}), then come back here.",
        info.device.name,
        how("format")
    ));
    let first = opened.recorder.stop();
    let reported = opened
        .failure()
        .or_else(|| first.as_ref().ok().and_then(end_error).cloned());
    let mut steps = Steps::default();
    match &reported {
        Some(e) => steps.add("recording", (true, format!("reported {:?}: {e}", e.kind()))),
        // Not reported: a recording afterwards must run at the right speed.
        None => {
            let started = Instant::now();
            let result = match opened.recorder.start(ProbeSink::new()) {
                Err(e) => (true, format!("start refused: {}", e.error)),
                Ok(()) => {
                    thread::sleep(Duration::from_secs(5));
                    let wall = started.elapsed().as_secs_f64();
                    match opened.recorder.stop() {
                        Ok(s) => {
                            let ratio = s.sink.seconds() / wall;
                            (
                                s.is_complete() && (0.97..=1.05).contains(&ratio),
                                format!(
                                    "no failure reported; {:.2} s of audio over {wall:.2} s ({ratio:.3}x), {:?}",
                                    s.sink.seconds(),
                                    s.end_reason
                                ),
                            )
                        }
                        Err(e) => (false, format!("stop failed: {e}")),
                    }
                }
            };
            steps.add("a recording after the change", result);
        }
    }
    drop(opened);

    let result = match open(device.as_deref()) {
        Ok(reopened) => {
            let after = reopened.recorder.info().device_format.sample_rate;
            let (good, line) = check(&reopened, 2.0);
            let changed = if after == before {
                " (was the rate changed?)"
            } else {
                ""
            };
            (
                good,
                format!("{before} Hz before, {after} Hz now{changed}; {line}"),
            )
        }
        Err(e) => (false, format!("open failed: {e}")),
    };
    steps.add("a new recorder", result);
    say!("\n>>> Set the device back to {before} Hz when you are done.");
    Ok(steps.outcome())
}

fn probe_permission(opts: &Opts) -> Result<Outcome, Error> {
    if permission_status() != Permission::Denied {
        prompt(&format!(
            "Revoke microphone access now ({}), or press Enter to run with the current status.",
            how("permission")
        ));
    }
    // Read after the prompt: the operator may have changed the setting.
    let status = permission_status();
    Ok(Outcome::Info(match open(opts.device.as_deref()) {
        Err(e) => format!("status {status:?}; open failed: {e} (kind {:?})", e.kind()),
        Ok(opened) => {
            let (_, line) = check(&opened, 2.0);
            format!("status {status:?}; open succeeded; recording: {line}")
        }
    }))
}

/// One recorder kept open the way an application keeps it: for a long
/// time, recording now and then. Catches what short probes cannot: clock
/// drift between the device and the resampler, memory growth, and watchdog
/// trips on a healthy device.
fn probe_soak(opts: &Opts) -> Result<Outcome, Error> {
    const RECORD: Duration = Duration::from_secs(5);
    const EVERY: Duration = Duration::from_secs(30);
    let total = Duration::from_secs(opts.secs.unwrap_or(1800));
    let opened = open(opts.device.as_deref())?;
    let rss_start = rss_mb();
    let started = Instant::now();
    let (mut count, mut audio, mut wall) = (0u32, 0.0f64, 0.0f64);
    let (mut lowest, mut highest) = (f64::MAX, 0.0f64);
    let mut problems = Vec::new();
    say!(
        "recording {} s every {} s for {} min...",
        RECORD.as_secs(),
        EVERY.as_secs(),
        total.as_secs() / 60
    );
    while started.elapsed() < total && problems.is_empty() {
        let cycle = Instant::now();
        let stopped = record(&opened, RECORD.as_secs_f64());
        let took = cycle.elapsed().as_secs_f64();
        count += 1;
        match stopped {
            Ok(s) => {
                let ratio = s.sink.seconds() / took;
                audio += s.sink.seconds();
                wall += took;
                lowest = lowest.min(ratio);
                highest = highest.max(ratio);
                say!(
                    "  #{count} at {:.1} min: {:.3} s of audio over {took:.3} s ({ratio:.4}x), {:.1} dBFS{}",
                    started.elapsed().as_secs_f64() / 60.0,
                    s.sink.seconds(),
                    s.sink.dbfs(),
                    rss_mb().map_or(String::new(), |m| format!(", RSS {m:.1} MB"))
                );
                if !s.is_complete() {
                    problems.push(format!(
                        "recording #{count} incomplete: {:?}, {} frames dropped",
                        s.end_reason, s.dropped_frames
                    ));
                } else if !(0.97..=1.05).contains(&ratio) {
                    problems.push(format!(
                        "recording #{count}: {:.3} s of audio over {took:.3} s",
                        s.sink.seconds()
                    ));
                }
            }
            Err(e) => problems.push(format!("recording #{count}: {e}")),
        }
        if let Some(e) = opened.failure() {
            problems.push(format!("the recorder failed: {e}"));
        }
        thread::sleep(EVERY.saturating_sub(cycle.elapsed()));
    }
    let memory = match (rss_start, rss_mb()) {
        (Some(a), Some(b)) => format!("; RSS {a:.1} MB -> {b:.1} MB"),
        _ => String::new(),
    };
    let summary = format!(
        "{count} recordings over {:.1} min; audio/wall {:.4} overall, {lowest:.4}..{highest:.4} per recording{memory}",
        started.elapsed().as_secs_f64() / 60.0,
        if wall > 0.0 { audio / wall } else { 0.0 }
    );
    Ok(if problems.is_empty() {
        Outcome::Pass(summary)
    } else {
        Outcome::Fail(format!("{}; {summary}", problems.join("; ")))
    })
}

/// This process's resident memory, where the platform makes it easy to
/// read (Linux).
fn rss_mb() -> Option<f64> {
    let status = std::fs::read_to_string("/proc/self/status").ok()?;
    let kb: f64 = status
        .lines()
        .find_map(|l| l.strip_prefix("VmRSS:"))?
        .trim()
        .trim_end_matches("kB")
        .trim()
        .parse()
        .ok()?;
    Some(kb / 1024.0)
}

// ---- raw CPAL ---------------------------------------------------------------

/// A CPAL stream with no library and no watchdog, for `sleep`: whether
/// callbacks stop and resume, the gaps on the wall clock and on `Instant`
/// (which excludes sleep on macOS and Linux), and any platform errors.
mod raw {
    use std::{
        sync::{
            Arc, Mutex,
            atomic::{AtomicBool, AtomicU32, AtomicU64, Ordering},
        },
        thread::{self, JoinHandle},
        time::{Duration, Instant, SystemTime},
    };

    use cpal::traits::{DeviceTrait, HostTrait, StreamTrait};

    #[derive(Default)]
    struct Shared {
        callbacks: AtomicU64,
        /// Peak |sample| since the last reset, as f32 bits (order-preserving
        /// for non-negative floats).
        peak: AtomicU32,
        errors: Mutex<Vec<String>>,
        gaps: Mutex<Vec<String>>,
        done: AtomicBool,
    }

    pub struct Stream {
        name: String,
        _stream: cpal::Stream,
        shared: Arc<Shared>,
        monitor: JoinHandle<()>,
        /// Callback counts at each `mark`, and when the last was taken.
        marks: Vec<u64>,
        last_mark: Instant,
    }

    impl Stream {
        /// On the device with library ID `id`, or the default input.
        pub fn start(id: &str) -> Result<Self, String> {
            let host = cpal::default_host();
            let device = id
                .parse::<cpal::DeviceId>()
                .ok()
                .and_then(|id| host.device_by_id(&id))
                .or_else(|| host.default_input_device())
                .ok_or("no input device")?;
            let name = device
                .description()
                .map(|d| d.name().to_owned())
                .unwrap_or_default();
            let config = device.default_input_config().map_err(|e| e.to_string())?;
            let shared = Arc::new(Shared::default());
            let stream = match config.sample_format() {
                cpal::SampleFormat::F32 => build::<f32>(&device, &config, &shared),
                cpal::SampleFormat::I16 => build::<i16>(&device, &config, &shared),
                cpal::SampleFormat::I32 => build::<i32>(&device, &config, &shared),
                cpal::SampleFormat::U8 => build::<u8>(&device, &config, &shared),
                other => return Err(format!("unsupported sample format {other:?}")),
            }
            .map_err(|e| e.to_string())?;
            stream.play().map_err(|e| e.to_string())?;
            say!("raw CPAL stream on {name}: {config:?}");
            let monitor = {
                let shared = Arc::clone(&shared);
                thread::spawn(move || watch_gaps(&shared))
            };
            Ok(Self {
                name,
                _stream: stream,
                shared,
                monitor,
                marks: Vec::new(),
                last_mark: Instant::now(),
            })
        }

        /// Notes the callback count (before sleep, then after wake) and
        /// resets the peak.
        pub fn mark(&mut self) {
            self.marks
                .push(self.shared.callbacks.load(Ordering::Relaxed));
            self.shared.peak.store(0, Ordering::Relaxed);
            self.last_mark = Instant::now();
        }

        /// Measures at least 5 s after the last mark, then stops.
        pub fn finish(self) -> String {
            thread::sleep(Duration::from_secs(5).saturating_sub(self.last_mark.elapsed()));
            let after = self.shared.callbacks.load(Ordering::Relaxed);
            let peak = f32::from_bits(self.shared.peak.load(Ordering::Relaxed));
            self.shared.done.store(true, Ordering::Relaxed);
            let _ = self.monitor.join();
            let (before, at_wake) = (self.marks[0], self.marks[1]);
            format!(
                "{}: {before} callbacks before sleep; callbacks {} after wake ({} since, peak {peak:.4}); gaps: [{}]; errors: [{}]",
                self.name,
                if after > at_wake {
                    "RESUMED"
                } else {
                    "DID NOT RESUME"
                },
                after - at_wake,
                self.shared.gaps.lock().unwrap().join("; "),
                self.shared.errors.lock().unwrap().join("; ")
            )
        }
    }

    fn build<T: cpal::SizedSample + Copy + Send + 'static>(
        device: &cpal::Device,
        config: &cpal::SupportedStreamConfig,
        shared: &Arc<Shared>,
    ) -> Result<cpal::Stream, cpal::Error>
    where
        f32: cpal::FromSample<T>,
    {
        let (data, errors) = (Arc::clone(shared), Arc::clone(shared));
        device.build_input_stream(
            config.config(),
            move |samples: &[T], _: &cpal::InputCallbackInfo| {
                data.callbacks.fetch_add(1, Ordering::Relaxed);
                let p = samples
                    .iter()
                    .fold(0.0f32, |m, &s| m.max(s.to_sample::<f32>().abs()));
                data.peak.fetch_max(p.to_bits(), Ordering::Relaxed);
            },
            move |e: cpal::Error| {
                errors.errors.lock().unwrap().push(format!(
                    "{} {:?}: {e}",
                    utc_clock(SystemTime::now()),
                    e.kind()
                ));
            },
            None,
        )
    }

    /// Records every gap of 0.5 s or more between callbacks.
    fn watch_gaps(shared: &Shared) {
        let mut last = shared.callbacks.load(Ordering::Relaxed);
        let mut last_change = (SystemTime::now(), Instant::now());
        while !shared.done.load(Ordering::Relaxed) {
            thread::sleep(Duration::from_millis(50));
            let now = shared.callbacks.load(Ordering::Relaxed);
            if now == last {
                continue;
            }
            let wall = SystemTime::now()
                .duration_since(last_change.0)
                .unwrap_or_default();
            if wall >= Duration::from_millis(500) {
                let line = format!(
                    "no callbacks from {} for {:.1} s wall / {:.1} s uptime",
                    utc_clock(last_change.0),
                    wall.as_secs_f64(),
                    last_change.1.elapsed().as_secs_f64()
                );
                say!("  gap: {line}");
                shared.gaps.lock().unwrap().push(line);
            }
            last = now;
            last_change = (SystemTime::now(), Instant::now());
        }
    }

    fn utc_clock(t: SystemTime) -> String {
        let secs = t
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap_or_default()
            .as_secs()
            % 86_400;
        format!("{:02}:{:02}:{:02}Z", secs / 3600, secs / 60 % 60, secs % 60)
    }
}
