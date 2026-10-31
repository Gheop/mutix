mod aap;
mod bluez;
mod config;
mod control;
mod dataset;
mod dtw;
mod eval;
mod features;
mod frame;
mod model;
mod output;
mod segmenter;
mod sensor;
mod tui;
mod voice;

use std::io::Write;
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};

use anyhow::{Context, Result, bail};
use clap::{Parser, Subcommand};

use config::Config;
use dataset::Dataset;
use features::FeatureSet;
use model::Model;
use segmenter::Segmenter;
use sensor::{Event, Sensor};

#[derive(Parser)]
#[command(
    version,
    about = "Silent words to text, read from the motion sensors of your AirPods"
)]
struct Cli {
    /// Configuration file.
    #[arg(long, global = true, default_value_os_t = config::default_path())]
    config: PathBuf,
    /// Recorded takes.
    #[arg(long, global = true, default_value_os_t = dataset::default_path())]
    dataset: PathBuf,
    #[command(subcommand)]
    command: Option<Cmd>,
}

#[derive(Subcommand)]
enum Cmd {
    /// Live sensors, training and decoding in the terminal (the default).
    Ui,
    /// List the AirPods BlueZ knows.
    Devices,
    /// Print motion samples as they arrive.
    Stream {
        /// Comma-separated values with a header, for recording to a file.
        #[arg(long)]
        csv: bool,
        /// Stop after this many seconds.
        #[arg(long)]
        seconds: Option<f64>,
    },
    /// Record takes of one or more words: keep still for 2 s, then mouth each word when prompted.
    Train {
        #[arg(required = true)]
        words: Vec<String>,
        /// Takes per word.
        #[arg(long, default_value_t = 10)]
        takes: usize,
        /// Speak the prompts (the `speak` command of the config, or spd-say).
        #[arg(long)]
        speak: bool,
    },
    /// List the trained words.
    Words,
    /// Delete the takes of a word, or only its last take.
    Forget {
        word: String,
        #[arg(long)]
        last: bool,
    },
    /// Check whether the motion tells your words apart.
    Eval {
        /// Compare every combination of sensor groups.
        #[arg(long)]
        compare: bool,
        #[arg(long, default_value_t = 500)]
        permutations: usize,
    },
    /// Recognize words and type them, or run their commands.
    Listen {
        /// Print words without typing or running anything.
        #[arg(long)]
        dry_run: bool,
        /// Wait for `mutix toggle` instead of detecting movement.
        #[arg(long)]
        manual: bool,
        /// Say each recognized word aloud (the `speak` command of the config, or spd-say).
        #[arg(long)]
        speak: bool,
        /// Stop after this many seconds.
        #[arg(long)]
        seconds: Option<u64>,
    },
    /// Start or end a capture in a running `mutix listen`.
    Toggle,
    /// Make a running `mutix listen` measure the still noise again.
    Calibrate,
    /// Show the configuration path, or write a commented sample.
    Config {
        #[arg(long)]
        init: bool,
    },
    /// Check Bluetooth, the motion stream and the virtual keyboard.
    Doctor,
}

static INTERRUPTED: AtomicBool = AtomicBool::new(false);

extern "C" fn on_signal(_: libc::c_int) {
    INTERRUPTED.store(true, Ordering::Relaxed);
}

/// Ctrl+C sets a flag instead of killing the process, so the AirPods get
/// the stop request.
fn catch_signals() {
    let handler: extern "C" fn(libc::c_int) = on_signal;
    // SAFETY: the handler only stores to an atomic.
    unsafe {
        libc::signal(libc::SIGINT, handler as libc::sighandler_t);
        libc::signal(libc::SIGTERM, handler as libc::sighandler_t);
    }
}

fn interrupted() -> bool {
    INTERRUPTED.load(Ordering::Relaxed)
}

fn main() {
    // Exit quietly when the output pipe closes (`mutix stream | head`).
    // SAFETY: restoring the default disposition of one signal.
    unsafe {
        libc::signal(libc::SIGPIPE, libc::SIG_DFL);
    }
    let cli = Cli::parse();
    if let Err(e) = run(cli) {
        eprintln!("mutix: {e:#}");
        std::process::exit(1);
    }
}

