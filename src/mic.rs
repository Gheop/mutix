//! Notices when you speak aloud, so a word said in a conversation is not
//! typed. It reads the microphone through `parec` (PipeWire or PulseAudio),
//! keeps only a loudness level per 20 ms, and never stores audio.

use std::collections::VecDeque;
use std::io::Read;
use std::process::{Child, Command, Stdio};
use std::sync::{Arc, Mutex};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

use anyhow::{Context, Result};

const RATE: usize = 16_000;
const CHUNK: usize = RATE / 50; // 20 ms
const HISTORY: Duration = Duration::from_secs(15);
/// Voice is this many times louder than the room noise.
const RATIO: f32 = 4.0;
/// A voiced chunk keeps the flag up this long, to bridge short pauses.
const HANGOVER: Duration = Duration::from_millis(250);

#[derive(Default)]
struct State {
    chunks: VecDeque<(Instant, bool)>,
    floor: f32,
    level: f32,
    last_voice: Option<Instant>,
}

pub struct Microphone {
    state: Arc<Mutex<State>>,
    child: Child,
    reader: Option<JoinHandle<()>>,
}

impl Microphone {
    /// Starts listening on `source`, or on the default input.
    pub fn open(source: Option<&str>) -> Result<Microphone> {
        let mut cmd = Command::new("parec");
        cmd.args([
            "--raw",
            "--rate=16000",
            "--channels=1",
            "--format=s16le",
            "--latency-msec=20",
        ]);
        if let Some(s) = source {
            cmd.arg(format!("--device={s}"));
        }
        let mut child = cmd
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .spawn()
            .context("cannot run parec to listen to the microphone; install pulseaudio-utils")?;
        let mut stdout = child.stdout.take().context("parec gave no output")?;
        let state = Arc::new(Mutex::new(State::default()));
        let shared = state.clone();
        let reader = std::thread::Builder::new()
            .name("mutix-mic".into())
            .spawn(move || {
                let mut buf = vec![0u8; CHUNK * 2];
                while stdout.read_exact(&mut buf).is_ok() {
                    let sum: f64 = buf
                        .as_chunks::<2>()
                        .0
                        .iter()
                        .map(|b| f64::from(i16::from_le_bytes(*b)).powi(2))
                        .sum();
                    let rms = (sum / CHUNK as f64).sqrt() as f32;
                    let Ok(mut s) = shared.lock() else { return };
                    s.push(rms, Instant::now());
                }
            })
            .context("cannot start the microphone thread")?;
        Ok(Microphone {
            state,
            child,
            reader: Some(reader),
        })
    }

    /// True while you are speaking aloud.
    pub fn speaking(&self) -> bool {
        self.state
            .lock()
            .ok()
            .and_then(|s| s.last_voice)
            .is_some_and(|t| t.elapsed() < HANGOVER)
    }

    /// Loudness relative to the room noise; above 4 is voice.
    pub fn level(&self) -> f32 {
        self.state.lock().map(|s| s.level).unwrap_or(0.0)
    }

    /// Share of the 20 ms chunks between `from` and `to` that held voice.
    pub fn voiced_share(&self, from: Instant, to: Instant) -> f32 {
        let Ok(s) = self.state.lock() else { return 0.0 };
        let inside: Vec<bool> = s
            .chunks
            .iter()
            .filter(|(t, _)| *t >= from && *t <= to)
            .map(|(_, v)| *v)
            .collect();
        if inside.is_empty() {
            return 0.0;
        }
        inside.iter().filter(|v| **v).count() as f32 / inside.len() as f32
    }
}

impl State {
    fn push(&mut self, rms: f32, now: Instant) {
        // The floor drops at once to quieter levels and creeps up slowly,
        // so it tracks the room, not the voice.
        if self.floor <= 0.0 || rms < self.floor {
            self.floor = rms.max(1.0);
        } else {
            self.floor *= 1.002;
        }
        self.level = rms / self.floor.max(1.0);
        let voiced = self.level > RATIO && rms > 200.0;
        if voiced {
            self.last_voice = Some(now);
        }
        let held = self.last_voice.is_some_and(|t| now.duration_since(t) < HANGOVER);
        self.chunks.push_back((now, held));
        while self
            .chunks
            .front()
            .is_some_and(|(t, _)| now.duration_since(*t) > HISTORY)
        {
            self.chunks.pop_front();
        }
    }
}

impl Drop for Microphone {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
        if let Some(r) = self.reader.take() {
            let _ = r.join();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn voice_stands_out_from_room_noise() {
        let mut s = State::default();
        let t0 = Instant::now();
        for i in 0..200 {
            s.push(80.0 + (i % 7) as f32, t0 + Duration::from_millis(20 * i));
        }
        assert!(s.last_voice.is_none());
        let speech = t0 + Duration::from_millis(4000);
        for i in 0..25 {
            s.push(2500.0, speech + Duration::from_millis(20 * i));
        }
        assert!(s.last_voice.is_some());
        assert!(s.level > RATIO);
        // The floor stayed near the room level.
        assert!(s.floor < 200.0, "{}", s.floor);
    }
}
