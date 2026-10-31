//! Turns a captured segment into the sequence the recognizer compares.

use anyhow::{Result, bail};
use serde::{Deserialize, Serialize};

use crate::frame::Frame;

/// Which sensor groups feed the recognizer.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct FeatureSet {
    pub attitude: bool,
    pub rotation: bool,
    pub acceleration: bool,
    pub gravity: bool,
}

impl Default for FeatureSet {
    fn default() -> Self {
        FeatureSet {
            attitude: true,
            rotation: true,
            acceleration: true,
            gravity: false,
        }
    }
}

pub const GROUPS: [&str; 4] = ["attitude", "rotation", "acceleration", "gravity"];

impl FeatureSet {
    pub fn flags(&self) -> [bool; 4] {
        [self.attitude, self.rotation, self.acceleration, self.gravity]
    }

    pub fn from_flags(f: [bool; 4]) -> FeatureSet {
        FeatureSet {
            attitude: f[0],
            rotation: f[1],
            acceleration: f[2],
            gravity: f[3],
        }
    }

    pub fn from_names(names: &[String]) -> Result<FeatureSet> {
        let mut flags = [false; 4];
        for n in names {
            match GROUPS.iter().position(|g| g == n) {
                Some(i) => flags[i] = true,
                None => bail!("unknown feature group {n:?}; use {}", GROUPS.join(", ")),
            }
        }
        let set = FeatureSet::from_flags(flags);
        if set.dims() == 0 {
            bail!("choose at least one feature group");
        }
        Ok(set)
    }

    pub fn dims(&self) -> usize {
        self.flags().iter().filter(|f| **f).count() * 3
    }

    pub fn label(&self) -> String {
        let names: Vec<&str> = GROUPS
            .iter()
            .zip(self.flags())
            .filter(|(_, on)| *on)
            .map(|(n, _)| *n)
            .collect();
        names.join("+")
    }

    /// Every non-empty combination, for comparing which groups carry the signal.
    pub fn all_combinations() -> Vec<FeatureSet> {
        (1u8..16)
            .map(|m| FeatureSet::from_flags([m & 1 != 0, m & 2 != 0, m & 4 != 0, m & 8 != 0]))
            .collect()
    }
}

/// A multichannel sequence stored row by row.
#[derive(Clone, Debug, PartialEq)]
pub struct Seq {
    pub dims: usize,
    pub data: Vec<f32>,
}

impl Seq {
    pub fn len(&self) -> usize {
        self.data.len().checked_div(self.dims).unwrap_or(0)
    }

    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    pub fn row(&self, i: usize) -> &[f32] {
        &self.data[i * self.dims..(i + 1) * self.dims]
    }
}

/// Motion energy of each frame in units of the segment's own noise (median
/// absolute deviation of each channel), smoothed over 5 frames.
fn energy(frames: &[Frame]) -> Vec<f32> {
    let vecs: Vec<[f32; 9]> = frames
        .windows(2)
        .map(|w| {
            let mut v = [0.0; 9];
            for i in 0..3 {
                v[i] = f32::from(w[1].attitude[i]) - f32::from(w[0].attitude[i]);
                v[3 + i] = f32::from(w[1].rotation[i]);
                v[6 + i] = f32::from(w[1].acceleration[i]);
            }
            v
        })
        .collect();
    let median = |mut v: Vec<f32>| {
        v.sort_by(f32::total_cmp);
        v[v.len() / 2]
    };
    let mut centre = [0.0; 9];
    let mut scale = [1.0; 9];
    for c in 0..9 {
        centre[c] = median(vecs.iter().map(|v| v[c]).collect());
        scale[c] = (median(vecs.iter().map(|v| (v[c] - centre[c]).abs()).collect()) * 1.4826).max(1.0);
    }
    let raw: Vec<f32> = vecs
        .iter()
        .map(|v| ((0..9).map(|c| ((v[c] - centre[c]) / scale[c]).powi(2)).sum::<f32>() / 9.0).sqrt())
        .collect();
    (0..raw.len())
        .map(|i| {
            let lo = i.saturating_sub(2);
            let hi = (i + 3).min(raw.len());
            raw[lo..hi].iter().sum::<f32>() / (hi - lo) as f32
        })
        .collect()
}

/// Keeps the movement around the strongest moment and drops the still
/// frames around it. Short dips (between two syllables) stay inside.
pub fn trim(frames: &[Frame]) -> &[Frame] {
    const MIN: usize = 12;
    const MARGIN: usize = 5;
    const MAX_GAP: usize = 15;
    if frames.len() <= MIN {
        return frames;
    }
    let e = energy(frames);
    let (peak_at, peak) = e
        .iter()
        .copied()
        .enumerate()
        .fold((0, 0.0f32), |best, (i, v)| if v > best.1 { (i, v) } else { best });
    let thr = (peak * 0.2).max(2.0);
    if peak < thr {
        return frames;
    }
    let reach = |indices: &mut dyn Iterator<Item = usize>| {
        let mut last = peak_at;
        let mut gap = 0;
        for i in indices {
            if e[i] > thr {
                last = i;
                gap = 0;
            } else {
                gap += 1;
                if gap > MAX_GAP {
                    break;
                }
            }
        }
        last
    };
    let first = reach(&mut (0..peak_at).rev());
    let last = reach(&mut (peak_at + 1..e.len()));
    // Energy index i describes the step from frame i to frame i + 1.
    let mut lo = first.saturating_sub(MARGIN);
    let mut hi = (last + 2 + MARGIN).min(frames.len());
    if hi - lo < MIN {
        let grow = MIN - (hi - lo);
        lo = lo.saturating_sub(grow / 2 + 1);
        hi = (lo + MIN).min(frames.len());
    }
    &frames[lo..hi]
}

