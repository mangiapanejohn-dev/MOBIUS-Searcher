//! A trained model as a lab rule. `scripts/direction_model.py train` writes
//! the file; this reads it, builds the same features from the candles (the
//! bar that closed and the ones before it, never after) and says how likely
//! the model thinks a rise is. The features and both kinds of model are
//! checked against the Python that trained them, on a fixture.

use super::rules::Bar;
use serde::Deserialize;

/// Bars of history the features look back over (a 672-bar deviation of
/// one-bar returns needs 673 closes).
pub const HISTORY: usize = 673;

const LOOKBACKS: [usize; 6] = [1, 4, 16, 96, 288, 672];

/// The features, in the order the model was trained on.
pub const FEATURES: [&str; 26] = [
    "sol_ret_1",
    "sol_ret_4",
    "sol_ret_16",
    "sol_ret_96",
    "sol_ret_288",
    "sol_ret_672",
    "z_96",
    "z_288",
    "z_672",
    "range_96",
    "range_672",
    "vol_ratio",
    "volume_z",
    "volume_burst",
    "bar_body",
    "bar_upper_wick",
    "btc_ret_1",
    "btc_ret_4",
    "btc_ret_96",
    "rel_4",
    "rel_96",
    "hour_sin",
    "hour_cos",
    "dow_sin",
    "dow_cos",
    "down_streak",
];

#[derive(Clone, Debug, Deserialize, PartialEq)]
#[serde(tag = "kind", rename_all = "kebab-case")]
pub enum Fitted {
    Logit { mean: Vec<f64>, sd: Vec<f64>, weights: Vec<f64> },
    Trees { base: f64, edges: Vec<Vec<f64>>, trees: Vec<Tree> },
}

#[derive(Clone, Debug, Deserialize, PartialEq)]
pub struct Tree {
    /// Per level, per node of that level: the feature and the bin a row must be above to go right.
    pub splits: Vec<Vec<Option<(usize, usize)>>>,
    pub leaves: Vec<f64>,
}

#[derive(Clone, Debug, Deserialize, PartialEq)]
pub struct Model {
    /// Day (UTC) the training data ends: before it the model has seen the answers.
    pub trained_to: String,
    /// The first bar it was not trained on (its start, ms), when the file says it to the bar.
    #[serde(default)]
    pub unseen_from_ms: Option<i64>,
    pub features: Vec<String>,
    /// Bars a position is held after the last signal.
    pub horizon_bars: usize,
    /// In when the model's probability of a rise is at least this.
    pub threshold: f64,
    pub model: Fitted,
}

fn mean(x: impl Iterator<Item = f64>, n: usize) -> f64 {
    x.sum::<f64>() / n as f64
}

/// Deviation of `x` around its mean; `ddof` 1 for a sample, 0 for the values themselves.
fn sd(x: &[f64], ddof: usize) -> f64 {
    let m = mean(x.iter().copied(), x.len());
    (x.iter().map(|v| (v - m).powi(2)).sum::<f64>() / (x.len() - ddof) as f64).sqrt()
}