fn run(cli: Cli) -> Result<()> {
    let cfg = Config::load(&cli.config)?;
    match cli.command.unwrap_or(Cmd::Ui) {
        Cmd::Ui => tui::run(&cfg, &cli.dataset),
        Cmd::Devices => devices(),
        Cmd::Stream { csv, seconds } => stream(&cfg, csv, seconds),
        Cmd::Train { words, takes, speak } => train(&cfg, &cli.dataset, &words, takes, speak),
        Cmd::Words => words(&cli.dataset),
        Cmd::Forget { word, last } => forget(&cli.dataset, &word, last),
        Cmd::Eval { compare, permutations } => evaluate(&cfg, &cli.dataset, compare, permutations),
        Cmd::Listen {
            dry_run,
            manual,
            speak,
            seconds,
        } => listen(&cfg, &cli.dataset, dry_run, manual, speak, seconds),
        Cmd::Toggle => control::send(control::Command::Toggle),
        Cmd::Calibrate => control::send(control::Command::Calibrate),
        Cmd::Config { init } => config_cmd(&cli.config, init),
        Cmd::Doctor => doctor(&cfg),
    }
}

fn devices() -> Result<()> {
    let found = bluez::devices()?;
    if found.is_empty() {
        println!("No AirPods paired. Pair them in the Bluetooth settings first.");
    }
    for d in found {
        println!(
            "{}  {}  {}",
            d.address,
            if d.connected { "connected   " } else { "disconnected" },
            d.name
        );
    }
    Ok(())
}

/// Logs connection changes to stderr; returns the frame, if any.
fn report(event: Event) -> Option<frame::Frame> {
    match event {
        Event::Frame(f) => return Some(f),
        Event::Connecting(name) => eprintln!("Connecting to {name}…"),
        Event::Connected { name, address } => eprintln!("Streaming from {name} ({address})."),
        Event::Ears(l, r) => {
            if !(l && r) {
                eprintln!("A bud is out of the ear; the motion stream pauses until both are in.");
            }
        }
        Event::Lost(why) => eprintln!("{why}; retrying…"),
    }
    None
}

fn stream(cfg: &Config, csv: bool, seconds: Option<f64>) -> Result<()> {
    catch_signals();
    let sensor = Sensor::spawn(cfg.device.clone(), cfg.interval_us());
    let mut out = std::io::stdout().lock();
    if csv {
        writeln!(
            out,
            "t,att_x,att_y,att_z,rot_x,rot_y,rot_z,acc_x,acc_y,acc_z,grav_x,grav_y,grav_z"
        )?;
    }
    let mut first: Option<Instant> = None;
    let mut count = 0usize;
    let mut window = (Instant::now(), 0usize);
    let started = Instant::now();
    while !interrupted() {
        // Count from the first sample, but give up if none comes at all.
        let since = first.unwrap_or(started).elapsed().as_secs_f64();
        if seconds.is_some_and(|limit| since >= limit + if first.is_some() { 0.0 } else { 20.0 }) {
            break;
        }
        let Ok(event) = sensor.rx.recv_timeout(Duration::from_millis(200)) else {
            continue;
        };
        let Some(f) = report(event) else { continue };
        first.get_or_insert_with(Instant::now);
        count += 1;
        let c = f.channels();
        if csv {
            let cols: Vec<String> = c.iter().map(|v| format!("{v}")).collect();
            writeln!(out, "{:.6},{}", f.t, cols.join(","))?;
        } else {
            window.1 += 1;
            if window.0.elapsed() >= Duration::from_millis(100) {
                let hz = window.1 as f64 / window.0.elapsed().as_secs_f64();
                writeln!(
                    out,
                    "{hz:5.0} Hz  attitude {:6} {:6} {:6}  rotation {:5} {:5} {:5}  accel {:4} {:4} {:4}  gravity {:5} {:5} {:5}",
                    c[0], c[1], c[2], c[3], c[4], c[5], c[6], c[7], c[8], c[9], c[10], c[11]
                )?;
                window = (Instant::now(), 0);
            }
        }
    }
    if let Some(s) = first {
        eprintln!(
            "{count} samples in {:.1} s ({:.0} Hz).",
            s.elapsed().as_secs_f64(),
            count as f64 / s.elapsed().as_secs_f64()
        );
    }
    Ok(())
}

