//! A synthetic command stream for benchmarking the matching engine.
//!
//! The mix is meant to look like a busy electronic book rather than a
//! best case: most messages are passive adds near the touch and cancels of
//! resting orders, with a steady trickle of amends and marketable orders.
//!
//! | share | message                                                        |
//! |-------|----------------------------------------------------------------|
//! | ~46%  | new limit order, at or behind the touch (geometric distance)   |
//! | ~38%  | cancel of a random live order                                  |
//! | 8%    | amend: half size-down in place, half a 1-2 tick reprice        |
//! | 8%    | marketable: IOC up to 2 ticks through, or market order         |
//!
//! The split between adds and cancels leans a few points one way or the other
//! to keep the number of resting orders near `target_depth`. A warm-up phase of plain adds builds the
//! book first; benchmarks apply it untimed.
//!
//! The stream is generated against a model book so cancels and amends name
//! orders that are live at that point, then replayed on a fresh book when
//! benchmarking. Replays are deterministic, so the benchmark book goes through
//! exactly the same states.

use rustc_hash::FxHashMap;

use crate::book::{BookConfig, OrderBook};
use crate::market_data::DepthView;
use crate::rng::Rng;
use crate::types::{Command, Event, NewOrder, OrderId, Price, Side};

#[derive(Clone, Debug)]
pub struct WorkloadConfig {
    pub ops: usize,
    pub seed: u64,
    /// Resting orders the stream tries to hover around.
    pub target_depth: usize,
    pub initial_price: Price,
}

impl Default for WorkloadConfig {
    fn default() -> Self {
        Self {
            ops: 1_000_000,
            seed: 1,
            target_depth: 5_000,
            initial_price: 100_000,
        }
    }
}

/// What the generated stream is made of.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Mix {
    pub adds: usize,
    pub cancels: usize,
    pub amends: usize,
    pub aggressive: usize,
}

struct Live {
    ids: Vec<OrderId>,
    pos: FxHashMap<OrderId, usize>,
}

impl Live {
    fn insert(&mut self, id: OrderId) {
        self.pos.insert(id, self.ids.len());
        self.ids.push(id);
    }

    fn remove(&mut self, id: OrderId) {
        if let Some(p) = self.pos.remove(&id) {
            self.ids.swap_remove(p);
            if p < self.ids.len() {
                self.pos.insert(self.ids[p], p);
            }
        }
    }
}

/// A generated benchmark stream.
#[derive(Clone, Debug)]
pub struct Workload {
    /// Adds that build the book up to `target_depth`. Apply these untimed.
    pub warmup: Vec<Command>,
    /// The measured stream.
    pub ops: Vec<Command>,
    pub mix: Mix,
}

