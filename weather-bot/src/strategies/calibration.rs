//! Probability calibration and market shrinkage for the forecast strategy.
//!
//! Ensembles are usually over-confident. Two corrections are applied to the raw bucket probability:
//!  1. Calibration: a reliability table learned from settled markets (ten buckets of predicted probability, each
//!     compared with how often YES really won). With few samples a bucket stays close to the raw probability
//!     (prior of `prior` pseudo-observations), and the whole table is switched off until `min_total` forecasts settled.
//!  2. Shrinkage: p = w * calibrated + (1 - w) * market YES mid, so the bot only acts on the part of its view that the
//!     market does not already price.
use crate::util::math::dec_from_f64;
use rust_decimal::prelude::ToPrimitive;
use rust_decimal::Decimal;
use std::collections::{BTreeMap, HashMap};

pub const BINS: usize = 10;

#[derive(Debug, Clone)]
pub struct Calibrator {
    n: [u32; BINS],
    hits: [u32; BINS],
    pred_sum: [f64; BINS],
    total: u32,
    brier_sum: f64,
    min_total: u32,
    prior: f64,
}

#[derive(Debug, Clone)]
pub struct BinReport {
    pub lo: f64,
    pub hi: f64,
    pub n: u32,
    pub predicted: f64,
    pub observed: f64,
}

fn bin_of(p: f64) -> usize {
    ((p.clamp(0.0, 1.0) * BINS as f64) as usize).min(BINS - 1)
}

impl Calibrator {
    pub fn identity(min_total: u32, prior: f64) -> Calibrator {
        Calibrator { n: [0; BINS], hits: [0; BINS], pred_sum: [0.0; BINS], total: 0, brier_sum: 0.0, min_total, prior }
    }

    /// Learn from (raw probability of YES, did YES win) pairs.
    pub fn fit(rows: &[(f64, bool)], min_total: u32, prior: f64) -> Calibrator {
        let mut c = Calibrator::identity(min_total, prior);
        for (p, won) in rows {
            let b = bin_of(*p);
            c.n[b] += 1;
            if *won {
                c.hits[b] += 1;
            }
            c.pred_sum[b] += *p;
            c.total += 1;
            let o = if *won { 1.0 } else { 0.0 };
            c.brier_sum += (*p - o) * (*p - o);
        }
        c
    }

    pub fn active(&self) -> bool {
        self.total >= self.min_total
    }
    pub fn total(&self) -> u32 {
        self.total
    }
    pub fn min_total(&self) -> u32 {
        self.min_total
    }
    /// Mean squared error of the RAW probabilities on settled forecasts (lower is better; 0.25 = coin flip).
    pub fn brier(&self) -> Option<f64> {
        if self.total == 0 {
            None
        } else {
            Some(self.brier_sum / self.total as f64)
        }
    }

    pub fn apply(&self, p: f64) -> f64 {
        if !self.active() {
            return p;
        }
        let b = bin_of(p);
        let n = self.n[b] as f64;
        ((self.hits[b] as f64 + self.prior * p) / (n + self.prior)).clamp(0.0, 1.0)
    }

    pub fn apply_dec(&self, p: Decimal) -> Decimal {
        dec_from_f64(self.apply(p.to_f64().unwrap_or(0.5)))
    }

    pub fn report(&self) -> Vec<BinReport> {
        (0..BINS)
            .map(|b| {
                let n = self.n[b];
                BinReport {
                    lo: b as f64 / BINS as f64,
                    hi: (b + 1) as f64 / BINS as f64,
                    n,
                    predicted: if n > 0 { self.pred_sum[b] / n as f64 } else { 0.0 },
                    observed: if n > 0 { self.hits[b] as f64 / n as f64 } else { 0.0 },
                }
            })
            .collect()
    }
}

/// Brier scores on settled forecasts (lower is better): raw model, market YES mid, and the bot's blend
/// (calibrated model shrunk toward the market). The honest question is whether `model` beats `market`.
#[derive(Debug, Clone, Default)]
pub struct Scores {
    pub n: u32,
    pub model: f64,
    pub market: f64,
    pub blend: f64,
}

/// rows are (raw model probability of YES, market YES mid at forecast time, did YES win).
pub fn score(rows: &[(f64, f64, bool)], cal: &Calibrator, w: f64) -> Scores {
    if rows.is_empty() {
        return Scores::default();
    }
    let w = w.clamp(0.0, 1.0);
    let (mut m, mut k, mut b) = (0.0f64, 0.0f64, 0.0f64);
    for (p, pm, won) in rows {
        let o = if *won { 1.0 } else { 0.0 };
        let blend = w * cal.apply(*p) + (1.0 - w) * *pm;
        m += (*p - o) * (*p - o);
        k += (*pm - o) * (*pm - o);
        b += (blend - o) * (blend - o);
    }
    let n = rows.len() as f64;
    Scores { n: rows.len() as u32, model: m / n, market: k / n, blend: b / n }
}

