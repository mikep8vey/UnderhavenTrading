use crate::config::RiskCfg;
use rust_decimal::Decimal;
use std::collections::HashMap;

#[derive(Debug, Clone, PartialEq)]
pub struct PositionRec {
    pub market_id: String,
    pub token_id: String,
    pub outcome_idx: usize,
    pub city: String,
    pub event_key: String,
    pub bracket: String,
    pub outcome: String,
    pub size: Decimal,
    pub avg_price: Decimal,
}

struct Pending {
    city: String,
    event_key: String,
    token_id: String,
    stake: Decimal,
}

/// In-memory book of positions, in-flight reservations and cash. Enforces every hard cap in risk.toml.
pub struct Exposure {
    cash: Decimal,
    peak: Decimal,
    realized: Decimal,
    positions: Vec<PositionRec>,
    pending: HashMap<u64, Pending>,
    next_id: u64,
    marks: HashMap<String, Decimal>,
}

impl Exposure {
    pub fn new(bankroll: Decimal) -> Exposure {
        Exposure {
            cash: bankroll,
            peak: bankroll,
            realized: Decimal::ZERO,
            positions: Vec::new(),
            pending: HashMap::new(),
            next_id: 1,
            marks: HashMap::new(),
        }
    }

    /// Rebuild after a restart from stored open positions and realized PnL (fees are not replayed).
    pub fn restore(bankroll: Decimal, positions: Vec<PositionRec>, realized: Decimal, peak: Decimal) -> Exposure {
        let mut e = Exposure::new(bankroll);
        let cost: Decimal = positions.iter().map(|p| p.avg_price * p.size).sum();
        e.cash = bankroll + realized - cost;
        e.realized = realized;
        e.positions = positions;
        if peak > e.peak {
            e.peak = peak;
        }
        e
    }

    pub fn positions(&self) -> &[PositionRec] {
        &self.positions
    }

    pub fn cash(&self) -> Decimal {
        self.cash
    }

    pub fn equity(&self) -> Decimal {
        let held: Decimal = self
            .positions
            .iter()
            .map(|p| p.size * self.marks.get(&p.token_id).copied().unwrap_or(p.avg_price))
            .sum();
        self.cash + held
    }

    /// Drawdown from peak equity as a fraction; also advances the peak.
    pub fn drawdown(&mut self) -> Decimal {
        let eq = self.equity();
        if eq > self.peak {
            self.peak = eq;
        }
        if self.peak <= Decimal::ZERO {
            return Decimal::ZERO;
        }
        (self.peak - eq) / self.peak
    }

    pub fn mark(&mut self, token: &str, price: Decimal) {
        self.marks.insert(token.to_string(), price);
    }

    pub fn last_mark(&self, token: &str) -> Option<Decimal> {
        self.marks.get(token).copied()
    }

    pub fn has_token(&self, token: &str) -> bool {
        self.positions.iter().any(|p| p.token_id == token) || self.pending.values().any(|p| p.token_id == token)
    }

    pub fn total_exposure(&self) -> Decimal {
        let held: Decimal = self.positions.iter().map(|p| p.avg_price * p.size).sum();
        let pend: Decimal = self.pending.values().map(|p| p.stake).sum();
        held + pend
    }

    pub fn event_exposure(&self, event_key: &str) -> Decimal {
        let held: Decimal =
            self.positions.iter().filter(|p| p.event_key == event_key).map(|p| p.avg_price * p.size).sum();
        let pend: Decimal = self.pending.values().filter(|p| p.event_key == event_key).map(|p| p.stake).sum();
        held + pend
    }

    pub fn city_count(&self, city: &str) -> usize {
        self.positions.iter().filter(|p| p.city == city).count()
            + self.pending.values().filter(|p| p.city == city).count()
    }

    /// Reserve capital for an order that is about to be worked. Enforces: no duplicate market,
    /// max positions per city, max capital per event, max total weather exposure.
    pub fn try_reserve(
        &mut self,
        lim: &RiskCfg,
        city: &str,
        event_key: &str,
        token_id: &str,
        stake: Decimal,
    ) -> Result<u64, String> {
        if self.has_token(token_id) {
            return Err("already positioned or working an order in this market".into());
        }
        let eq = self.equity();
        if self.city_count(city) >= lim.max_positions_per_city {
            return Err(format!("max {} positions in {city}", lim.max_positions_per_city));
        }
        if self.event_exposure(event_key) + stake > lim.max_event_pct * eq {
            return Err("max capital per event".into());
        }
        if self.total_exposure() + stake > lim.max_total_exposure_pct * eq {
            return Err("max total weather exposure".into());
        }
        let id = self.next_id;
        self.next_id += 1;
        self.pending.insert(
            id,
            Pending { city: city.to_string(), event_key: event_key.to_string(), token_id: token_id.to_string(), stake },
        );
        Ok(id)
    }

    pub fn release(&mut self, id: u64) {
        self.pending.remove(&id);
    }

    /// Apply a fill (`delta.size`, `delta.avg_price` = fill price). `cost` = notional + fees. Returns merged position.
    pub fn add_fill(&mut self, delta: &PositionRec, cost: Decimal) -> PositionRec {
        self.cash -= cost;
        if let Some(p) = self.positions.iter_mut().find(|p| p.token_id == delta.token_id) {
            let total = p.size + delta.size;
            if total > Decimal::ZERO {
                p.avg_price = (p.avg_price * p.size + delta.avg_price * delta.size) / total;
            }
            p.size = total;
            return p.clone();
        }
        self.positions.push(delta.clone());
        delta.clone()
    }

    /// Settle a position at `payout` per share. Returns the closed position and its PnL.
    pub fn resolve(&mut self, token: &str, payout: Decimal) -> Option<(PositionRec, Decimal)> {
        let idx = self.positions.iter().position(|p| p.token_id == token)?;
        let p = self.positions.remove(idx);
        let proceeds = payout * p.size;
        self.cash += proceeds;
        let pnl = proceeds - p.avg_price * p.size;
        self.realized += pnl;
        self.marks.remove(token);
        Some((p, pnl))
    }
}
