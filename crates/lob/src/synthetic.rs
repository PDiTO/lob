//! A synthetic market: Poisson order flow around a latent fair value.
//!
//! The model is intentionally simple and every part of it is a knob:
//!
//! * A **fair value** moves as a random walk, or as an Ornstein-Uhlenbeck process
//!   that reverts to the starting price.
//! * **Limit orders** arrive at rate `limit_rate`, on a random side, at a distance
//!   from fair drawn from an exponential with mean `mean_offset` ticks. When fair
//!   value drifts past resting orders on the other side, new limit orders cross
//!   and trade, which is what moves the book.
//! * **Market orders** arrive at rate `market_rate`. With probability `informed`
//!   they trade toward fair value (buy when fair is above mid); otherwise the side
//!   is a coin flip. The informed share is what makes passive fills lose money on
//!   average, i.e. adverse selection.
//! * **Cancels**: every resting order is cancelled at rate `cancel_rate`, so the
//!   total cancel intensity scales with book size and depth settles near
//!   `limit_rate / cancel_rate` orders.
//!
//! Events are drawn with the Gillespie algorithm: exponential waiting time on the
//! total intensity, then pick which kind. The generator runs its own copy of the
//! matching engine so cancels always name orders that are actually live.

use rustc_hash::FxHashMap;
use serde::{Deserialize, Serialize};