/// The features at the close of the last of `bars`; `btc` holds the other
/// coin's closes for the same bars. `None` without enough history.
pub fn features(bars: &[Bar], btc: &[f64]) -> Option<[f64; 26]> {
    let n = bars.len();
    if n < HISTORY || btc.len() != n {
        return None;
    }
    let (bars, btc) = (&bars[n - HISTORY..], &btc[n - HISTORY..]);
    let last = bars[HISTORY - 1];
    let c: Vec<f64> = bars.iter().map(|b| b.close).collect();
    let r: Vec<f64> = c.windows(2).map(|w| (w[1] / w[0]).ln()).collect(); // 672 one-bar returns
    let rb: Vec<f64> = btc.windows(2).map(|w| (w[1] / w[0]).ln()).collect();
    let (dev, dev_btc) = (sd(&r, 1), sd(&rb, 1));
    let back = |x: &[f64], k: usize, dev: f64| (x[HISTORY - 1] / x[HISTORY - 1 - k]).ln() / (dev * (k as f64).sqrt());
    let mut f = [0.0; 26];
    for (i, k) in LOOKBACKS.iter().enumerate() {
        f[i] = back(&c, *k, dev);
    }
    for (i, k) in [96, 288, 672].into_iter().enumerate() {
        let w = &c[HISTORY - k..];
        f[6 + i] = (last.close - mean(w.iter().copied(), k)) / sd(w, 0);
    }
    for (i, k) in [96, 672].into_iter().enumerate() {
        let w = &bars[HISTORY - k..];
        let lo = w.iter().map(|b| b.low).fold(f64::MAX, f64::min);
        let hi = w.iter().map(|b| b.high).fold(f64::MIN, f64::max);
        f[9 + i] = (last.close - lo) / (hi - lo);
    }
    f[11] = sd(&r[r.len() - 16..], 1) / dev;
    let vol = |k: usize| mean(bars[HISTORY - k..].iter().map(|b| b.volume), k);
    f[12] = (vol(4) / vol(672)).ln();
    f[13] = ((last.volume + 1e-9) / vol(96)).ln();
    let span = last.high - last.low + 1e-12;
    f[14] = (last.close - last.open) / span;
    f[15] = (last.high - last.close.max(last.open)) / span;
    for (i, k) in [1, 4, 96].into_iter().enumerate() {
        f[16 + i] = back(btc, k, dev_btc);
    }
    f[19] = f[1] - f[17];
    f[20] = f[3] - f[18];
    let hour = (last.ts.div_euclid(3_600_000) % 24) as f64 + (last.ts.div_euclid(900_000) % 4) as f64 / 4.0;
    let dow = ((last.ts.div_euclid(86_400_000) + 4) % 7) as f64 + hour / 24.0;
    let turn = std::f64::consts::TAU;
    (f[21], f[22]) = ((turn * hour / 24.0).sin(), (turn * hour / 24.0).cos());
    (f[23], f[24]) = ((turn * dow / 7.0).sin(), (turn * dow / 7.0).cos());
    f[25] = r.iter().rev().take_while(|x| **x < 0.0).count().min(8) as f64 / 8.0;
    f.iter().all(|v| v.is_finite()).then_some(f)
}

impl Model {
    pub fn load(path: &std::path::Path) -> Result<Model, String> {
        let text = std::fs::read_to_string(path).map_err(|e| format!("{}: {e}", path.display()))?;
        let m: Model = serde_json::from_str(&text).map_err(|e| format!("{}: {e}", path.display()))?;
        if m.features != FEATURES {
            return Err(format!("{}: trained on other features than this version builds", path.display()));
        }
        if m.horizon_bars == 0 {
            return Err(format!("{}: horizon_bars is zero", path.display()));
        }
        Ok(m)
    }

    /// Start of the first bar the model has not been trained on, ms (the
    /// start of the `trained_to` day when the file does not say it to the bar).
    pub fn unseen_from(&self) -> i64 {
        if let Some(ms) = self.unseen_from_ms {
            return ms;
        }
        let mut p = self.trained_to.split('-').map(|x| x.parse::<i64>().unwrap_or(0));
        let (y, m, d) = (p.next().unwrap_or(0), p.next().unwrap_or(1), p.next().unwrap_or(1));
        // days from the civil date (the inverse of `stats::ymd`)
        let y = if m <= 2 { y - 1 } else { y };
        let era = y.div_euclid(400);
        let yoe = y.rem_euclid(400);
        let doy = (153 * (if m > 2 { m - 3 } else { m + 9 }) + 2) / 5 + d - 1;
        let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
        (era * 146_097 + doe - 719_468) * 86_400_000
    }