/// Per frame: attitude change since the segment start, rotation rate, user
/// acceleration and gravity, lightly smoothed. The recognizer then compares
/// how these change (the frame-to-frame derivative), scaled so every take has
/// the same overall amplitude: a word mouthed more or less widely keeps its
/// shape. On two sessions of 4 French words this took leave-one-out accuracy
/// from 58 % to 75 %.
pub fn extract(frames: &[Frame], set: FeatureSet) -> Seq {
    let frames = trim(frames);
    let dims = set.dims();
    let n = frames.len();
    let mut data = Vec::with_capacity(n * dims);
    let origin = frames.first().map(|f| f.attitude).unwrap_or_default();
    for f in frames {
        if set.attitude {
            data.extend(f.attitude.iter().zip(origin).map(|(v, o)| f32::from(*v) - f32::from(o)));
        }
        if set.rotation {
            data.extend(f.rotation.iter().map(|v| f32::from(*v)));
        }
        if set.acceleration {
            data.extend(f.acceleration.iter().map(|v| f32::from(*v)));
        }
        if set.gravity {
            data.extend(f.gravity.iter().map(|v| f32::from(*v)));
        }
    }
    let mut seq = Seq { dims, data };
    smooth_and_center(&mut seq);
    differentiate(&mut seq);
    normalize_amplitude(&mut seq);
    seq
}

fn smooth_and_center(seq: &mut Seq) {
    let (n, d) = (seq.len(), seq.dims);
    if n == 0 {
        return;
    }
    let src = seq.data.clone();
    for c in 0..d {
        for i in 0..n {
            let lo = i.saturating_sub(1);
            let hi = (i + 2).min(n);
            seq.data[i * d + c] = (lo..hi).map(|k| src[k * d + c]).sum::<f32>() / (hi - lo) as f32;
        }
        let mean = (0..n).map(|i| seq.data[i * d + c]).sum::<f32>() / n as f32;
        for i in 0..n {
            seq.data[i * d + c] -= mean;
        }
    }
}

/// Central difference of each channel.
fn differentiate(seq: &mut Seq) {
    let (n, d) = (seq.len(), seq.dims);
    if n < 2 {
        return;
    }
    let src = seq.data.clone();
    for i in 0..n {
        let before = i.saturating_sub(1);
        let after = (i + 1).min(n - 1);
        for c in 0..d {
            seq.data[i * d + c] = src[after * d + c] - src[before * d + c];
        }
    }
}

/// Divides by the root mean square over all channels and frames.
fn normalize_amplitude(seq: &mut Seq) {
    let rms = (seq.data.iter().map(|v| v * v).sum::<f32>() / seq.data.len().max(1) as f32).sqrt();
    if rms > 1.0 {
        for v in &mut seq.data {
            *v /= rms;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    pub fn still(n: usize) -> Vec<Frame> {
        (0..n)
            .map(|i| Frame {
                t: i as f64 * 0.01,
                attitude: [-25000, -3000, -4000],
                rotation: [0, 0, 0],
                acceleration: [0, 0, 0],
                gravity: [-300, 950, 250],
            })
            .collect()
    }

    #[test]
    fn trims_still_edges() {
        let mut frames = still(100);
        for (i, f) in frames.iter_mut().enumerate().skip(40).take(20) {
            f.rotation = [((i - 40) as i16) * 30, 0, 0];
        }
        let t = trim(&frames);
        assert!(t.len() < 40 && t.len() >= 20, "{}", t.len());
        assert!(t[0].t >= 0.30);
    }

    #[test]
    fn dims_follow_the_set() {
        let set = FeatureSet {
            attitude: true,
            rotation: false,
            acceleration: true,
            gravity: false,
        };
        let seq = extract(&still(30), set);
        assert_eq!(seq.dims, 6);
        assert_eq!(seq.len(), 30);
        assert!(seq.data.iter().all(|v| v.abs() < 1e-3));
        assert_eq!(set.label(), "attitude+acceleration");
    }

    #[test]
    fn names_round_trip() {
        let set = FeatureSet::from_names(&["rotation".into(), "gravity".into()]).unwrap();
        assert_eq!(set.label(), "rotation+gravity");
        assert!(FeatureSet::from_names(&["jaw".into()]).is_err());
        assert_eq!(FeatureSet::all_combinations().len(), 15);
    }
}