/// Next motion frame, skipping connection messages; `None` on Ctrl+C.
fn next_frame(sensor: &Sensor) -> Option<frame::Frame> {
    while !interrupted() {
        if let Ok(event) = sensor.rx.recv_timeout(Duration::from_millis(200))
            && let Some(f) = report(event)
        {
            return Some(f);
        }
    }
    None
}

/// Drops the samples that piled up while a prompt was spoken.
fn flush(sensor: &Sensor, seg: &mut Segmenter) {
    while let Ok(event) = sensor.rx.try_recv() {
        if !matches!(event, Event::Frame(_)) {
            report(event);
        }
    }
    seg.reset();
}

fn train(cfg: &Config, path: &std::path::Path, words: &[String], takes: usize, speak: bool) -> Result<()> {
    let words: Vec<&str> = words.iter().map(|w| w.trim()).filter(|w| !w.is_empty()).collect();
    if words.is_empty() {
        bail!("give at least one word to train");
    }
    catch_signals();
    let voice = voice::Voice::new(speak || cfg.speak.is_some(), cfg.speak.as_deref());
    let mut ds = Dataset::load(path)?;
    let sensor = Sensor::spawn(cfg.device.clone(), cfg.interval_us());
    let mut seg = Segmenter::new(segmenter::Params::for_rate(cfg.rate_hz(), cfg.trigger()));

    // Calibrate on a still head before the first word.
    let Some(first) = next_frame(&sensor) else {
        return Ok(());
    };
    voice.say(voice.keep_still());
    flush(&sensor, &mut seg);
    seg.push(first);
    seg.calibrate();
    loop {
        let Some(f) = next_frame(&sensor) else { return Ok(()) };
        if seg.push(f) == Some(segmenter::Event::Calibrated) {
            break;
        }
    }

    // Each take is a fixed window after the prompt; the features keep only
    // the part that moves, so the window can be generous.
    let window = (cfg.rate_hz() * 2.5) as usize;
    for word in &words {
        voice.say(&voice.next_word(word, takes));
        let mut done = 0;
        let mut misses = 0;
        while done < takes && !interrupted() {
            voice.say(word);
            flush(&sensor, &mut seg);
            let Some(f) = next_frame(&sensor) else { break };
            seg.push(f);
            seg.start();
            let mut frames = Vec::new();
            for _ in 0..window {
                let Some(f) = next_frame(&sensor) else { break };
                seg.push(f);
            }
            if let segmenter::Event::Finished(captured) = seg.stop() {
                frames = captured;
            }
            if interrupted() {
                break;
            }
            if seg.peak < seg.params.k || frames.is_empty() {
                misses += 1;
                eprintln!("  no movement (peak {:.1})", seg.peak);
                if misses >= 3 {
                    eprintln!(
                        "  three misses in a row; try `trigger` lower than {} in the config",
                        seg.params.k
                    );
                    misses = 0;
                }
                voice.say(voice.again());
                continue;
            }
            misses = 0;
            done += 1;
            eprintln!("  take {done}/{takes} of \"{word}\": peak {:.1}", seg.peak);
            ds.add(word, frames);
            ds.save(path)?;
        }
    }
    voice.say(voice.done());
    for (w, n) in ds.words() {
        if words.contains(&w.as_str()) {
            eprintln!("{n:4}  {w}");
        }
    }
    Ok(())
}

fn words(path: &std::path::Path) -> Result<()> {
    let ds = Dataset::load(path)?;
    if ds.templates.is_empty() {
        println!("No words yet. Record some with `mutix train WORD` or in `mutix ui`.");
    }
    for (w, n) in ds.words() {
        println!("{n:4}  {w}");
    }
    Ok(())
}

fn forget(path: &std::path::Path, word: &str, last: bool) -> Result<()> {
    let mut ds = Dataset::load(path)?;
    let removed = if last {
        usize::from(ds.remove_last(word))
    } else {
        ds.remove_word(word)
    };
    if removed == 0 {
        bail!("no takes of {word:?}");
    }
    ds.save(path)?;
    println!("Removed {removed} take(s) of {word:?}.");
    Ok(())
}

