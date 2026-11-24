//! Min / max / mean over any index range of a series in O(log n).
//!
//! Level 0 is the series itself; level k holds one entry per aligned block
//! of 2^k points. A range is covered by at most ~2·log2(n) blocks, so
//! drawing a chart costs about (columns × log n) regardless of how many
//! frames the analysis has.

pub struct Pyramid {
    levels: Vec<Level>,
}

struct Level {
    min: Vec<f64>,
    max: Vec<f64>,
    sum: Vec<f64>,
}

#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Agg {
    pub min: f64,
    pub max: f64,
    pub mean: f64,
    pub count: usize,
}

impl Pyramid {
    pub fn new(values: &[f64]) -> Self {
        let mut levels = vec![Level { min: values.to_vec(), max: values.to_vec(), sum: values.to_vec() }];
        while levels.last().unwrap().min.len() > 1 {
            let p = levels.last().unwrap();
            let n = p.min.len().div_ceil(2);
            let mut l = Level { min: Vec::with_capacity(n), max: Vec::with_capacity(n), sum: Vec::with_capacity(n) };
            for i in 0..n {
                let a = 2 * i;
                let b = (a + 1).min(p.min.len() - 1);
                if b == a {
                    l.min.push(p.min[a]);
                    l.max.push(p.max[a]);
                    l.sum.push(p.sum[a]);
                } else {
                    l.min.push(p.min[a].min(p.min[b]));
                    l.max.push(p.max[a].max(p.max[b]));
                    l.sum.push(p.sum[a] + p.sum[b]);
                }
            }
            levels.push(l);
        }
        Self { levels }
    }

    pub fn len(&self) -> usize {
        self.levels[0].min.len()
    }

    /// Aggregate of `values[i0..i1]`, `None` when empty.
    pub fn query(&self, i0: usize, i1: usize) -> Option<Agg> {
        let i1 = i1.min(self.len());
        if i0 >= i1 {
            return None;
        }
        let (mut min, mut max, mut sum) = (f64::INFINITY, f64::NEG_INFINITY, 0.0);
        let mut i = i0;
        while i < i1 {
            // Largest aligned block starting at i that fits in the range.
            let mut k = (i.trailing_zeros() as usize).min(self.levels.len() - 1);
            while k > 0 && i + (1 << k) > i1 {
                k -= 1;
            }
            let l = &self.levels[k];
            let j = i >> k;
            min = min.min(l.min[j]);
            max = max.max(l.max[j]);
            sum += l.sum[j];
            i += 1 << k;
        }
        let count = i1 - i0;
        Some(Agg { min, max, mean: sum / count as f64, count })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn brute(v: &[f64], a: usize, b: usize) -> Option<Agg> {
        let s = &v[a..b.min(v.len())];
        if s.is_empty() {
            return None;
        }
        Some(Agg {
            min: s.iter().cloned().fold(f64::INFINITY, f64::min),
            max: s.iter().cloned().fold(f64::NEG_INFINITY, f64::max),
            mean: s.iter().sum::<f64>() / s.len() as f64,
            count: s.len(),
        })
    }

    #[test]
    fn matches_brute_force_on_every_range() {
        for n in [1usize, 2, 3, 7, 8, 9, 31, 64, 100] {
            let v: Vec<f64> = (0..n).map(|i| ((i * 7919) % 101) as f64 - 50.0).collect();
            let p = Pyramid::new(&v);
            for a in 0..=n {
                for b in a..=n + 1 {
                    let (x, y) = (p.query(a, b), brute(&v, a, b));
                    match (x, y) {
                        (None, None) => {}
                        (Some(x), Some(y)) => {
                            assert_eq!((x.min, x.max, x.count), (y.min, y.max, y.count), "n={n} {a}..{b}");
                            assert!((x.mean - y.mean).abs() < 1e-9);
                        }
                        _ => panic!("n={n} {a}..{b}: {x:?} vs {y:?}"),
                    }
                }
            }
        }
    }

    #[test]
    fn feature_length_query_is_cheap() {
        let v: Vec<f64> = (0..130_000).map(|i| (i as f64 * 0.01).sin()).collect();
        let p = Pyramid::new(&v);
        let t = std::time::Instant::now();
        // A 4K-wide chart: 3840 column queries over the whole feature.
        let mut acc = 0.0;
        for rep in 0..10 {
            for c in 0..3840usize {
                let a = c * 130_000 / 3840;
                let b = (c + 1) * 130_000 / 3840 + rep;
                acc += p.query(a, b).unwrap().max;
            }
        }
        let per_frame = t.elapsed() / 10;
        assert!(acc.is_finite());
        eprintln!("3840 column queries over 130k points: {per_frame:?}");
        assert!(per_frame.as_millis() < 20, "{per_frame:?}");
    }
}
