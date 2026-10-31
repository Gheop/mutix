//! Dynamic time warping between two multichannel sequences.

use crate::features::Seq;

/// Mean per-step cost of the best alignment, restricted to a Sakoe–Chiba
/// band whose half-width is `band` × the longer length (and never narrower
/// than the length difference). Channels are multiplied by `scale` first.
pub fn distance(a: &Seq, b: &Seq, scale: &[f32], band: f32) -> f32 {
    if a.is_empty() || b.is_empty() {
        return f32::INFINITY;
    }
    let (n, m) = (a.len(), b.len());
    let width = ((band * n.max(m) as f32).ceil() as usize).max(n.abs_diff(m)).max(1);
    let mut prev = vec![f32::INFINITY; m + 1];
    let mut cur = vec![f32::INFINITY; m + 1];
    prev[0] = 0.0;
    for i in 1..=n {
        cur.fill(f32::INFINITY);
        // Row i of `a` maps onto the diagonal position in `b`.
        let centre = i * m / n;
        let lo = centre.saturating_sub(width).max(1);
        let hi = (centre + width).min(m);
        let ra = a.row(i - 1);
        for j in lo..=hi {
            let rb = b.row(j - 1);
            let cost = ra
                .iter()
                .zip(rb)
                .zip(scale)
                .map(|((x, y), s)| {
                    let d = (x - y) * s;
                    d * d
                })
                .sum::<f32>()
                .sqrt();
            cur[j] = cost + prev[j - 1].min(prev[j]).min(cur[j - 1]);
        }
        std::mem::swap(&mut prev, &mut cur);
    }
    prev[m] / (n + m) as f32
}

#[cfg(test)]
mod tests {
    use super::*;

    fn seq(v: &[f32]) -> Seq {
        Seq {
            dims: 1,
            data: v.to_vec(),
        }
    }

    #[test]
    fn identical_is_zero() {
        let a = seq(&[0.0, 1.0, 3.0, 1.0, 0.0]);
        assert_eq!(distance(&a, &a, &[1.0], 0.2), 0.0);
    }

    #[test]
    fn stretched_copy_is_closer_than_other_shape() {
        let a = seq(&[0.0, 1.0, 3.0, 1.0, 0.0, 0.0]);
        let slow = seq(&[0.0, 0.0, 1.0, 1.0, 3.0, 3.0, 1.0, 0.0, 0.0]);
        let other = seq(&[3.0, 0.0, 0.0, 0.0, 3.0, 0.0]);
        let s = [1.0];
        assert!(distance(&a, &slow, &s, 0.3) < distance(&a, &other, &s, 0.3));
    }

    #[test]
    fn symmetric_and_scaled() {
        let a = seq(&[0.0, 2.0, 0.0]);
        let b = seq(&[1.0, 1.0, 1.0, 0.0]);
        let d = distance(&a, &b, &[1.0], 0.5);
        assert!((d - distance(&b, &a, &[1.0], 0.5)).abs() < 1e-6);
        assert!((distance(&a, &b, &[2.0], 0.5) - 2.0 * d).abs() < 1e-5);
    }
}