/// Brier scores where every **city-day** counts once, instead of every bracket counting once.
/// A city-day with ten brackets and one city-day with two therefore carry the same weight, which is
/// the right unit for "does this model forecast weather days better than the market?".
#[derive(Debug, Clone, Default, PartialEq)]
pub struct ClusterScores {
    pub forecasts: u32,
    pub clusters: u32,
    pub model: f64,
    pub market: f64,
    pub blend: f64,
}

#[derive(Default)]
struct ClusterAcc {
    model: f64,
    market: f64,
    blend: f64,
    n: u32,
}

fn cluster_finish(map: HashMap<(String, String), ClusterAcc>) -> ClusterScores {
    if map.is_empty() {
        return ClusterScores::default();
    }
    let mut forecasts = 0u32;
    let (mut m, mut k, mut b) = (0.0f64, 0.0f64, 0.0f64);
    for a in map.values() {
        if a.n == 0 {
            continue;
        }
        forecasts += a.n;
        let n = a.n as f64;
        m += a.model / n;
        k += a.market / n;
        b += a.blend / n;
    }
    let clusters = map.values().filter(|a| a.n > 0).count() as u32;
    if clusters == 0 {
        return ClusterScores::default();
    }
    ClusterScores {
        forecasts,
        clusters,
        model: m / clusters as f64,
        market: k / clusters as f64,
        blend: b / clusters as f64,
    }
}

/// In-sample, city-day-clustered Brier. Rows are `(city, event_date, p_raw, p_market, yes_won)`.
pub fn score_clustered(
    rows: &[(String, String, f64, f64, bool)],
    cal: &Calibrator,
    w: f64,
) -> ClusterScores {
    let w = w.clamp(0.0, 1.0);
    let mut map: HashMap<(String, String), ClusterAcc> = HashMap::new();
    for (city, date, p, pm, won) in rows {
        let o = if *won { 1.0 } else { 0.0 };
        let blend = w * cal.apply(*p) + (1.0 - w) * *pm;
        let e = map.entry((city.clone(), date.clone())).or_default();
        e.model += (*p - o).powi(2);
        e.market += (*pm - o).powi(2);
        e.blend += (blend - o).powi(2);
        e.n += 1;
    }
    cluster_finish(map)
}

/// Walk-forward, city-day-clustered Brier. For each event date the calibrator is fitted **only on rows
/// from earlier dates**, then applied to that date's forecasts. This is the honest comparison against
/// the market price (the in-sample `score` is optimistically biased because the calibrator saw those
/// same rows). The first date uses an identity calibrator.
pub fn walk_forward(
    rows: &[(String, String, f64, f64, bool)],
    min_total: u32,
    prior: f64,
    w: f64,
) -> ClusterScores {
    let w = w.clamp(0.0, 1.0);
    let mut by_date: BTreeMap<String, Vec<&(String, String, f64, f64, bool)>> = BTreeMap::new();
    for r in rows {
        by_date.entry(r.1.clone()).or_default().push(r);
    }
    let mut prior_pairs: Vec<(f64, bool)> = Vec::new();
    let mut map: HashMap<(String, String), ClusterAcc> = HashMap::new();
    for (date, group) in by_date.iter() {
        let cal = Calibrator::fit(&prior_pairs, min_total, prior);
        for r in group.iter() {
            let o = if r.4 { 1.0 } else { 0.0 };
            let blend = w * cal.apply(r.2) + (1.0 - w) * r.3;
            let e = map.entry((r.0.clone(), date.clone())).or_default();
            e.model += (r.2 - o).powi(2);
            e.market += (r.3 - o).powi(2);
            e.blend += (blend - o).powi(2);
            e.n += 1;
        }
        for r in group {
            prior_pairs.push((r.2, r.4));
        }
    }
    cluster_finish(map)
}

/// p = w * p_model + (1 - w) * p_market, with w clamped to [0, 1].
pub fn shrink(p_model: Decimal, p_market: Decimal, w: Decimal) -> Decimal {
    let w = w.max(Decimal::ZERO).min(Decimal::ONE);
    w * p_model + (Decimal::ONE - w) * p_market
}