    /// The model's probability that the price is higher `horizon_bars` after the next bar opens.
    pub fn probability(&self, x: &[f64; 26]) -> f64 {
        let logistic = |m: f64| 1.0 / (1.0 + (-m).exp());
        match &self.model {
            Fitted::Logit { mean, sd, weights } => {
                let z = x.iter().zip(mean).zip(sd).map(|((x, m), s)| ((x - m) / s).clamp(-5.0, 5.0));
                logistic(weights[0] + z.zip(&weights[1..]).map(|(z, w)| z * w).sum::<f64>())
            }
            Fitted::Trees { base, edges, trees } => {
                // a value's bin: how many of the feature's edges lie below it
                let bin: Vec<usize> = x.iter().zip(edges).map(|(x, e)| e.partition_point(|edge| edge < x)).collect();
                let sum: f64 = trees
                    .iter()
                    .map(|t| {
                        let leaf = t.splits.iter().fold(0usize, |node, level| {
                            let right =
                                level.get(node).copied().flatten().is_some_and(|(feature, at)| bin[feature] > at);
                            node * 2 + usize::from(right)
                        });
                        t.leaves[leaf]
                    })
                    .sum();
                logistic(base + sum)
            }
        }
    }

    /// Whether the model wants in at the close of the last of `bars`.
    pub fn signal(&self, bars: &[Bar], btc: &[f64]) -> bool {
        let Some(last) = bars.last() else { return false };
        // on days it was trained on it has seen the answers: it stays out
        last.ts >= self.unseen_from() && features(bars, btc).is_some_and(|x| self.probability(&x) >= self.threshold)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[derive(Deserialize)]
    struct Fixture {
        /// ts, open, high, low, close, volume, btc close
        bars: Vec<(i64, f64, f64, f64, f64, f64, f64)>,
        /// What the Python that trains computed at the last bar.
        features: Vec<f64>,
        logit: Model,
        logit_probability: f64,
        trees: Model,
        trees_probability: f64,
    }

    fn fixture() -> (Vec<Bar>, Vec<f64>, Fixture) {
        let path = concat!(env!("CARGO_MANIFEST_DIR"), "/../../fixtures/lab/direction-model.json");
        let f: Fixture = serde_json::from_str(&std::fs::read_to_string(path).unwrap()).unwrap();
        let bars =
            f.bars.iter().map(|b| Bar { ts: b.0, open: b.1, high: b.2, low: b.3, close: b.4, volume: b.5 }).collect();
        let btc = f.bars.iter().map(|b| b.6).collect();
        (bars, btc, f)
    }

    #[test]
    fn the_features_are_the_ones_the_training_built() {
        let (bars, btc, f) = fixture();
        let x = features(&bars, &btc).expect("enough history");
        for ((name, ours), theirs) in FEATURES.iter().zip(x).zip(&f.features) {
            assert!((ours - theirs).abs() < 1e-9, "{name}: {ours} here, {theirs} in training");
        }
        assert_eq!(features(&bars[1..], &btc[1..]), None, "one bar short of the history: no features");
    }

    #[test]
    fn both_models_give_the_probability_the_training_gave() {
        let (bars, btc, f) = fixture();
        let x = features(&bars, &btc).unwrap();
        assert!((f.logit.probability(&x) - f.logit_probability).abs() < 1e-9, "{}", f.logit.probability(&x));
        assert!((f.trees.probability(&x) - f.trees_probability).abs() < 1e-9, "{}", f.trees.probability(&x));
    }

    #[test]
    fn a_model_stays_out_on_days_it_was_trained_on() {
        let (bars, btc, f) = fixture();
        let mut m = f.logit.clone();
        m.threshold = 0.0; // wants in whenever it may
        m.trained_to = "1999-01-01".into();
        assert!(m.signal(&bars, &btc));
        m.trained_to = "2999-01-01".into();
        assert!(!m.signal(&bars, &btc));
        assert_eq!(Model { trained_to: "1970-01-02".into(), ..m.clone() }.unseen_from(), 86_400_000);
        assert_eq!(Model { trained_to: "2026-10-04".into(), ..m.clone() }.unseen_from(), 1_791_072_000_000);
        assert_eq!(Model { unseen_from_ms: Some(42), ..m }.unseen_from(), 42, "to the bar when the file says so");
    }
}
