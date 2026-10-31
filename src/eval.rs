//! Does the motion stream tell the words apart? Leave-one-out accuracy of the
//! nearest-take recognizer, compared with the same procedure on shuffled
//! labels.

use anyhow::{Result, bail};

use crate::dataset::Template;
use crate::features::{self, FeatureSet, Seq};
use crate::model::{channel_scale, matrix};

#[derive(Clone, Debug)]
pub struct Report {
    pub set: FeatureSet,
    pub words: Vec<String>,
    pub takes: usize,
    pub accuracy: f32,
    /// Mean accuracy with shuffled labels.
    pub chance: f32,
    pub p_value: f32,
    pub permutations: usize,
    /// `confusion[truth][predicted]`.
    pub confusion: Vec<Vec<usize>>,
}

impl Report {
    pub fn significant(&self) -> bool {
        self.p_value < 0.05
    }
}

/// Small xorshift generator: the test only needs reproducible shuffles.
struct Rng(u64);

impl Rng {
    fn next(&mut self) -> u64 {
        self.0 ^= self.0 << 13;
        self.0 ^= self.0 >> 7;
        self.0 ^= self.0 << 17;
        self.0
    }

    fn shuffle<T>(&mut self, v: &mut [T]) {
        for i in (1..v.len()).rev() {
            let j = (self.next() % (i as u64 + 1)) as usize;
            v.swap(i, j);
        }
    }
}

pub fn evaluate(templates: &[Template], set: FeatureSet, band: f32, permutations: usize) -> Result<Report> {
    let mut words: Vec<String> = templates.iter().map(|t| t.word.clone()).collect();
    words.sort();
    words.dedup();
    let usable = words
        .iter()
        .filter(|w| templates.iter().filter(|t| &t.word == *w).count() >= 2)
        .count();
    if usable < 2 {
        bail!("record at least 2 takes of at least 2 words to evaluate");
    }
    let labels: Vec<usize> = templates
        .iter()
        .map(|t| words.iter().position(|w| *w == t.word).unwrap_or(0))
        .collect();
    let seqs: Vec<Seq> = templates.iter().map(|t| features::extract(&t.frames, set)).collect();
    let refs: Vec<&Seq> = seqs.iter().collect();
    let scale = channel_scale(&refs, set.dims());
    let dist = matrix(&seqs, &scale, band);
    let n = seqs.len();

    // The nearest neighbour does not depend on the labels, so each shuffle
    // only relabels it.
    let nearest: Vec<usize> = (0..n)
        .map(|i| {
            (0..n)
                .filter(|&j| j != i)
                .min_by(|&a, &b| dist[i * n + a].total_cmp(&dist[i * n + b]))
                .unwrap_or(i)
        })
        .collect();
    let accuracy_of = |lab: &[usize]| (0..n).filter(|&i| lab[nearest[i]] == lab[i]).count() as f32 / n as f32;

    let accuracy = accuracy_of(&labels);
    let mut confusion = vec![vec![0usize; words.len()]; words.len()];
    for i in 0..n {
        confusion[labels[i]][labels[nearest[i]]] += 1;
    }

    let mut rng = Rng(0x9e37_79b9_7f4a_7c15);
    let mut shuffled = labels.clone();
    let mut at_least = 0usize;
    let mut total = 0.0f32;
    for _ in 0..permutations {
        rng.shuffle(&mut shuffled);
        let acc = accuracy_of(&shuffled);
        total += acc;
        if acc >= accuracy {
            at_least += 1;
        }
    }
    Ok(Report {
        set,
        words,
        takes: n,
        accuracy,
        chance: if permutations > 0 {
            total / permutations as f32
        } else {
            0.0
        },
        p_value: (at_least + 1) as f32 / (permutations + 1) as f32,
        permutations,
        confusion,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::tests::take;

    #[test]
    fn separable_words_are_significant() {
        let mut takes = Vec::new();
        for k in 0..5 {
            takes.push(take("oui", 0, 30 + k * 2, 5));
            takes.push(take("non", 1, 30 + k * 2, 5));
            takes.push(take("stop", 2, 30 + k * 2, 5));
        }
        let r = evaluate(&takes, FeatureSet::default(), 0.15, 200).unwrap();
        assert_eq!(r.accuracy, 1.0);
        assert!(r.significant(), "{r:?}");
        assert!(r.chance < 0.5);
        assert_eq!(r.confusion[0].iter().sum::<usize>(), 5);
    }

    #[test]
    fn identical_words_are_not() {
        let mut takes = Vec::new();
        for k in 0..6 {
            takes.push(take(if k % 2 == 0 { "a" } else { "b" }, 0, 30 + k, 5));
        }
        let r = evaluate(&takes, FeatureSet::default(), 0.15, 200).unwrap();
        assert!(!r.significant(), "{r:?}");
    }

    #[test]
    fn needs_two_words() {
        let takes = vec![take("a", 0, 30, 0), take("a", 0, 31, 0)];
        assert!(evaluate(&takes, FeatureSet::default(), 0.15, 10).is_err());
    }
}
