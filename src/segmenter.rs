//! Cuts the frame stream into segments, one per mouthed word.
//!
//! The motion energy is the size of the motion vector (attitude change,
//! rotation rate, user acceleration) in units of the sensor noise. The noise
//! is measured once on a still head, then keeps adapting while nobody
//! speaks, so breathing and small head moves raise the floor instead of
//! triggering captures. In auto mode a segment starts above `k` and ends
//! after a short spell below `k / 2`. In manual mode the caller starts and
//! stops it.

use std::collections::VecDeque;

use crate::frame::Frame;

const DIMS: usize = 9;

#[derive(Clone, Copy, Debug)]
pub struct Params {
    /// Start threshold, in units of the noise.
    pub k: f32,
    pub calibration_frames: usize,
    pub preroll: usize,
    pub hangover: usize,
    pub start_frames: usize,
    pub min_frames: usize,
    pub max_frames: usize,
}

impl Params {
    /// Durations in seconds turned into frame counts at `hz`.
    pub fn for_rate(hz: f32, k: f32) -> Params {
        let f = |s: f32| ((s * hz).round() as usize).max(1);
        Params {
            k,
            calibration_frames: f(2.0),
            preroll: f(0.2),
            hangover: f(0.3),
            start_frames: f(0.04).max(2),
            min_frames: f(0.15),
            max_frames: f(2.5),
        }
    }
}

#[derive(Debug, PartialEq)]
pub enum Event {
    Started,
    Finished(Vec<Frame>),
    /// A segment too short to be a word.
    Dropped,
    Calibrated,
}

#[derive(Clone, Debug)]
struct Noise {
    baseline: [f32; DIMS],
    scale: [f32; DIMS],
}

pub struct Segmenter {
    pub params: Params,
    pub auto: bool,
    noise: Option<Noise>,
    calibrating: Option<Vec<[f32; DIMS]>>,
    prev: Option<Frame>,
    recent: VecDeque<f32>,
    preroll: VecDeque<Frame>,
    current: Option<Vec<Frame>>,
    above: usize,
    below: usize,
    /// Smoothed energy of the last frame, for display.
    pub energy: f32,
    /// Highest energy in the current or last segment.
    pub peak: f32,
}

impl Segmenter {
    pub fn new(params: Params) -> Segmenter {
        Segmenter {
            params,
            auto: false,
            noise: None,
            calibrating: None,
            prev: None,
            recent: VecDeque::new(),
            preroll: VecDeque::new(),
            current: None,
            above: 0,
            below: 0,
            energy: 0.0,
            peak: 0.0,
        }
    }

    pub fn threshold(&self) -> Option<f32> {
        self.noise.as_ref().map(|_| self.params.k)
    }

    pub fn is_calibrating(&self) -> bool {
        self.calibrating.is_some()
    }

    pub fn is_capturing(&self) -> bool {
        self.current.is_some()
    }

    /// Measures the noise over the next frames; keep still meanwhile.
    pub fn calibrate(&mut self) {
        self.current = None;
        self.calibrating = Some(Vec::new());
    }

    pub fn start(&mut self) {
        if self.current.is_none() {
            self.current = Some(self.preroll.iter().copied().collect());
            self.below = 0;
            self.peak = 0.0;
        }
    }

    pub fn stop(&mut self) -> Event {
        match self.current.take() {
            Some(frames) if frames.len() >= self.params.min_frames => Event::Finished(frames),
            _ => Event::Dropped,
        }
    }

    pub fn cancel(&mut self) {
        self.current = None;
    }

    /// Forgets the recent stream, after a gap in the samples.
    pub fn reset(&mut self) {
        self.current = None;
        self.prev = None;
        self.recent.clear();
        self.preroll.clear();
        self.above = 0;
        self.below = 0;
    }

    /// Motion vector: attitude change since the last frame, rotation rate
    /// and user acceleration.
    fn vector(prev: &Frame, f: &Frame) -> [f32; DIMS] {
        let mut v = [0.0; DIMS];
        for i in 0..3 {
            v[i] = f32::from(f.attitude[i]) - f32::from(prev.attitude[i]);
            v[3 + i] = f32::from(f.rotation[i]);
            v[6 + i] = f32::from(f.acceleration[i]);
        }
        v
    }

    fn raw_energy(v: &[f32; DIMS], noise: &Noise) -> f32 {
        let s: f32 = (0..DIMS)
            .map(|i| ((v[i] - noise.baseline[i]) / noise.scale[i]).powi(2))
            .sum();
        (s / DIMS as f32).sqrt()
    }