pub fn print_report(r: &eval::Report) {
    println!(
        "{}: {} takes, {} words. Accuracy {:.0} %, chance {:.0} %, p = {:.3}{}",
        r.set.label(),
        r.takes,
        r.words.len(),
        r.accuracy * 100.0,
        r.chance * 100.0,
        r.p_value,
        if r.significant() {
            "  → the motion tells these words apart"
        } else {
            "  → not better than chance"
        }
    );
}

fn evaluate(cfg: &Config, path: &std::path::Path, compare: bool, permutations: usize) -> Result<()> {
    let ds = Dataset::load(path)?;
    let params = cfg.params()?;
    if compare {
        let mut reports = Vec::new();
        for set in FeatureSet::all_combinations() {
            reports.push(eval::evaluate(&ds.templates, set, params.band, permutations)?);
        }
        reports.sort_by(|a, b| b.accuracy.total_cmp(&a.accuracy));
        for r in &reports {
            print_report(r);
        }
        return Ok(());
    }
    let r = eval::evaluate(&ds.templates, params.set, params.band, permutations)?;
    print_report(&r);
    let width = r.words.iter().map(|w| w.chars().count()).max().unwrap_or(4).max(5);
    print!("\n{:>width$}", "said ↓");
    for w in &r.words {
        print!(" {:>width$}", w);
    }
    println!();
    for (i, row) in r.confusion.iter().enumerate() {
        print!("{:>width$}", r.words[i]);
        for v in row {
            print!(" {v:>width$}");
        }
        println!();
    }
    let model = Model::train(&ds.templates, params)?;
    println!(
        "\nRejection distance: {:.2} (max_distance in the config to change it).",
        model.max_distance
    );
    Ok(())
}

fn listen(
    cfg: &Config,
    path: &std::path::Path,
    dry_run: bool,
    manual: bool,
    speak: bool,
    seconds: Option<u64>,
) -> Result<()> {
    catch_signals();
    let ds = Dataset::load(path)?;
    let model = Model::train(&ds.templates, cfg.params()?)?;
    let mut keyboard = if dry_run {
        None
    } else {
        Some(output::Keyboard::new(output::Layout::load(cfg.keyboard.as_deref())?)?)
    };
    let server = control::Server::bind()?;
    let sensor = Sensor::spawn(cfg.device.clone(), cfg.interval_us());
    let mut seg = Segmenter::new(segmenter::Params::for_rate(cfg.rate_hz(), cfg.trigger()));
    seg.auto = !manual;
    let words: Vec<String> = ds.words().into_iter().map(|(w, _)| w).collect();
    eprintln!("Listening for: {}.", words.join(", "));
    let voice = speak.then(|| voice::Voice::new(true, cfg.speak.as_deref()));
    let mut calibrated = false;
    let started = Instant::now();

    while !interrupted() && seconds.is_none_or(|s| started.elapsed() < Duration::from_secs(s)) {
        while let Some(cmd) = server.poll() {
            use control::Command as C;
            match cmd {
                C::Toggle if seg.is_capturing() => {
                    handle(seg.stop(), &model, cfg, keyboard.as_mut());
                }
                C::Toggle | C::Start => {
                    seg.start();
                    eprintln!("● capturing");
                }
                C::Stop => {
                    handle(seg.stop(), &model, cfg, keyboard.as_mut());
                }
                C::Calibrate => {
                    eprintln!("Keep still for 2 seconds…");
                    seg.calibrate();
                }
                C::Auto => seg.auto = true,
                C::Manual => seg.auto = false,
            }
        }
        let Ok(event) = sensor.rx.recv_timeout(Duration::from_millis(50)) else {
            continue;
        };
        let Some(f) = report(event) else { continue };
        if seg.auto && !calibrated && !seg.is_calibrating() {
            eprintln!("Keep still with your mouth closed for 2 seconds…");
            seg.calibrate();
        }
        match seg.push(f) {
            Some(segmenter::Event::Calibrated) => {
                calibrated = true;
                eprintln!("Ready.");
            }
            Some(ev) => {
                if let (Some(word), Some(v)) = (handle(ev, &model, cfg, keyboard.as_mut()), &voice) {
                    v.say(&word);
                    flush(&sensor, &mut seg);
                }
            }
            None => {}
        }
    }
    Ok(())
}