/// Generates the stream and reports its composition.
pub fn generate(cfg: &WorkloadConfig) -> Workload {
    let mut rng = Rng::new(cfg.seed);
    let mut book = OrderBook::new(BookConfig {
        emit_book_updates: false,
        ..BookConfig::default()
    });
    let mut live = Live {
        ids: Vec::new(),
        pos: FxHashMap::default(),
    };
    let mut events = Vec::new();
    let mut warmup = Vec::with_capacity(cfg.target_depth);
    let mut out = Vec::with_capacity(cfg.ops);
    let mut mix = Mix::default();
    let mut next_id: OrderId = 1;
    let mut last_mid = cfg.initial_price;

    // Distance from the touch: 0 with probability ~0.35, then a geometric tail.
    let behind = |rng: &mut Rng| -> Price {
        let mut d = 0;
        while d < 30 && rng.chance(0.65) {
            d += 1;
        }
        d
    };

    while out.len() < cfg.ops {
        let warming = live.ids.len() < cfg.target_depth && out.is_empty();
        let side = if rng.chance(0.5) {
            Side::Bid
        } else {
            Side::Ask
        };
        // 8% marketable, 8% amends, and the rest split between adds and cancels,
        // leaning toward whichever keeps the book near its target depth.
        let r = if warming { 1.0 } else { rng.f64() };
        let add_given = if live.ids.len() < cfg.target_depth {
            0.62
        } else {
            0.53
        };
        let add = warming || live.ids.is_empty() || (r >= 0.16 && rng.chance(add_given));
        let cmd = if add {
            let touch = book
                .best(side)
                .map_or_else(|| last_mid - side.sign(), |l| l.price);
            // Occasionally improve the touch by a tick, if that does not cross.
            let improve = rng.chance(0.05) && book.top_of_book().spread().is_some_and(|s| s > 1);
            let price = if improve {
                touch + side.sign()
            } else {
                touch - side.sign() * behind(&mut rng)
            };
            let id = next_id;
            next_id += 1;
            if !warming {
                mix.adds += 1;
            }
            let owner = 1 + rng.below(64) as u32;
            Command::New(NewOrder::limit(id, owner, side, price, rng.range(1, 10)))
        } else if r >= 0.16 {
            mix.cancels += 1;
            let id = live.ids[rng.below(live.ids.len() as u64) as usize];
            Command::Cancel { id }
        } else if r >= 0.08 {
            mix.amends += 1;
            let id = live.ids[rng.below(live.ids.len() as u64) as usize];
            let o = book.order(id).expect("live order");
            if rng.chance(0.5) && o.qty > 1 {
                Command::Amend {
                    id,
                    price: o.price,
                    qty: rng.range(1, o.qty - 1),
                }
            } else {
                let delta = rng.range(1, 2) as Price;
                // Move away from the touch so amends mostly stay passive.
                Command::Amend {
                    id,
                    price: o.price - o.side.sign() * delta,
                    qty: o.qty,
                }
            }
        } else {
            mix.aggressive += 1;
            let id = next_id;
            next_id += 1;
            let owner = 1 + rng.below(64) as u32;
            let far = book.best(side.opposite()).map_or(last_mid, |l| l.price);
            if rng.chance(0.6) {
                let price = far + side.sign() * rng.below(3) as Price;
                Command::New(NewOrder::ioc(id, owner, side, price, rng.range(1, 10)))
            } else {
                Command::New(NewOrder::market(id, owner, side, rng.range(1, 8)))
            }
        };

        events.clear();
        book.process(&cmd, &mut events);
        for e in &events {
            match *e {
                Event::Trade {
                    maker_id,
                    maker_remaining: 0,
                    ..
                } => live.remove(maker_id),
                Event::Cancelled { id, .. } => live.remove(id),
                _ => {}
            }
        }
        if let Command::New(o) = cmd
            && book.order(o.id).is_some()
        {
            live.insert(o.id);
        }
        if let Some(mid) = book.mid() {
            last_mid = mid.round() as Price;
        }
        if warming {
            warmup.push(cmd);
        } else {
            out.push(cmd);
        }
    }
    Workload {
        warmup,
        ops: out,
        mix,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn stream_is_deterministic_and_close_to_the_intended_mix() {
        let cfg = WorkloadConfig {
            ops: 50_000,
            target_depth: 2_000,
            ..WorkloadConfig::default()
        };
        let w = generate(&cfg);
        assert_eq!(w.ops, generate(&cfg).ops);
        assert_eq!(w.warmup.len(), 2_000);
        assert_eq!(w.ops.len(), 50_000);
        let (a, mix) = (&w.ops, w.mix);
        let share = |n: usize| n as f64 / a.len() as f64;
        assert!((0.43..0.53).contains(&share(mix.adds)), "{mix:?}");
        assert!((0.31..0.41).contains(&share(mix.cancels)), "{mix:?}");
        assert!((0.07..0.09).contains(&share(mix.aggressive)), "{mix:?}");
        assert!((0.07..0.09).contains(&share(mix.amends)), "{mix:?}");

        // Replayed on a fresh book, nothing names a dead order.
        let mut book = OrderBook::default();
        let mut ev = Vec::new();
        let mut trades = 0;
        for c in w.warmup.iter().chain(a) {
            ev.clear();
            book.process(c, &mut ev);
            assert!(
                !ev.iter().any(|e| matches!(e, Event::Rejected { .. })),
                "{c:?}"
            );
            trades += ev
                .iter()
                .filter(|e| matches!(e, Event::Trade { .. }))
                .count();
        }
        assert!(trades > 1_000);
        assert!(
            (1_000..3_000).contains(&book.len()),
            "{} {mix:?}",
            book.len()
        );
    }
}