    pub fn push(&mut self, f: Frame) -> Option<Event> {
        let prev = self.prev.replace(f)?;
        let v = Self::vector(&prev, &f);

        self.preroll.push_back(f);
        while self.preroll.len() > self.params.preroll {
            self.preroll.pop_front();
        }

        if let Some(samples) = self.calibrating.as_mut() {
            samples.push(v);
            if samples.len() >= self.params.calibration_frames {
                let samples = self.calibrating.take().unwrap_or_default();
                self.noise = Some(robust_noise(&samples));
                self.recent.clear();
                return Some(Event::Calibrated);
            }
            return None;
        }

        let Some(noise) = self.noise.as_mut() else {
            // Not calibrated: only manual capture works.
            let full = self.current.as_mut().map(|c| {
                c.push(f);
                c.len() >= self.params.max_frames * 2
            });
            return if full == Some(true) { Some(self.stop()) } else { None };
        };
        let e = Self::raw_energy(&v, noise);
        self.recent.push_back(e);
        while self.recent.len() > 5 {
            self.recent.pop_front();
        }
        self.energy = self.recent.iter().sum::<f32>() / self.recent.len() as f32;
        let loud = self.energy > self.params.k;
        let quiet = self.energy < self.params.k * 0.5;

        // Follow the noise while nobody speaks: ~1 s for the bias, ~3 s for
        // the spread (mean absolute deviation × √(π/2) estimates σ).
        if self.current.is_none() && !loud {
            for ((x, base), scale) in v.iter().zip(&mut noise.baseline).zip(&mut noise.scale) {
                let d = x - *base;
                *base += d * 0.01;
                *scale = (*scale + (d.abs() * 1.2533 - *scale) * 0.003).max(1.0);
            }
        }

        if let Some(frames) = self.current.as_mut() {
            frames.push(f);
            self.peak = self.peak.max(self.energy);
            if self.auto {
                self.below = if quiet { self.below + 1 } else { 0 };
                if self.below >= self.params.hangover || frames.len() >= self.params.max_frames {
                    // Keep a little of the quiet tail, drop the rest.
                    let tail = self.below.saturating_sub(self.params.preroll / 2);
                    let n = frames.len().saturating_sub(tail).max(1);
                    frames.truncate(n);
                    return Some(self.stop());
                }
            } else if frames.len() >= self.params.max_frames * 2 {
                return Some(self.stop());
            }
            return None;
        }

        if self.auto {
            self.above = if loud { self.above + 1 } else { 0 };
            if self.above >= self.params.start_frames {
                self.above = 0;
                self.start();
                return Some(Event::Started);
            }
        }
        None
    }
}

/// Median and scaled median absolute deviation of each channel: a blink or
/// a swallow during calibration barely moves them.
fn robust_noise(samples: &[[f32; DIMS]]) -> Noise {
    let mut baseline = [0.0; DIMS];
    let mut scale = [1.0; DIMS];
    if samples.is_empty() {
        return Noise { baseline, scale };
    }
    let median = |mut v: Vec<f32>| {
        v.sort_by(f32::total_cmp);
        v[v.len() / 2]
    };
    for i in 0..DIMS {
        let m = median(samples.iter().map(|s| s[i]).collect());
        let mad = median(samples.iter().map(|s| (s[i] - m).abs()).collect());
        baseline[i] = m;
        // A channel that never moved still has one unit of quantisation noise.
        scale[i] = (mad * 1.4826).max(1.0);
    }
    Noise { baseline, scale }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn frame(i: usize, rot: i16) -> Frame {
        // Deterministic sensor noise of ±2.
        let noise = ((i * 7919) % 5) as i16 - 2;
        Frame {
            t: i as f64 * 0.01,
            attitude: [-25000 + noise, -3000, -4000 - noise],
            rotation: [noise + rot, -noise, noise],
            acceleration: [noise, noise, -noise],
            gravity: [0, 1024, 0],
        }
    }

    #[test]
    fn auto_mode_finds_one_word() {
        let mut s = Segmenter::new(Params::for_rate(100.0, 4.0));
        s.auto = true;
        s.calibrate();
        let mut events = Vec::new();
        let mut i = 0;
        for _ in 0..250 {
            events.extend(s.push(frame(i, 0)));
            i += 1;
        }
        assert_eq!(events, vec![Event::Calibrated]);
        for k in 0..60 {
            let rot = ((k as f32 / 60.0 * std::f32::consts::PI).sin() * 200.0) as i16;
            events.extend(s.push(frame(i, rot)));
            i += 1;
        }
        for _ in 0..100 {
            events.extend(s.push(frame(i, 0)));
            i += 1;
        }
        assert_eq!(events.len(), 3, "{events:?}");
        assert_eq!(events[1], Event::Started);
        match &events[2] {
            Event::Finished(frames) => assert!((40..=120).contains(&frames.len()), "{}", frames.len()),
            e => panic!("{e:?}"),
        }
    }

    #[test]
    fn manual_mode_needs_no_calibration() {
        let mut s = Segmenter::new(Params::for_rate(100.0, 4.0));
        for i in 0..5 {
            s.push(frame(i, 0));
        }
        s.start();
        for i in 5..40 {
            assert_eq!(s.push(frame(i, 0)), None);
        }
        match s.stop() {
            Event::Finished(frames) => assert!(frames.len() >= 35),
            e => panic!("{e:?}"),
        }
        // Without frames before it, an immediate stop is too short.
        let mut fresh = Segmenter::new(Params::for_rate(100.0, 4.0));
        fresh.start();
        assert_eq!(fresh.stop(), Event::Dropped);
    }
}