use crate::book::{BookConfig, OrderBook};
use crate::feed::FeedEvent;
use crate::market_data::DepthView;
use crate::rng::Rng;
use crate::types::{Command, Event, NewOrder, OrderId, Price, Qty, Side};

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum FairValueModel {
    #[default]
    RandomWalk,
    MeanReverting,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct SyntheticConfig {
    pub seed: u64,
    pub duration_s: f64,
    pub initial_price: Price,
    pub fair_value: FairValueModel,
    /// Fair value volatility in ticks per sqrt(second).
    pub volatility: f64,
    /// Reversion speed per second (mean-reverting model only).
    pub mean_reversion: f64,
    /// Limit order arrivals per second.
    pub limit_rate: f64,
    /// Market order arrivals per second.
    pub market_rate: f64,
    /// Cancel rate per resting order per second.
    pub cancel_rate: f64,
    /// Mean distance of new limit orders from fair value, in ticks.
    pub mean_offset: f64,
    /// Limit order size is uniform in `1..=max_qty` lots.
    pub max_qty: Qty,
    /// Market order size is uniform in `1..=max_market_qty` lots.
    pub max_market_qty: Qty,
    /// Probability a market order trades toward fair value.
    pub informed: f64,
    /// Number of distinct owner ids to spread flow across.
    pub participants: u32,
    /// Levels per side seeded at t = 0 so the book does not start empty.
    pub initial_levels: u32,
}

impl Default for SyntheticConfig {
    fn default() -> Self {
        Self {
            seed: 1,
            duration_s: 600.0,
            initial_price: 10_000,
            fair_value: FairValueModel::RandomWalk,
            volatility: 2.0,
            mean_reversion: 0.05,
            limit_rate: 80.0,
            market_rate: 10.0,
            cancel_rate: 0.25,
            mean_offset: 5.0,
            max_qty: 10,
            max_market_qty: 12,
            informed: 0.5,
            participants: 500,
            initial_levels: 10,
        }
    }
}

/// Owner id reserved for the backtested strategy. Synthetic flow never uses it.
pub const STRATEGY_OWNER: u32 = 0;

/// Lazily generates a feed. Iterate it or collect it.
pub struct SyntheticMarket {
    cfg: SyntheticConfig,
    rng: Rng,
    book: OrderBook,
    events: Vec<Event>,
    live: Vec<OrderId>,
    live_pos: FxHashMap<OrderId, usize>,
    fair: f64,
    t: f64,
    end: f64,
    next_id: OrderId,
    pending: std::collections::VecDeque<FeedEvent>,
}

impl SyntheticMarket {
    pub fn new(cfg: SyntheticConfig) -> Self {
        let mut m = Self {
            rng: Rng::new(cfg.seed),
            book: OrderBook::new(BookConfig {
                emit_book_updates: false,
                ..BookConfig::default()
            }),
            events: Vec::new(),
            live: Vec::new(),
            live_pos: FxHashMap::default(),
            fair: cfg.initial_price as f64,
            t: 0.0,
            end: cfg.duration_s,
            next_id: 1,
            pending: Default::default(),
            cfg,
        };
        m.seed_book();
        m
    }

    /// Current latent fair value, in ticks.
    pub fn fair_value(&self) -> f64 {
        self.fair
    }

    fn owner(&mut self) -> u32 {
        1 + self.rng.below(self.cfg.participants.max(1) as u64) as u32
    }

    fn seed_book(&mut self) {
        let p0 = self.cfg.initial_price;
        for level in 1..=self.cfg.initial_levels as i64 {
            for side in [Side::Bid, Side::Ask] {
                for _ in 0..2 {
                    let price = p0 - side.sign() * level;
                    let qty = self.rng.range(1, self.cfg.max_qty.max(1));
                    let owner = self.owner();
                    let id = self.fresh_id();
                    self.emit(
                        0,
                        Command::New(NewOrder::limit(id, owner, side, price, qty)),
                    );
                }
            }
        }
    }

    fn fresh_id(&mut self) -> OrderId {
        let id = self.next_id;
        self.next_id += 1;
        id
    }

    fn add_live(&mut self, id: OrderId) {
        self.live_pos.insert(id, self.live.len());
        self.live.push(id);
    }

    fn remove_live(&mut self, id: OrderId) {
        if let Some(pos) = self.live_pos.remove(&id) {
            self.live.swap_remove(pos);
            if pos < self.live.len() {
                self.live_pos.insert(self.live[pos], pos);
            }
        }
    }

    /// Runs a command through the shadow book, tracks live ids and queues it.
    fn emit(&mut self, ts: u64, cmd: Command) {
        self.events.clear();
        self.book.process(&cmd, &mut self.events);
        for i in 0..self.events.len() {
            match self.events[i] {
                Event::Trade {
                    maker_id,
                    maker_remaining: 0,
                    ..
                } => self.remove_live(maker_id),
                Event::Cancelled { id, .. } => self.remove_live(id),
                _ => {}
            }
        }
        if let Command::New(o) = cmd
            && self.book.order(o.id).is_some()
        {
            self.add_live(o.id);
        }
        self.pending.push_back(FeedEvent { ts, cmd });
    }

    fn advance_fair(&mut self, dt: f64) {
        let sigma = self.cfg.volatility;
        match self.cfg.fair_value {
            FairValueModel::RandomWalk => self.fair += sigma * dt.sqrt() * self.rng.normal(),
            FairValueModel::MeanReverting => {
                let k = self.cfg.mean_reversion.max(1e-12);
                let mu = self.cfg.initial_price as f64;
                let decay = (-k * dt).exp();
                let sd = sigma * ((1.0 - decay * decay) / (2.0 * k)).sqrt();
                self.fair = mu + (self.fair - mu) * decay + sd * self.rng.normal();
            }
        }
    }

    fn step(&mut self) -> bool {
        let n = self.live.len() as f64;
        let (rl, rm, rc) = (
            self.cfg.limit_rate,
            self.cfg.market_rate,
            self.cfg.cancel_rate * n,
        );
        let total = rl + rm + rc;
        if total <= 0.0 {
            return false;
        }
        let dt = self.rng.exp(1.0 / total);
        self.t += dt;
        if self.t > self.end {
            return false;
        }
        self.advance_fair(dt);
        let ts = (self.t * 1e9) as u64;
        let u = self.rng.f64() * total;
        let owner = self.owner();

        if u < rl {
            let side = if self.rng.chance(0.5) {
                Side::Bid
            } else {
                Side::Ask
            };
            let d = self.rng.exp(self.cfg.mean_offset);
            let price = match side {
                Side::Bid => (self.fair - d).floor() as Price,
                Side::Ask => (self.fair + d).ceil() as Price,
            };
            let qty = self.rng.range(1, self.cfg.max_qty.max(1));
            let id = self.fresh_id();
            self.emit(
                ts,
                Command::New(NewOrder::limit(id, owner, side, price, qty)),
            );
        } else if u < rl + rm {
            let side = match self.book.mid() {
                Some(mid) if self.rng.chance(self.cfg.informed) && self.fair != mid => {
                    if self.fair > mid {
                        Side::Bid
                    } else {
                        Side::Ask
                    }
                }
                _ if self.rng.chance(0.5) => Side::Bid,
                _ => Side::Ask,
            };
            let qty = self.rng.range(1, self.cfg.max_market_qty.max(1));
            let id = self.fresh_id();
            self.emit(ts, Command::New(NewOrder::market(id, owner, side, qty)));
        } else if !self.live.is_empty() {
            let id = self.live[self.rng.below(self.live.len() as u64) as usize];
            self.emit(ts, Command::Cancel { id });
        }
        true
    }
}

impl Iterator for SyntheticMarket {
    type Item = FeedEvent;

    fn next(&mut self) -> Option<FeedEvent> {
        loop {
            if let Some(e) = self.pending.pop_front() {
                return Some(e);
            }
            if !self.step() {
                return None;
            }
        }
    }
}

/// Generates a whole feed up front.
pub fn generate(cfg: &SyntheticConfig) -> Vec<FeedEvent> {
    SyntheticMarket::new(cfg.clone()).collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn small() -> SyntheticConfig {
        SyntheticConfig {
            duration_s: 30.0,
            ..SyntheticConfig::default()
        }
    }

    #[test]
    fn same_seed_same_feed() {
        let a = generate(&small());
        let b = generate(&small());
        assert_eq!(a, b);
        let c = generate(&SyntheticConfig { seed: 2, ..small() });
        assert_ne!(a, c);
    }

    #[test]
    fn feed_is_time_ordered_and_replays_cleanly() {
        let feed = generate(&small());
        assert!(feed.len() > 1000);
        assert!(feed.windows(2).all(|w| w[0].ts <= w[1].ts));
        assert!(feed.last().unwrap().ts <= 30_000_000_000);

        // Replayed on its own, nothing is rejected: cancels always hit live orders.
        let mut book = OrderBook::default();
        let mut ev = Vec::new();
        let mut trades = 0;
        for e in &feed {
            ev.clear();
            book.process(&e.cmd, &mut ev);
            assert!(!ev.iter().any(|x| matches!(x, Event::Rejected { .. })));
            trades += ev
                .iter()
                .filter(|x| matches!(x, Event::Trade { .. }))
                .count();
            assert!(!book.is_crossed());
        }
        assert!(trades > 100, "only {trades} trades");
        let tob = book.top_of_book();
        assert!(tob.spread().is_some());
    }

    #[test]
    fn never_uses_the_strategy_owner() {
        for e in generate(&small()) {
            if let Command::New(o) = e.cmd {
                assert_ne!(o.owner, STRATEGY_OWNER);
            }
        }
    }

    #[test]
    fn mean_reverting_fair_value_stays_near_start() {
        let cfg = SyntheticConfig {
            fair_value: FairValueModel::MeanReverting,
            mean_reversion: 1.0,
            volatility: 2.0,
            duration_s: 200.0,
            ..SyntheticConfig::default()
        };
        let mut m = SyntheticMarket::new(cfg);
        let mut max_dev: f64 = 0.0;
        while m.next().is_some() {
            max_dev = max_dev.max((m.fair_value() - 10_000.0).abs());
        }
        // Stationary sd is 2 / sqrt(2) ~ 1.4 ticks; 10 would be a 7-sigma excursion.
        assert!(max_dev < 10.0, "max deviation {max_dev}");
    }
}
