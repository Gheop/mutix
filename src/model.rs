//! Nearest-template recognizer: each take is a template, a new segment gets
//! the word of the closest template, unless it is too far from all of them or
//! too close to two different words.

use anyhow::{Result, bail};

use crate::dataset::Template;
use crate::dtw;
use crate::features::{self, FeatureSet, Seq};
use crate::frame::Frame;

#[derive(Clone, Copy, Debug)]
pub struct Params {
    pub set: FeatureSet,
    /// Sakoe–Chiba band, as a fraction of the longer sequence.
    pub band: f32,
    /// Reject above this distance; `None` derives it from the takes.
    pub max_distance: Option<f32>,
    /// Reject when (second word − best) / second word is below this.
    pub min_margin: f32,
}

impl Default for Params {
    fn default() -> Self {
        Params {
            set: FeatureSet::default(),
            band: 0.15,
            max_distance: None,
            min_margin: 0.08,
        }
    }
}

#[derive(Clone, Debug, PartialEq)]
pub struct Decision {
    pub word: Option<String>,
    pub best: String,
    pub distance: f32,
    pub margin: f32,
    pub reason: Option<&'static str>,
}

pub struct Model {
    pub params: Params,
    pub scale: Vec<f32>,
    pub templates: Vec<(String, Seq)>,
    pub max_distance: f32,
}

/// 1 / standard deviation of each channel over every frame of every sequence.
pub fn channel_scale(seqs: &[&Seq], dims: usize) -> Vec<f32> {
    let mut sum = vec![0.0f64; dims];
    let mut sq = vec![0.0f64; dims];
    let mut n = 0usize;
    for s in seqs {
        for i in 0..s.len() {
            for (c, v) in s.row(i).iter().enumerate() {
                sum[c] += f64::from(*v);
                sq[c] += f64::from(*v) * f64::from(*v);
            }
            n += 1;
        }
    }
    (0..dims)
        .map(|c| {
            if n == 0 {
                return 1.0;
            }
            let mean = sum[c] / n as f64;
            let var = (sq[c] / n as f64 - mean * mean).max(0.0);
            1.0 / (var.sqrt() as f32).max(1.0)
        })
        .collect()
}

/// Full distance matrix, computed on every core.
pub fn matrix(seqs: &[Seq], scale: &[f32], band: f32) -> Vec<f32> {
    let n = seqs.len();
    let mut out = vec![0.0f32; n * n];
    let threads = std::thread::available_parallelism()
        .map(|p| p.get())
        .unwrap_or(1)
        .min(n.max(1));
    let rows: Vec<Vec<(usize, usize, f32)>> = std::thread::scope(|s| {
        let handles: Vec<_> = (0..threads)
            .map(|t| {
                s.spawn(move || {
                    let mut part = Vec::new();
                    for i in (t..n).step_by(threads) {
                        for j in i + 1..n {
                            part.push((i, j, dtw::distance(&seqs[i], &seqs[j], scale, band)));
                        }
                    }
                    part
                })
            })
            .collect();
        handles
            .into_iter()
            .map(|h| h.join().expect("distance thread panicked"))
            .collect()
    });
    for (i, j, d) in rows.into_iter().flatten() {
        out[i * n + j] = d;
        out[j * n + i] = d;
    }
    out
}

impl Model {
    pub fn train(templates: &[Template], params: Params) -> Result<Model> {
        if templates.is_empty() {
            bail!("no takes recorded yet; train a few words first");
        }
        let seqs: Vec<Seq> = templates
            .iter()
            .map(|t| features::extract(&t.frames, params.set))
            .collect();
        let refs: Vec<&Seq> = seqs.iter().collect();
        let scale = channel_scale(&refs, params.set.dims());
        let max_distance = match params.max_distance {
            Some(d) => d,
            None => auto_threshold(templates, &seqs, &scale, params.band),
        };
        let templates = templates.iter().map(|t| t.word.clone()).zip(seqs).collect();
        Ok(Model {
            params,
            scale,
            templates,
            max_distance,
        })
    }

    pub fn classify(&self, frames: &[Frame]) -> Decision {
        let seq = features::extract(frames, self.params.set);
        let mut best: Vec<(String, f32)> = Vec::new();
        for (word, t) in &self.templates {
            let d = dtw::distance(&seq, t, &self.scale, self.params.band);
            match best.iter_mut().find(|(w, _)| w == word) {
                Some(entry) => entry.1 = entry.1.min(d),
                None => best.push((word.clone(), d)),
            }
        }
        best.sort_by(|a, b| a.1.total_cmp(&b.1));
        let (word, distance) = best[0].clone();
        let margin = match best.get(1) {
            Some((_, d2)) if *d2 > 0.0 && d2.is_finite() => (d2 - distance) / d2,
            _ => 1.0,
        };
        let reason = if !distance.is_finite() || distance > self.max_distance {
            Some("too far from every take")
        } else if margin < self.params.min_margin {
            Some("too close to two words")
        } else {
            None
        };
        Decision {
            word: reason.is_none().then(|| word.clone()),
            best: word,
            distance,
            margin,
            reason,
        }
    }
}

/// 1.5 × the 90th percentile of the distance from each take to its nearest
/// other take of the same word.
fn auto_threshold(templates: &[Template], seqs: &[Seq], scale: &[f32], band: f32) -> f32 {
    let n = seqs.len();
    let mut nearest = Vec::new();
    for i in 0..n {
        let d = (0..n)
            .filter(|&j| j != i && templates[j].word == templates[i].word)
            .map(|j| dtw::distance(&seqs[i], &seqs[j], scale, band))
            .fold(f32::INFINITY, f32::min);
        if d.is_finite() {
            nearest.push(d);
        }
    }
    if nearest.is_empty() {
        return f32::INFINITY;
    }
    nearest.sort_by(f32::total_cmp);
    let idx = ((nearest.len() as f32 * 0.9).ceil() as usize).clamp(1, nearest.len()) - 1;
    nearest[idx] * 1.5
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;

    /// A synthetic take: a bump on one rotation axis, with some jitter.
    pub fn take(word: &str, axis: usize, len: usize, jitter: i16) -> Template {
        let frames = (0..len + 40)
            .map(|i| {
                let mut rotation = [0i16; 3];
                if (20..20 + len).contains(&i) {
                    let x = (i - 20) as f32 / len as f32 * std::f32::consts::PI;
                    rotation[axis] = (x.sin() * 300.0) as i16 + jitter * ((i % 3) as i16 - 1);
                }
                Frame {
                    t: i as f64 * 0.01,
                    attitude: [-25000, -3000, -4000],
                    rotation,
                    acceleration: [0; 3],
                    gravity: [0, 1024, 0],
                }
            })
            .collect();
        Template {
            word: word.to_string(),
            recorded: 0,
            frames,
        }
    }

    #[test]
    fn recognizes_and_rejects() {
        let mut takes = Vec::new();
        for k in 0..4 {
            takes.push(take("oui", 0, 30 + k * 3, 5));
            takes.push(take("non", 1, 30 + k * 3, 5));
        }
        let model = Model::train(&takes, Params::default()).unwrap();
        let d = model.classify(&take("oui", 0, 36, 8).frames);
        assert_eq!(d.word.as_deref(), Some("oui"), "{d:?}");
        let d = model.classify(&take("non", 1, 33, 8).frames);
        assert_eq!(d.word.as_deref(), Some("non"), "{d:?}");
        // A movement on the third axis matches neither word.
        let d = model.classify(&take("?", 2, 33, 0).frames);
        assert_eq!(d.word, None, "{d:?}");
    }
}
