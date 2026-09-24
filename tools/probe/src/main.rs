//! Scripted manual hardware probes for handy-recorder (DESIGN.md, testing
//! tier 3). Each probe drives the public API against a real device, asks the
//! operator to act ("disconnect now"), and prints PASS, FAIL, or INFO with
//! what it measured.
//!
//! Usage: handy-recorder-probe <probe> [--device <id>] [--secs <n>] [--recording] [--take-headset]
//! Run with no arguments for the list of probes. See README.md for what to
//! run on each platform. Every run writes `results/<time>-<os>-<probe>.txt`
//! with the machine description, all output, and the library's debug log.

#[macro_use]
mod report;

use std::{
    env,
    io::{self, BufRead, Write},
    process::ExitCode,
    sync::{
        atomic::{AtomicBool, Ordering},
        mpsc,
    },
    thread,
    time::{Duration, Instant},
};

use handy_recorder::{
    AudioChunk, EndReason, Error, ErrorKind, InputDevice, Permission, Recorder, RecorderConfig,
    Sink, Stopped, list_input_devices, permission_status,
};

const RATE: f64 = 16_000.0;
/// How long to wait for the operator's action to produce a failure.
const ACTION_WAIT: Duration = Duration::from_secs(30);

const PROBES: &[(&str, &str, bool)] = &[
    // (name, description, interactive)
    ("list", "permission status and input devices", false),
    (
        "baseline",
        "record a few seconds; check the audio is real and complete",
        false,
    ),
    (
        "warm-cycles",
        "many quick recordings on one open recorder",
        false,
    ),
    (
        "two-clients",
        "two recorders on the same device at once",
        false,
    ),
    (
        "slow-sink",
        "a sink whose first call blocks for --secs (default 3)",
        false,
    ),
    (
        "second-process",
        "another process holds the mic; this one opens later and records too",
        false,
    ),
    (
        "external-app",
        "record alongside another app (meeting app, Voice Memos), both orders",
        true,
    ),
    (
        "meeting-app",
        "a warm recorder across a real call (Meet, Zoom): joining, during, leaving, after",
        true,
    ),
    (
        "slow-start",
        "time from open to first audio (use a Bluetooth device)",
        true,
    ),
    (
        "disconnect-recording",
        "unplug/disconnect the device during a recording",
        true,
    ),
    (
        "disconnect-idle",
        "unplug/disconnect the device while the recorder is open but idle",
        true,
    ),
    (
        "bluetooth-handoff",
        "move a Bluetooth headset to another device (phone) mid-recording, then back",
        true,
    ),
    (
        "replug",
        "diagnose silence after replugging: fresh process vs this process",
        true,
    ),
    (
        "default-change",
        "change the system default input during a recording",
        true,
    ),
    (
        "sleep-wake",
        "sleep and wake the machine with a recorder open (--recording: while recording)",
        true,
    ),
    (
        "sleep-raw",
        "a raw CPAL stream (no watchdog) across sleep: gaps, errors, whether it resumes",
        true,
    ),
    (
        "audio-service-restart",
        "restart the OS audio service during a recording; then reopen in the same process",
        true,
    ),
    (
        "permission",
        "what happens with microphone access denied",
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
    let known = matches!(probe.as_str(), "all" | "auto") || PROBES.iter().any(|p| p.0 == probe);
    if known && !opts.no_results {
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
                        "device: {}{} [{}] id={} channels={:?}",
                        d.name,
                        if d.is_default { " (default)" } else { "" },
                        d.backend,
                        d.id,
                        d.channels
                    );
                }
            }
            Err(e) => say!("devices: {e}"),
        }
    }

    let names: Vec<&str> = match probe.as_str() {
        "all" => PROBES.iter().map(|p| p.0).collect(),
        "auto" => PROBES.iter().filter(|p| !p.2).map(|p| p.0).collect(),
        // Internal: the second process of `second-process`.
        "hold" => {
            let outcome = run("hold", &opts);
            say!("{outcome}");
            return ExitCode::SUCCESS;
        }
        name if PROBES.iter().any(|p| p.0 == name) => {
            vec![PROBES.iter().find(|p| p.0 == name).unwrap().0]
        }
        _ => {
            usage();
            return ExitCode::FAILURE;
        }
    };

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
        "usage: handy-recorder-probe <probe|auto|all> [--device <id>] [--secs <n>] [--recording] [--take-headset]\n"
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
    recording: bool,
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
                "--recording" => opts.recording = true,
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
        "slow-sink" => probe_slow_sink(opts),
        "second-process" => probe_second_process(opts),
        "external-app" => probe_external_app(opts),
        "meeting-app" => probe_meeting_app(opts),
        "hold" => probe_hold(opts),
        "slow-start" => probe_slow_start(opts),
        "disconnect-recording" => probe_disconnect_recording(opts),
        "disconnect-idle" => probe_disconnect_idle(opts),
        "default-change" => probe_default_change(opts),
        "replug" => probe_replug(opts),
        "bluetooth-handoff" => probe_bluetooth_handoff(opts),
        "sleep-wake" => probe_sleep_wake(opts),
        "permission" => probe_permission(opts),
        "sleep-raw" => probe_sleep_raw(opts),
        "audio-service-restart" => probe_service_restart(opts),
        _ => unreachable!(),
    };
    result.unwrap_or_else(|error| Outcome::Fail(format!("unexpected error: {error}")))
}

// ---- helpers --------------------------------------------------------------