/// Classifies a finished segment and acts on it; returns the word if one was recognized.
fn handle(
    ev: segmenter::Event,
    model: &Model,
    cfg: &Config,
    keyboard: Option<&mut output::Keyboard>,
) -> Option<String> {
    let segmenter::Event::Finished(frames) = ev else {
        return None;
    };
    let d = model.classify(&frames);
    let Some(word) = &d.word else {
        eprintln!(
            "  ? closest \"{}\" (distance {:.2}, margin {:.2}): {}",
            d.best,
            d.distance,
            d.margin,
            d.reason.unwrap_or("")
        );
        return None;
    };
    println!("{word}");
    eprintln!("  ✓ {word} (distance {:.2}, margin {:.2})", d.distance, d.margin);
    let action = cfg.action(word);
    if let Some(cmd) = &action.command
        && keyboard.is_some()
        && let Err(e) = output::run_command(cmd, word)
    {
        eprintln!("mutix: {e:#}");
    }
    if let (Some(text), Some(kb)) = (&action.text, keyboard)
        && let Err(e) = kb.type_text(text)
    {
        eprintln!("mutix: {e:#}");
    }
    Some(word.clone())
}

fn config_cmd(path: &std::path::Path, init: bool) -> Result<()> {
    if init {
        if path.exists() {
            bail!("{} already exists", path.display());
        }
        if let Some(dir) = path.parent() {
            std::fs::create_dir_all(dir)?;
        }
        std::fs::write(path, config::SAMPLE).with_context(|| format!("cannot write {}", path.display()))?;
        println!("Wrote {}.", path.display());
    } else {
        println!("{}", path.display());
    }
    Ok(())
}

fn doctor(cfg: &Config) -> Result<()> {
    let ok = |b: bool| if b { "ok  " } else { "FAIL" };
    let found = bluez::devices();
    let connected = match &found {
        Ok(list) => list.iter().find(|d| d.connected).cloned(),
        Err(_) => None,
    };
    match &found {
        Ok(list) => println!(
            "{} BlueZ: {} AirPods paired, {} connected",
            ok(connected.is_some()),
            list.len(),
            usize::from(connected.is_some())
        ),
        Err(e) => println!("FAIL BlueZ: {e:#}"),
    }

    let sensor = Sensor::spawn(cfg.device.clone(), cfg.interval_us());
    let deadline = Instant::now() + Duration::from_secs(12);
    let mut first: Option<Instant> = None;
    let mut count = 0usize;
    let mut last_error = None;
    while Instant::now() < deadline {
        match sensor.rx.recv_timeout(Duration::from_millis(200)) {
            Ok(Event::Frame(_)) => {
                first.get_or_insert_with(Instant::now);
                count += 1;
                if first.is_some_and(|s| s.elapsed() >= Duration::from_secs(2)) {
                    break;
                }
            }
            Ok(Event::Lost(e)) => last_error = Some(e),
            _ => {}
        }
    }
    drop(sensor);
    match first {
        Some(s) => {
            let hz = count as f64 / s.elapsed().as_secs_f64();
            println!(
                "{} Motion stream: {hz:.0} Hz (asked for {:.0} Hz)",
                ok(hz > cfg.rate_hz() as f64 * 0.5),
                cfg.rate_hz()
            );
        }
        None => println!(
            "FAIL Motion stream: {}",
            last_error.unwrap_or_else(|| "no samples".into())
        ),
    }

    match output::Layout::load(cfg.keyboard.as_deref()) {
        Ok(layout) => {
            let typable = "Bonjour".chars().all(|c| layout.strokes(c).is_some());
            println!(
                "{} Keyboard layout: {}",
                ok(typable),
                cfg.keyboard.as_deref().unwrap_or("from GNOME")
            );
            match output::Keyboard::new(layout) {
                Ok(_) => println!("ok   Virtual keyboard (/dev/uinput)"),
                Err(e) => println!("FAIL Virtual keyboard: {e:#}"),
            }
        }
        Err(e) => println!("FAIL Keyboard layout: {e:#}"),
    }
    Ok(())
}