/// Measures what it receives: level over time, first-chunk latency, and
/// optionally blocks in its first call.
struct ProbeSink {
    started: Instant,
    frames: usize,
    peak: f32,
    sum_sq: f64,
    first_chunk_after: Option<Duration>,
    /// Peak per 100 ms of audio, to see where audio stopped or went silent.
    timeline: Vec<f32>,
    block_first_call: Option<Duration>,
    ready: Option<mpsc::Sender<Duration>>,
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
            block_first_call: None,
            ready: None,
        }
    }

    fn with_ready(ready: mpsc::Sender<Duration>) -> Self {
        Self {
            ready: Some(ready),
            ..Self::new()
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
                let _ = ready.send(self.started.elapsed());
            }
            if let Some(block) = self.block_first_call {
                thread::sleep(block);
            }
        }
        let real = &chunk.samples[..chunk.valid_frames * chunk.channels as usize];
        for &s in real {
            let frame_in_bucket = self.frames % 1600;
            if frame_in_bucket == 0 {
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
    opened_at: Instant,
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
    Ok(Opened {
        recorder,
        failures,
        opened_at: Instant::now(),
    })
}

fn record(opened: &Opened, secs: f64) -> Result<Stopped<ProbeSink>, Error> {
    opened.recorder.start(ProbeSink::new())?;
    thread::sleep(Duration::from_secs_f64(secs));
    opened.recorder.stop()
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

fn ask_yes(question: &str) -> bool {
    print!("{question} [Y/n] ");
    let _ = io::stdout().flush();
    let line = io::stdin()
        .lock()
        .lines()
        .next()
        .and_then(Result::ok)
        .unwrap_or_default();
    let yes = !line.trim().eq_ignore_ascii_case("n");
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
    let line = io::stdin()
        .lock()
        .lines()
        .next()
        .and_then(Result::ok)
        .unwrap_or_default();
    report::append(&format!("chose: {:?}", line.trim()));
    Ok(line
        .trim()
        .parse::<usize>()
        .ok()
        .and_then(|i| devices.get(i))
        .map(|d| d.id.clone()))
}

fn end_error(stopped: &Stopped<ProbeSink>) -> Option<&Error> {
    match &stopped.end_reason {
        EndReason::RecorderFailed(error) => Some(error),
        _ => None,
    }
}

/// After a failure: waits for the operator to restore the device, then
/// checks a fresh recorder on the same device records real audio.
fn recover(device: Option<&str>, name: &str) -> Result<String, String> {
    prompt(&format!("Reconnect {name} now."));
    let deadline = Instant::now() + Duration::from_secs(15);
    let opened = loop {
        match open(device) {
            Ok(opened) => break opened,
            Err(e) if Instant::now() < deadline => {
                say!("  reopen failed ({e}); retrying");
                thread::sleep(Duration::from_secs(1));
            }
            Err(e) => return Err(format!("could not reopen after reconnecting: {e}")),
        }
    };
    let stopped =
        record(&opened, 2.0).map_err(|e| format!("recording after reopen failed: {e}"))?;
    if !stopped.is_complete() || stopped.sink.frames < 16_000 || stopped.sink.peak == 0.0 {
        return Err(format!(
            "recording after reopen was not healthy: {:?}, {}",
            stopped.end_reason,
            stopped.sink.describe()
        ));
    }
    Ok(format!(
        "reopened and recorded: {}",
        stopped.sink.describe()
    ))
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
            "Settings > Privacy & security > Microphone, turn off \"Let desktop apps access your microphone\""
        }
        ("permission", _) => {
            "Linux has no per-app microphone permission outside sandboxes (Flatpak portals); skip this probe"
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

fn is_device_failure(kind: ErrorKind) -> bool {
    matches!(
        kind,
        ErrorKind::DeviceLost
            | ErrorKind::StreamInvalidated
            | ErrorKind::Stalled
            | ErrorKind::Backend
    )
}

// ---- probes ---------------------------------------------------------------

fn probe_list() -> Result<Outcome, Error> {
    let permission = permission_status();
    let devices: Vec<InputDevice> = list_input_devices()?;
    for d in &devices {
        say!(
            "  {} {} [{}] id={} stable={} channels={:?}",
            if d.is_default { "*" } else { " " },
            d.name,
            d.backend,
            d.id,
            d.id_is_stable,
            d.channels
        );
    }
    if devices.is_empty() {
        return Ok(Outcome::Fail("no input devices".into()));
    }
    Ok(Outcome::Info(format!(
        "permission {permission:?}, {} devices",
        devices.len()
    )))
}

fn probe_baseline(opts: &Opts) -> Result<Outcome, Error> {
    let secs = opts.secs.unwrap_or(3) as f64;
    let opened = open(opts.device.as_deref())?;
    say!("recording {secs} s (speak if you like)...");
    let stopped = record(&opened, secs)?;
    let sink = &stopped.sink;
    let detail = sink.describe();
    if !stopped.is_complete() {
        return Ok(Outcome::Fail(format!(
            "incomplete: {:?}, dropped {}; {detail}",
            stopped.end_reason, stopped.dropped_frames
        )));
    }
    if sink.peak == 0.0 {
        return Ok(Outcome::Fail(format!(
            "exact digital silence (permission denied, or a stale stream?); {detail}"
        )));
    }
    if sink.seconds() < secs * 0.9 {
        return Ok(Outcome::Fail(format!(
            "too little audio for {secs} s; {detail}"
        )));
    }
    let gap = sink.longest_zero_seconds();
    if gap >= 0.5 {
        return Ok(Outcome::Fail(format!(
            "{gap:.1} s of exact digital silence inside the recording; {detail}"
        )));
    }
    opened.recorder.close()?;
    Ok(Outcome::Pass(detail))
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
    if let Ok((_, error)) = opened.failures.try_recv() {
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
    let sa = a.recorder.stop()?;
    let sb = b.recorder.stop()?;
    say!("  A: {}\n  B: {}", sa.sink.describe(), sb.sink.describe());
    if !sa.is_complete() || !sb.is_complete() || sa.sink.frames < 28_000 || sb.sink.frames < 28_000
    {
        return Ok(Outcome::Fail(
            "one of two simultaneous recordings was short or incomplete".into(),
        ));
    }
    a.recorder.close()?;
    let after = record(&b, 1.0)?;
    if !after.is_complete() || after.sink.frames < 14_000 || after.sink.peak == 0.0 {
        return Ok(Outcome::Fail(format!(
            "B after closing A: {:?}, {}",
            after.end_reason,
            after.sink.describe()
        )));
    }
    Ok(Outcome::Pass(
        "both recorded simultaneously; B unaffected by closing A".into(),
    ))
}

fn probe_slow_sink(opts: &Opts) -> Result<Outcome, Error> {
    let block = Duration::from_secs(opts.secs.unwrap_or(3));
    let opened = open(opts.device.as_deref())?;
    let mut sink = ProbeSink::new();
    sink.block_first_call = Some(block);
    opened.recorder.start(sink).map_err(Error::from)?;
    thread::sleep(block + Duration::from_secs(2));
    let result = opened.recorder.stop();
    let failure = opened.failures.try_recv().ok().map(|(_, e)| e);
    match result {
        Ok(stopped) => {
            let detail = format!(
                "first call blocked {block:?}: {}, dropped {} frames ({:.2} s)",
                stopped.sink.describe(),
                stopped.dropped_frames,
                stopped.dropped_frames as f64
                    / opened.recorder.info().device_format.sample_rate as f64
            );
            match failure {
                Some(e) => Ok(Outcome::Fail(format!("recorder failed: {e}; {detail}"))),
                // The ring holds 2 s; a longer block overruns, which is expected.
                None => Ok(Outcome::Pass(detail)),
            }
        }
        Err(e) if e.kind() == ErrorKind::SinkStalled => Ok(Outcome::Info(format!(
            "blocking {block:?} tripped SinkStalled (heartbeat bound): {e}"
        ))),
        Err(e) => Ok(Outcome::Fail(format!("stop failed: {e}"))),
    }
}

fn probe_slow_start(opts: &Opts) -> Result<Outcome, Error> {
    let device = choose_device(opts, "choose a Bluetooth device")?;
    prompt(
        "Make sure the device is connected but not in use by anything (AirPods: in your ears, no call or music).",
    );
    let (tx, rx) = mpsc::channel();
    let started = Instant::now();
    let opened = open(device.as_deref())?;
    let open_took = started.elapsed();
    opened
        .recorder
        .start(ProbeSink::with_ready(tx))
        .map_err(Error::from)?;
    let outcome = match rx.recv_timeout(Duration::from_secs(20)) {
        Ok(_) => {
            let first = started.elapsed();
            Outcome::Pass(format!(
                "open took {open_took:?}; first audio {first:?} after open began (NoAudio bound: 10 s)"
            ))
        }
        Err(_) => match opened.failures.try_recv() {
            Ok((_, e)) => Outcome::Fail(format!("no audio; recorder failed: {e}")),
            Err(_) => Outcome::Fail("no audio within 20 s and no failure reported".into()),
        },
    };
    let _ = opened.recorder.stop();
    Ok(outcome)
}

fn probe_disconnect_recording(opts: &Opts) -> Result<Outcome, Error> {
    let device = choose_device(opts, "choose the device you will disconnect")?;
    let opened = open(device.as_deref())?;
    let name = opened.recorder.info().device.name.clone();
    let (tx, rx) = mpsc::channel();
    opened
        .recorder
        .start(ProbeSink::with_ready(tx))
        .map_err(Error::from)?;
    if rx.recv_timeout(Duration::from_secs(15)).is_err() {
        return Ok(Outcome::Fail("no audio before the disconnect".into()));
    }
    thread::sleep(Duration::from_secs(1));
    say!(
        "\n>>> Disconnect {name} NOW (unplug it, turn it off, put AirPods in their case, or disable it in the system's sound settings)."
    );
    say!(
        "    Waiting up to {} s for the recorder to report it...",
        ACTION_WAIT.as_secs()
    );
    let asked = Instant::now();
    let failure = opened.failures.recv_timeout(ACTION_WAIT).ok();
    let stop_started = Instant::now();
    let stopped = opened.recorder.stop();
    let stop_took = stop_started.elapsed();

    let Some((at, error)) = failure else {
        let detail = match &stopped {
            Ok(s) => format!(
                "{}; trailing digital silence {:.1} s",
                s.sink.describe(),
                s.sink.trailing_zero_seconds()
            ),
            Err(e) => format!("stop: {e}"),
        };
        return Ok(Outcome::Fail(format!(
            "no failure reported within {} s of the prompt (stale stream?); {detail}",
            ACTION_WAIT.as_secs()
        )));
    };
    say!(
        "  reported {:.1} s after the prompt: {error}",
        (at - asked).as_secs_f64()
    );
    let mut problems = Vec::new();
    if !is_device_failure(error.kind()) {
        problems.push(format!("unexpected kind {:?}", error.kind()));
    }
    if error.device().is_none() || error.elapsed().is_none() {
        problems.push("error lacks device or elapsed context".into());
    }
    match &stopped {
        Ok(s) => {
            say!("  stop took {stop_took:?}: {}", s.sink.describe());
            if end_error(s).map(Error::kind) != Some(error.kind()) {
                problems.push(format!(
                    "end reason {:?} does not carry the failure",
                    s.end_reason
                ));
            }
            if s.sink.frames == 0 {
                problems.push("no audio kept from before the disconnect".into());
            }
        }
        Err(e) => problems.push(format!("stop returned Err: {e}")),
    }
    match opened.recorder.start(ProbeSink::new()) {
        Err(e) if e.error.kind() == error.kind() => {}
        Err(e) => problems.push(format!("start returned a different error: {}", e.error)),
        Ok(()) => problems.push("start succeeded on a failed recorder".into()),
    }
    drop(opened);
    let recovery = recover(device.as_deref(), &name);
    match (problems.is_empty(), recovery) {
        (true, Ok(r)) => Ok(Outcome::Pass(format!(
            "{:?} after {:.1} s; audio before it kept; {r}",
            error.kind(),
            (at - asked).as_secs_f64()
        ))),
        (_, Err(r)) => {
            problems.push(r);
            Ok(Outcome::Fail(problems.join("; ")))
        }
        (false, Ok(_)) => Ok(Outcome::Fail(problems.join("; "))),
    }
}

fn probe_disconnect_idle(opts: &Opts) -> Result<Outcome, Error> {
    let device = choose_device(opts, "choose the device you will disconnect")?;
    let opened = open(device.as_deref())?;
    let name = opened.recorder.info().device.name.clone();
    // Confirm the device works, then leave it open and idle.
    let warm = record(&opened, 1.0)?;
    if warm.sink.frames == 0 {
        return Ok(Outcome::Fail("no audio before the disconnect".into()));
    }
    say!(
        "\n>>> Disconnect {name} NOW (unplug, turn off, or disable it). The recorder is open and idle."
    );
    say!(
        "    Waiting up to {} s for the recorder to report it...",
        ACTION_WAIT.as_secs()
    );
    let asked = Instant::now();
    let Ok((at, error)) = opened.failures.recv_timeout(ACTION_WAIT) else {
        let after = record(&opened, 1.0);
        return Ok(Outcome::Fail(format!(
            "no failure reported while idle; a recording afterwards gave {:?}",
            after.map(|s| s.sink.describe())
        )));
    };
    say!(
        "  reported {:.1} s after the prompt: {error}",
        (at - asked).as_secs_f64()
    );
    let start = opened.recorder.start(ProbeSink::new());
    drop(opened);
    let recovery = recover(device.as_deref(), &name);
    match (start, recovery) {
        (Err(e), Ok(r)) if e.error.kind() == error.kind() => Ok(Outcome::Pass(format!(
            "{:?} while idle; start returned it; {r}",
            error.kind()
        ))),
        (Ok(()), _) => Ok(Outcome::Fail("start succeeded on a failed recorder".into())),
        (Err(e), Ok(_)) => Ok(Outcome::Fail(format!(
            "start returned a different error: {}",
            e.error
        ))),
        (_, Err(r)) => Ok(Outcome::Fail(r)),
    }
}

fn probe_default_change(_opts: &Opts) -> Result<Outcome, Error> {
    let devices = list_input_devices()?;
    if devices.len() < 2 {
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
    let failure = opened.failures.try_recv().ok();
    let stopped = opened.recorder.stop();
    let new_default = list_input_devices()?
        .into_iter()
        .find(|d| d.is_default)
        .map(|d| d.name);
    let audio = stopped
        .as_ref()
        .map(|s| s.sink.describe())
        .unwrap_or_else(|e| e.to_string());
    Ok(Outcome::Info(format!(
        "recorder opened on {name}; default is now {new_default:?}; failure: {}; recording: {audio}",
        failure.map_or("none".into(), |(_, e)| e.to_string())
    )))
}

fn probe_sleep_wake(opts: &Opts) -> Result<Outcome, Error> {
    let opened = open(opts.device.as_deref())?;
    record(&opened, 1.0)?;
    if opts.recording {
        opened
            .recorder
            .start(ProbeSink::new())
            .map_err(Error::from)?;
    }
    prompt(&format!(
        "Put the machine to sleep for at least 30 s ({}), wake it, then come back here. The recorder is open{}.",
        how("sleep"),
        if opts.recording {
            " and recording"
        } else {
            " and idle"
        }
    ));
    let failure = opened.failures.try_recv().ok();
    let recorded = if opts.recording {
        opened
            .recorder
            .stop()
            .map(|s| (s.sink.describe(), s.end_reason))
    } else {
        record(&opened, 2.0).map(|s| (s.sink.describe(), s.end_reason))
    };
    let slept = opened.opened_at.elapsed();
    match failure {
        None => match recorded {
            Ok((detail, EndReason::StopCalled)) => Ok(Outcome::Pass(format!(
                "no failure across sleep ({:.0} s open); {detail}",
                slept.as_secs_f64()
            ))),
            Ok((detail, reason)) => Ok(Outcome::Fail(format!("{reason:?}; {detail}"))),
            Err(e) => Ok(Outcome::Fail(format!("recording after wake failed: {e}"))),
        },
        Some((_, e))
            if matches!(
                e.kind(),
                ErrorKind::Stalled | ErrorKind::NoAudio | ErrorKind::SinkStalled
            ) =>
        {
            Ok(Outcome::Fail(format!(
                "watchdog false positive across sleep: {e}"
            )))
        }
        Some((_, e)) => Ok(Outcome::Info(format!(
            "the platform reported a failure across sleep: {e}"
        ))),
    }
}

fn probe_permission(opts: &Opts) -> Result<Outcome, Error> {
    let status = permission_status();
    say!("permission_status: {status:?}");
    if status != Permission::Denied {
        prompt(&format!(
            "To probe denial: revoke microphone access ({}), then rerun this probe. Press Enter to run with the current status anyway.",
            how("permission")
        ));
    }
    match open(opts.device.as_deref()) {
        Err(e) => Ok(Outcome::Info(format!(
            "status {status:?}; open failed: {e} (kind {:?})",
            e.kind()
        ))),
        Ok(opened) => {
            let stopped = record(&opened, 2.0);
            let failure = opened.failures.try_recv().ok().map(|(_, e)| e.to_string());
            Ok(Outcome::Info(format!(
                "status {status:?}; open succeeded; recording: {}; failure: {failure:?}",
                stopped
                    .map(|s| s.sink.describe())
                    .unwrap_or_else(|e| e.to_string())
            )))
        }
    }
}

// ---- sharing the microphone with other processes ---------------------------

/// Run by `second-process` in a child process: records for --secs and prints
/// one machine-readable line.
fn probe_hold(opts: &Opts) -> Result<Outcome, Error> {
    let secs = opts.secs.unwrap_or(8) as f64;
    let opened = open(opts.device.as_deref())?;
    let stopped = record(&opened, secs)?;
    let failure = opened.failures.try_recv().ok().map(|(_, e)| e.to_string());
    // Peak per second, to see whether audio kept flowing throughout.
    let per_second: Vec<String> = stopped
        .sink
        .timeline
        .chunks(10)
        .map(|c| format!("{:.4}", c.iter().fold(0.0f32, |m, &p| m.max(p))))
        .collect();
    say!(
        "HOLD complete={} seconds={:.2} peak={:.4} failure={:?} per_second={}",
        stopped.is_complete(),
        stopped.sink.seconds(),
        stopped.sink.peak,
        failure,
        per_second.join(",")
    );
    Ok(Outcome::Info("held".into()))
}

fn probe_second_process(opts: &Opts) -> Result<Outcome, Error> {
    let hold_secs = 8;
    let mut child = std::process::Command::new(env::current_exe().expect("own path"));
    child.args(["hold", "--no-results", "--secs", &hold_secs.to_string()]);
    if let Some(device) = &opts.device {
        child.args(["--device", device]);
    }
    let mut child = child
        .stdout(std::process::Stdio::piped())
        .spawn()
        .expect("spawn the holding process");
    say!("another process is recording for {hold_secs} s; opening here in 3 s...");
    thread::sleep(Duration::from_secs(3));

    let opened = match open(opts.device.as_deref()) {
        Ok(opened) => opened,
        Err(e) => {
            let _ = child.kill();
            let _ = child.wait();
            return Err(e);
        }
    };
    let ours = record(&opened, 3.0);
    let our_failure = opened.failures.try_recv().ok().map(|(_, e)| e);
    opened.recorder.close()?;
    say!("closed here; the other process records on");

    let output = child
        .wait_with_output()
        .expect("wait for the holding process");
    let text = String::from_utf8_lossy(&output.stdout);
    let Some(line) = text.lines().find(|l| l.starts_with("HOLD ")) else {
        return Ok(Outcome::Fail(format!(
            "the other process did not finish recording: {text}"
        )));
    };
    say!("  other process: {line}");
    let field = |name: &str| {
        line.split_whitespace()
            .find_map(|kv| kv.strip_prefix(&format!("{name}=")))
            .unwrap_or("")
            .to_owned()
    };
    let per_second: Vec<f32> = field("per_second")
        .split(',')
        .filter_map(|v| v.parse().ok())
        .collect();

    let mut problems = Vec::new();
    match &ours {
        Ok(s) => {
            say!("  this process: {}", s.sink.describe());
            if !s.is_complete() || s.sink.seconds() < 2.7 || s.sink.peak == 0.0 {
                problems.push(format!(
                    "this process: {:?}, {}",
                    s.end_reason,
                    s.sink.describe()
                ));
            }
        }
        Err(e) => problems.push(format!("this process could not record: {e}")),
    }
    if let Some(e) = our_failure {
        problems.push(format!("this process failed: {e}"));
    }
    if field("complete") != "true" || field("failure") != "None" {
        problems.push("the other process's recording was disrupted".into());
    }
    if field("seconds").parse::<f64>().unwrap_or(0.0) < hold_secs as f64 * 0.95 {
        problems.push("the other process lost audio".into());
    }
    if per_second.contains(&0.0) {
        problems.push(format!(
            "the other process got digital silence in some seconds: {per_second:?}"
        ));
    }
    Ok(if problems.is_empty() {
        Outcome::Pass(
            "both processes recorded; the other kept recording after this one opened and closed"
                .into(),
        )
    } else {
        Outcome::Fail(problems.join("; "))
    })
}

fn probe_external_app(opts: &Opts) -> Result<Outcome, Error> {
    let mut notes = Vec::new();

    // Order 1: the other app has the microphone first.
    prompt(
        "Start another app recording from the same microphone (a meeting app test call, Voice Memos, QuickTime audio recording) and leave it running.",
    );
    let opened = open(opts.device.as_deref())?;
    say!("recording here for 3 s alongside it...");
    let first = record(&opened, 3.0)?;
    let first_failure = opened.failures.try_recv().ok().map(|(_, e)| e.to_string());
    opened.recorder.close()?;
    say!("  here: {}", first.sink.describe());
    let first_ok = first.is_complete()
        && first.sink.seconds() > 2.7
        && first.sink.peak > 0.0
        && first_failure.is_none();
    let other_ok =
        ask_yes("Did the other app keep recording normally (no gap, error, or silence)?");
    notes.push(format!(
        "other app first: here {}{}; other app {}",
        if first_ok { "ok" } else { "PROBLEM" },
        first_failure.map_or(String::new(), |f| format!(" ({f})")),
        if other_ok { "ok" } else { "PROBLEM" }
    ));
    prompt("Stop the other app's recording.");

    // Order 2: this process records first; the other app joins mid-recording.
    let opened = open(opts.device.as_deref())?;
    opened
        .recorder
        .start(ProbeSink::new())
        .map_err(Error::from)?;
    prompt(
        "Recording here now. Start the other app recording from the same microphone, let it run a few seconds, then stop it.",
    );
    let second = opened.recorder.stop()?;
    let second_failure = opened.failures.try_recv().ok().map(|(_, e)| e.to_string());
    say!("  here: {}", second.sink.describe());
    let silent_gap = second
        .sink
        .timeline
        .windows(5)
        .any(|w| w.iter().all(|&p| p == 0.0));
    let second_ok =
        second.is_complete() && second.sink.peak > 0.0 && second_failure.is_none() && !silent_gap;
    notes.push(format!(
        "this first, other joined: here {}{}{}",
        if second_ok { "ok" } else { "PROBLEM" },
        second_failure.map_or(String::new(), |f| format!(" ({f})")),
        if silent_gap {
            " (0.5 s or more of digital silence)"
        } else {
            ""
        }
    ));

    let summary = notes.join("; ");
    Ok(if first_ok && other_ok && second_ok {
        Outcome::Pass(summary)
    } else {
        Outcome::Fail(summary)
    })
}

/// A warm recorder (Handy's always-on mode) held across a real call, plus
/// a new process and new recorders during the call and after it. By default the call starts while the
/// warm recorder is idle and ends while it records; `--recording` swaps
/// the two.
fn probe_meeting_app(opts: &Opts) -> Result<Outcome, Error> {
    let device = choose_device(opts, "choose the microphone the call will use")?;
    let mut warm = Some(open(device.as_deref())?);
    let name = warm.as_ref().unwrap().recorder.info().device.name.clone();
    prompt(&format!(
        "Get a call ready in Google Meet (or Zoom, Teams, FaceTime) using {name}, but do not start it \
         and do not open its preview screen yet (the preview already uses the microphone). In Meet, \
         \"New meeting > Start an instant meeting\" skips the preview; check its microphone afterwards. \
         Keep talking during every recording from here on."
    ));
    let mut steps = Vec::new();
    let mut ok = true;
    // Records on the warm recorder, reopening it first if it failed earlier.
    let mut warm_step = |label: &str,
                         secs: f64,
                         during: Option<&str>,
                         steps: &mut Vec<String>,
                         ok: &mut bool|
     -> Result<(), Error> {
        if warm.is_none() {
            say!("  reopening the warm recorder after its failure");
            warm = Some(open(device.as_deref())?);
        }
        let opened = warm.as_ref().unwrap();
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
        let failure = opened.failures.try_recv().ok().map(|(_, e)| e);
        let failed = failure.is_some();
        let (good, line) = judge(stopped, failure, secs - 0.3);
        say!("  {label}: {line}");
        *ok &= good;
        steps.push(format!(
            "{label}: {}{line}",
            if good { "" } else { "PROBLEM " }
        ));
        if failed {
            warm = None;
        }
        Ok(())
    };
    let fresh_step = |label: &str, steps: &mut Vec<String>, ok: &mut bool| -> Result<(), Error> {
        let (good, line) = match open(device.as_deref()) {
            Ok(opened) => {
                let format = opened.recorder.info().device_format.sample_rate;
                let stopped = record(&opened, 3.0);
                let failure = opened.failures.try_recv().ok().map(|(_, e)| e);
                let (good, line) = judge(stopped, failure, 2.7);
                (good, format!("{line} (device at {format} Hz)"))
            }
            Err(e) => (false, format!("open failed: {e}")),
        };
        say!("  {label}: {line}");
        *ok &= good;
        steps.push(format!(
            "{label}: {}{line}",
            if good { "" } else { "PROBLEM " }
        ));
        Ok(())
    };

    warm_step("before the call", 3.0, None, &mut steps, &mut ok)?;

    let join = "Start the call now. Continue once you are in it and the call app's microphone meter moves when you talk.";
    if opts.recording {
        warm_step(
            "call started while recording",
            3.0,
            Some(join),
            &mut steps,
            &mut ok,
        )?;
    } else {
        prompt(join);
    }

    // The case that matters most: an app started while the call holds the
    // microphone.
    let (good, line) = fresh_process(device.as_deref(), 5);
    say!("  in the call, new process: {line}");
    ok &= good;
    steps.push(format!(
        "in the call, new process: {}{line}",
        if good { "" } else { "PROBLEM " }
    ));

    for i in 1..=3 {
        warm_step(
            &format!("in the call, warm #{i}"),
            2.0,
            None,
            &mut steps,
            &mut ok,
        )?;
    }
    fresh_step("in the call, new recorder", &mut steps, &mut ok)?;
    let call_ok = ask_yes(
        "Did the call keep hearing you the whole time (meter moving, no mic warning or dropout)?",
    );
    ok &= call_ok;
    steps.push(format!(
        "call app: {}",
        if call_ok { "ok" } else { "PROBLEM" }
    ));

    let leave = "Leave the call now, keep talking, then continue.";
    if opts.recording {
        prompt(leave);
    } else {
        warm_step(
            "call ended while recording",
            3.0,
            Some(leave),
            &mut steps,
            &mut ok,
        )?;
    }

    warm_step("after the call, warm", 3.0, None, &mut steps, &mut ok)?;
    fresh_step("after the call, new recorder", &mut steps, &mut ok)?;

    let summary = steps.join("; ");
    Ok(if ok {
        Outcome::Pass(summary)
    } else {
        Outcome::Fail(summary)
    })
}

/// Runs `baseline` in a new process: a fresh CoreAudio/WASAPI/PulseAudio
/// client, as when an application starts. Returns whether it passed, and its
/// result with the device format it opened.
fn fresh_process(device: Option<&str>, secs: u64) -> (bool, String) {
    let mut child = std::process::Command::new(env::current_exe().expect("own path"));
    child.args(["baseline", "--no-results", "--secs", &secs.to_string()]);
    if let Some(device) = device {
        child.args(["--device", device]);
    }
    let output = match child.output() {
        Ok(output) => output,
        Err(e) => return (false, format!("could not start a new process: {e}")),
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
        .find_map(|l| l.split_once(": device ").map(|(_, f)| f.to_owned()))
        .map_or(String::new(), |f| format!(" (device {f})"));
    (result.starts_with("PASS"), format!("{result}{format}"))
}

/// A recording is good when it is complete, at least `min_secs` long, has
/// real audio with no 0.5 s run of digital silence, and the recorder did not
/// fail.
fn judge(
    stopped: Result<Stopped<ProbeSink>, Error>,
    failure: Option<Error>,
    min_secs: f64,
) -> (bool, String) {
    let s = match stopped {
        Ok(s) => s,
        Err(e) => return (false, format!("error: {e}")),
    };
    let gap = s.sink.longest_zero_seconds();
    let mut line = format!("{:.1} s, {:.1} dBFS", s.sink.seconds(), s.sink.dbfs());
    if gap >= 0.5 {
        line += &format!(", {gap:.1} s of digital silence");
    }
    if s.sink.peak == 0.0 {
        line += ", all digital silence";
    }
    if let Some(e) = end_error(&s) {
        line += &format!(", recorder failed: {e}");
    } else if let Some(e) = &failure {
        line += &format!(", recorder failed: {e}");
    }
    if s.dropped_frames > 0 {
        line += &format!(", {} frames dropped", s.dropped_frames);
    }
    let good = s.is_complete()
        && failure.is_none()
        && s.sink.peak > 0.0
        && gap < 0.5
        && s.sink.seconds() >= min_secs;
    (good, line)
}

fn level(stopped: &Stopped<ProbeSink>) -> String {
    format!(
        "{:.1} dBFS, peak {:.4}",
        stopped.sink.dbfs(),
        stopped.sink.peak
    )
}

fn probe_replug(opts: &Opts) -> Result<Outcome, Error> {
    let device = choose_device(opts, "choose the USB device you will replug")?;
    let opened = open(device.as_deref())?;
    let name = opened.recorder.info().device.name.clone();
    let before = record(&opened, 2.0)?;
    say!("  before unplugging: {}", level(&before));
    say!("\n>>> Unplug {name} NOW.");
    if opened.failures.recv_timeout(ACTION_WAIT).is_err() {
        return Ok(Outcome::Fail("no failure reported on unplug".into()));
    }
    drop(opened);
    prompt(&format!(
        "Plug {name} back in, wait until macOS shows it, speak or tap near it during the next recordings."
    ));

    // A fresh process: empty config cache, fresh CoreAudio state.
    let mut child = std::process::Command::new(env::current_exe().expect("own path"));
    child.args(["baseline", "--no-results", "--secs", "2"]);
    if let Some(device) = &device {
        child.args(["--device", device]);
    }
    let output = child.output().expect("run a fresh process");
    let text = String::from_utf8_lossy(&output.stdout);
    let fresh = text
        .lines()
        .find(|l| l.starts_with("PASS") || l.starts_with("FAIL"))
        .unwrap_or("no result")
        .to_owned();
    say!("  fresh process:   {fresh}");

    // This process: the config cached before the unplug.
    let mut here = Vec::new();
    for attempt in 1..=3 {
        let opened = open(device.as_deref())?;
        let stopped = record(&opened, 1.5)?;
        say!("  this process #{attempt}: {}", level(&stopped));
        here.push(stopped.sink.peak);
        drop(opened);
        thread::sleep(Duration::from_millis(500));
    }
    let fresh_ok = fresh.starts_with("PASS");
    let here_ok = here.iter().any(|&p| p > 0.0);
    Ok(Outcome::Info(format!(
        "before {}; fresh process {}; this process {} ({})",
        level(&before),
        if fresh_ok {
            "real audio"
        } else {
            "SILENT/failed"
        },
        if here_ok { "real audio" } else { "SILENT" },
        match (fresh_ok, here_ok) {
            (true, false) => "state held in this process: the config cache or CPAL",
            (false, false) => "the device or macOS needs time after replug",
            (true, true) => "not reproduced",
            (false, true) => "fresh process failed but this one worked",
        }
    )))
}

// ---- raw CPAL across sleep --------------------------------------------------

fn utc_clock(t: std::time::SystemTime) -> String {
    let secs = t
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs()
        % 86_400;
    format!("{:02}:{:02}:{:02}Z", secs / 3600, secs / 60 % 60, secs % 60)
}

fn probe_sleep_raw(opts: &Opts) -> Result<Outcome, Error> {
    use cpal::traits::{DeviceTrait, HostTrait, StreamTrait};
    use std::sync::{
        Arc, Mutex,
        atomic::{AtomicBool, AtomicU32, AtomicU64, Ordering},
    };
    use std::time::SystemTime;

    let host = cpal::default_host();
    let device = match &opts.device {
        Some(id) => id
            .parse::<cpal::DeviceId>()
            .ok()
            .and_then(|id| host.device_by_id(&id)),
        None => host.default_input_device(),
    };
    let Some(device) = device else {
        return Ok(Outcome::Fail("no such input device".into()));
    };
    let name = device
        .description()
        .map(|d| d.name().to_owned())
        .unwrap_or_default();
    let config = match device.default_input_config() {
        Ok(c) => c,
        Err(e) => return Ok(Outcome::Fail(format!("no input config: {e}"))),
    };
    say!("raw CPAL stream on {name}: {:?}", config);

    let callbacks = Arc::new(AtomicU64::new(0));
    // Peak |sample| since the last reset, as f32 bits (order-preserving for
    // non-negative floats).
    let peak = Arc::new(AtomicU32::new(0));
    let errors: Arc<Mutex<Vec<(SystemTime, String)>>> = Arc::default();

    fn build<T: cpal::SizedSample + Copy + Send + 'static>(
        device: &cpal::Device,
        config: &cpal::SupportedStreamConfig,
        callbacks: Arc<AtomicU64>,
        peak: Arc<AtomicU32>,
        errors: Arc<Mutex<Vec<(SystemTime, String)>>>,
    ) -> Result<cpal::Stream, cpal::Error>
    where
        f32: cpal::FromSample<T>,
    {
        device.build_input_stream(
            config.config(),
            move |data: &[T], _: &cpal::InputCallbackInfo| {
                callbacks.fetch_add(1, Ordering::Relaxed);
                let p = data
                    .iter()
                    .fold(0.0f32, |m, &s| m.max(s.to_sample::<f32>().abs()));
                peak.fetch_max(p.to_bits(), Ordering::Relaxed);
            },
            move |e: cpal::Error| {
                errors
                    .lock()
                    .unwrap()
                    .push((SystemTime::now(), format!("{:?}: {e}", e.kind())));
            },
            None,
        )
    }
    let (c, p, e) = (
        Arc::clone(&callbacks),
        Arc::clone(&peak),
        Arc::clone(&errors),
    );
    let stream = match config.sample_format() {
        cpal::SampleFormat::F32 => build::<f32>(&device, &config, c, p, e),
        cpal::SampleFormat::I16 => build::<i16>(&device, &config, c, p, e),
        cpal::SampleFormat::I32 => build::<i32>(&device, &config, c, p, e),
        cpal::SampleFormat::U8 => build::<u8>(&device, &config, c, p, e),
        other => {
            return Ok(Outcome::Fail(format!(
                "unsupported sample format {other:?}"
            )));
        }
    };
    let stream = match stream {
        Ok(s) => s,
        Err(e) => return Ok(Outcome::Fail(format!("build failed: {e}"))),
    };
    if let Err(e) = stream.play() {
        return Ok(Outcome::Fail(format!("play failed: {e}")));
    }

    // Record every gap of 0.5 s or more between callbacks, on the wall clock
    // (includes sleep) and on process uptime (Instant; excludes sleep on
    // macOS).
    let done = Arc::new(AtomicBool::new(false));
    let gaps: Arc<Mutex<Vec<String>>> = Arc::default();
    let monitor = {
        let (callbacks, done, gaps) =
            (Arc::clone(&callbacks), Arc::clone(&done), Arc::clone(&gaps));
        thread::spawn(move || {
            let mut last = callbacks.load(Ordering::Relaxed);
            let mut last_change = (SystemTime::now(), Instant::now());
            while !done.load(Ordering::Relaxed) {
                thread::sleep(Duration::from_millis(50));
                let now = callbacks.load(Ordering::Relaxed);
                if now != last {
                    let wall = SystemTime::now()
                        .duration_since(last_change.0)
                        .unwrap_or_default();
                    let uptime = last_change.1.elapsed();
                    if wall >= Duration::from_millis(500) {
                        let line = format!(
                            "no callbacks from {} for {:.1} s wall / {:.1} s uptime",
                            utc_clock(last_change.0),
                            wall.as_secs_f64(),
                            uptime.as_secs_f64()
                        );
                        say!("  gap: {line}");
                        gaps.lock().unwrap().push(line);
                    }
                    last = now;
                    last_change = (SystemTime::now(), Instant::now());
                }
            }
        })
    };

    thread::sleep(Duration::from_secs(1));
    let before = callbacks.load(Ordering::Relaxed);
    prompt(&format!(
        "Raw stream running. Put the machine to sleep for at least 30 s ({}), wake it, then come back here.",
        how("sleep")
    ));
    let at_return = callbacks.load(Ordering::Relaxed);
    peak.store(0, Ordering::Relaxed);
    say!("measuring 5 s after wake...");
    thread::sleep(Duration::from_secs(5));
    let after = callbacks.load(Ordering::Relaxed);
    let peak_after = f32::from_bits(peak.load(Ordering::Relaxed));
    done.store(true, Ordering::Relaxed);
    let _ = monitor.join();
    drop(stream);

    let resumed = after > at_return;
    let errors: Vec<String> = errors
        .lock()
        .unwrap()
        .iter()
        .map(|(t, e)| format!("{} {e}", utc_clock(*t)))
        .collect();
    let gaps = gaps.lock().unwrap().clone();
    Ok(Outcome::Info(format!(
        "{name}: {before} callbacks before sleep; callbacks {} after wake ({} in the 5 s measured, peak {peak_after:.4}); gaps: [{}]; errors: [{}]",
        if resumed { "RESUMED" } else { "DID NOT RESUME" },
        after - at_return,
        gaps.join("; "),
        errors.join("; ")
    )))
}

// ---- audio service restart ------------------------------------------------

/// The sound server or audio service restarting: coreaudiod, audiosrv,
/// PipeWire/PulseAudio. The recorder should report it (or keep going with
/// real audio), and a new recorder in the same process should work: the
/// library keeps one audio host per process (TODO.md, "PulseAudio server
/// restarts").
fn probe_service_restart(opts: &Opts) -> Result<Outcome, Error> {
    let opened = open(opts.device.as_deref())?;
    let (tx, rx) = mpsc::channel();
    opened
        .recorder
        .start(ProbeSink::with_ready(tx))
        .map_err(Error::from)?;
    if rx.recv_timeout(Duration::from_secs(15)).is_err() {
        return Ok(Outcome::Fail("no audio before the restart".into()));
    }
    prompt(&format!(
        "Restart the audio service now ({}). Wait until it is back (a few seconds), speak, then come back here.",
        how("service")
    ));
    let failure = opened
        .failures
        .recv_timeout(Duration::from_secs(5))
        .ok()
        .map(|(_, e)| e);
    let stopped = opened.recorder.stop();
    let mut notes = Vec::new();
    let mut problems = Vec::new();
    match (&failure, &stopped) {
        (Some(e), Ok(s)) => {
            notes.push(format!(
                "reported {:?}: {e}; kept {}",
                e.kind(),
                s.sink.describe()
            ));
            if end_error(s).is_none() {
                problems.push("the recording's end reason does not carry the failure".into());
            }
        }
        (None, Ok(s)) => {
            let silent = s.sink.trailing_zero_seconds();
            notes.push(format!(
                "no failure reported; the stream kept running: {}, trailing digital silence {silent:.1} s",
                s.sink.describe()
            ));
            if silent >= 1.0 {
                problems
                    .push("stale stream: no failure, and digital silence after the restart".into());
            }
        }
        (_, Err(e)) => problems.push(format!("stop failed: {e}")),
    }
    drop(opened);

    // A new recorder in this process, on the same audio host.
    let deadline = Instant::now() + Duration::from_secs(15);
    let reopened = loop {
        match open(opts.device.as_deref()) {
            Ok(opened) => break Some(opened),
            Err(e) if Instant::now() < deadline => {
                say!("  reopen failed ({e}); retrying");
                thread::sleep(Duration::from_secs(1));
            }
            Err(e) => {
                problems.push(format!("could not reopen in this process: {e}"));
                break None;
            }
        }
    };
    if let Some(opened) = reopened {
        match record(&opened, 2.0) {
            Ok(s) if s.is_complete() && s.sink.peak > 0.0 => {
                notes.push(format!("reopened in this process: {}", s.sink.describe()))
            }
            Ok(s) => problems.push(format!(
                "reopened, but: {:?}, {}",
                s.end_reason,
                s.sink.describe()
            )),
            Err(e) => problems.push(format!("reopened, but recording failed: {e}")),
        }
    }
    let summary = notes.join("; ");
    Ok(if problems.is_empty() {
        Outcome::Pass(summary)
    } else {
        Outcome::Fail(format!("{}; {summary}", problems.join("; ")))
    })
}

// ---- Bluetooth handoff ------------------------------------------------------

/// A headset shared between devices (AirPods on an Apple account) moving to
/// the phone mid-recording. The OS and headset negotiate this; the library
/// must report it: a device loss, or at least not digital silence posing as
/// a working stream.
fn probe_bluetooth_handoff(opts: &Opts) -> Result<Outcome, Error> {
    let device = choose_device(opts, "choose the Bluetooth headset")?;
    prompt(
        "Make sure the headset is connected to this computer and worn (play a moment of audio here if needed).",
    );
    let opened = open(device.as_deref())?;
    let name = opened.recorder.info().device.name.clone();
    let (tx, rx) = mpsc::channel();
    opened
        .recorder
        .start(ProbeSink::with_ready(tx))
        .map_err(Error::from)?;
    if rx.recv_timeout(Duration::from_secs(15)).is_err() {
        return Ok(Outcome::Fail("no audio before the handoff".into()));
    }
    say!(
        "\n>>> Move {name} to your phone NOW (play something on the phone, or pick it there), keep speaking."
    );
    say!(
        "    Waiting up to {} s for the recorder to report it...",
        ACTION_WAIT.as_secs()
    );
    let asked = Instant::now();
    let mut stale = false;
    let failure = opened.failures.recv_timeout(ACTION_WAIT).ok();
    let stopped = opened.recorder.stop();
    let first = match (&failure, &stopped) {
        (Some((at, e)), _) => {
            say!(
                "  reported {:.1} s after the prompt: {e}",
                (*at - asked).as_secs_f64()
            );
            format!(
                "handoff reported as {:?} after {:.1} s",
                e.kind(),
                (*at - asked).as_secs_f64()
            )
        }
        (None, Ok(s)) => {
            let silent = s.sink.trailing_zero_seconds();
            let line = format!(
                "no failure reported; {}; trailing digital silence {silent:.1} s",
                s.sink.describe()
            );
            say!("  {line}");
            if silent >= 1.0 {
                stale = true;
                format!("stale stream: {line}")
            } else {
                format!("{line} (the headset may not have moved)")
            }
        }
        (None, Err(e)) => format!("stop failed: {e}"),
    };
    drop(opened);
    prompt(&format!(
        "Bring {name} back to this computer (select it in the menu bar / sound settings), then continue."
    ));
    match recover_quietly(device.as_deref()) {
        // A stale stream is the library's failure to report the handoff.
        Ok(r) if stale => Ok(Outcome::Fail(format!(
            "{first} (the library only logs digital silence; see TODO.md, \"Digital silence\"); \
             back on this computer: {r}"
        ))),
        Ok(r) => Ok(Outcome::Info(format!(
            "{first}; back on this computer: {r}"
        ))),
        Err(r) => Ok(Outcome::Fail(format!(
            "{first}; after bringing it back: {r}"
        ))),
    }
}

/// Opens (retrying for 15 s) and records 2 s; Ok if real audio.
fn recover_quietly(device: Option<&str>) -> Result<String, String> {
    let deadline = Instant::now() + Duration::from_secs(15);
    let opened = loop {
        match open(device) {
            Ok(opened) => break opened,
            Err(e) if Instant::now() < deadline => {
                say!("  reopen failed ({e}); retrying");
                thread::sleep(Duration::from_secs(1));
            }
            Err(e) => return Err(format!("could not reopen: {e}")),
        }
    };
    let stopped = record(&opened, 2.0).map_err(|e| format!("recording failed: {e}"))?;
    if stopped.is_complete() && stopped.sink.peak > 0.0 {
        Ok(stopped.sink.describe())
    } else {
        Err(format!(
            "{:?}, {}",
            stopped.end_reason,
            stopped.sink.describe()
        ))
    }
}
