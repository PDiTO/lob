//! Backtester behaviour: latency, queue position, accounting and determinism.

use lob::backtest::{self, Ctx, Fill, Liquidity, OrderUpdate, SimConfig, Strategy};
use lob::config::{BacktestConfig, StrategyConfig};
use lob::feed::FeedEvent;
use lob::strategies::{MarketMakerConfig, MomentumConfig};
use lob::synthetic::SyntheticConfig;
use lob::{Command, DepthView, NewOrder, OrderType, Placement, Price, Qty, Side};

const MS: u64 = 1_000_000;

/// Sends a fixed list of orders at start, then records everything it is told.
#[derive(Default)]
struct Script {
    orders: Vec<(Side, OrderType, Price, Qty)>,
    fills: Vec<(u64, Fill)>,
    updates: Vec<(u64, OrderUpdate)>,
    book_updates: Vec<(u64, Option<Price>, Option<Price>)>,
}

impl Script {
    fn sending(orders: &[(Side, OrderType, Price, Qty)]) -> Self {
        Self {
            orders: orders.to_vec(),
            ..Self::default()
        }
    }

    fn filled(&self) -> Qty {
        self.fills.iter().map(|(_, f)| f.qty).sum()
    }
}

impl Strategy for Script {
    fn on_start(&mut self, ctx: &mut Ctx) {
        for &(side, ty, price, qty) in &self.orders {
            ctx.submit(side, ty, price, qty);
        }
    }

    fn on_book_update(&mut self, ctx: &mut Ctx) {
        let tob = ctx.book().top_of_book();
        self.book_updates.push((
            ctx.now(),
            tob.bid.map(|l| l.price),
            tob.ask.map(|l| l.price),
        ));
    }

    fn on_fill(&mut self, ctx: &mut Ctx, fill: &Fill) {
        self.fills.push((ctx.now(), *fill));
    }

    fn on_order_update(&mut self, ctx: &mut Ctx, update: &OrderUpdate) {
        self.updates.push((ctx.now(), *update));
    }
}

fn at(ts: u64, cmd: Command) -> FeedEvent {
    FeedEvent { ts, cmd }
}

fn limit(id: u64, side: Side, price: Price, qty: Qty) -> Command {
    // Distinct owners so feed orders never trip self-trade prevention.
    Command::New(NewOrder::limit(id, 100 + id as u32, side, price, qty))
}

fn market(id: u64, side: Side, qty: Qty) -> Command {
    Command::New(NewOrder::market(id, 100 + id as u32, side, qty))
}

fn sim(order_latency: u64, md_latency: u64) -> SimConfig {
    SimConfig {
        order_latency_ns: order_latency,
        market_data_latency_ns: md_latency,
        maker_fee: 0.0,
        taker_fee: 0.0,
        timer_interval_ns: 0,
        ..SimConfig::default()
    }
}

// ---------------------------------------------------------------------------------
// Latency
// ---------------------------------------------------------------------------------

#[test]
fn order_latency_delays_arrival_at_the_exchange() {
    let feed = vec![
        at(0, limit(1, Side::Ask, 101, 5)),
        at(20 * MS, limit(2, Side::Bid, 90, 1)),
    ];
    let mut s = Script::sending(&[(Side::Bid, OrderType::Ioc, 101, 2)]);
    backtest::run(feed, &mut s, &sim(3 * MS, 0));
    assert_eq!(s.filled(), 2);
    // Sent at t = 0, reaches the exchange at t = 3 ms.
    assert_eq!(s.fills[0].1.exchange_ts, 3 * MS);
    assert_eq!(s.fills[0].1.liquidity, Liquidity::Taker);
}

#[test]
fn market_data_latency_delays_what_the_strategy_hears() {
    let feed = vec![
        at(0, limit(1, Side::Ask, 101, 5)),
        at(20 * MS, limit(2, Side::Bid, 90, 1)),
        // The run ends at the last feed message, so give deliveries time to land.
        at(100 * MS, limit(3, Side::Bid, 80, 1)),
    ];
    let mut s = Script::sending(&[(Side::Bid, OrderType::Ioc, 101, 2)]);
    backtest::run(feed, &mut s, &sim(3 * MS, 5 * MS));
    // Fill at 3 ms on the exchange, the strategy learns at 8 ms.
    assert_eq!(s.fills[0].0, 8 * MS);
    // The first book update (the feed ask at t = 0) arrives at 5 ms.
    assert_eq!(s.book_updates[0], (5 * MS, None, Some(101)));
    // The bid at 20 ms shows up at 25 ms.
    assert!(s.book_updates.contains(&(25 * MS, Some(90), Some(101))));
}

#[test]
fn slow_order_misses_liquidity_that_a_fast_one_gets() {
    // The ask is cancelled 2 ms in. A 1 ms order gets it; a 3 ms order does not.
    let feed = || {
        vec![
            at(0, limit(1, Side::Ask, 101, 5)),
            at(2 * MS, Command::Cancel { id: 1 }),
            at(10 * MS, limit(2, Side::Bid, 90, 1)),
        ]
    };
    let order = [(Side::Bid, OrderType::Ioc, 101, 5)];

    let mut fast = Script::sending(&order);
    let r = backtest::run(feed(), &mut fast, &sim(MS, 0));
    assert_eq!(fast.filled(), 5);
    // The feed's cancel then names an order we already took.
    assert_eq!(r.summary.feed_rejects, 1);

    let mut slow = Script::sending(&order);
    let r = backtest::run(feed(), &mut slow, &sim(3 * MS, 0));
    assert_eq!(slow.filled(), 0);
    assert!(
        slow.updates
            .iter()
            .any(|(_, u)| matches!(u, OrderUpdate::Cancelled { qty: 5, .. }))
    );
    assert_eq!(r.summary.feed_rejects, 0);
}

#[test]
fn feed_wins_ties_with_strategy_orders() {
    let feed = vec![
        at(0, limit(1, Side::Ask, 101, 5)),
        at(MS, market(2, Side::Bid, 5)),
        at(5 * MS, limit(3, Side::Bid, 90, 1)),
    ];
    let mut s = Script::sending(&[(Side::Bid, OrderType::Ioc, 101, 5)]);
    backtest::run(feed, &mut s, &sim(MS, 0));
    assert_eq!(
        s.filled(),
        0,
        "the feed market order arrived at the same ns and went first"
    );
}

// ---------------------------------------------------------------------------------
// Queue position
// ---------------------------------------------------------------------------------

/// Ten lots rest at 101 ahead of us; then market buys of 4 lots every 10 ms.
fn queue_feed() -> Vec<FeedEvent> {
    let mut feed = vec![
        at(0, limit(1, Side::Ask, 101, 6)),
        at(0, limit(2, Side::Ask, 101, 4)),
        at(0, limit(3, Side::Bid, 99, 50)),
    ];
    for i in 0..5 {
        feed.push(at((i + 1) * 10 * MS, market(10 + i, Side::Bid, 4)));
    }
    feed
}

#[test]
fn passive_order_waits_for_the_queue_ahead() {
    let mut s = Script::sending(&[(Side::Ask, OrderType::Limit, 101, 5)]);
    backtest::run(queue_feed(), &mut s, &sim(MS, 0));
    // 10 lots ahead: the first two market orders (8 lots) do not reach us, the
    // third takes the last 2 ahead and 2 of ours, the fourth takes our last 3.
    let fills: Vec<_> = s
        .fills
        .iter()
        .map(|(_, f)| (f.exchange_ts, f.qty))
        .collect();
    assert_eq!(fills, vec![(30 * MS, 2), (40 * MS, 3)]);
    assert!(s.fills.iter().all(|(_, f)| f.liquidity == Liquidity::Maker));
}

#[test]
fn front_of_queue_fills_first() {
    let mut s = Script::sending(&[(Side::Ask, OrderType::Limit, 101, 5)]);
    let cfg = SimConfig {
        queue_position: Placement::Front,
        ..sim(MS, 0)
    };
    backtest::run(queue_feed(), &mut s, &cfg);
    let fills: Vec<_> = s
        .fills
        .iter()
        .map(|(_, f)| (f.exchange_ts, f.qty))
        .collect();
    assert_eq!(fills, vec![(10 * MS, 4), (20 * MS, 1)]);
}

#[test]
fn cancels_ahead_of_us_move_us_up() {
    let mut feed = queue_feed();
    // Order 1 (6 lots, first in line) is cancelled at 5 ms, leaving 4 ahead.
    feed.insert(3, at(5 * MS, Command::Cancel { id: 1 }));
    let mut s = Script::sending(&[(Side::Ask, OrderType::Limit, 101, 5)]);
    backtest::run(feed, &mut s, &sim(MS, 0));
    let fills: Vec<_> = s
        .fills
        .iter()
        .map(|(_, f)| (f.exchange_ts, f.qty))
        .collect();
    assert_eq!(fills, vec![(20 * MS, 4), (30 * MS, 1)]);
}

#[test]
fn order_sent_later_queues_behind_orders_that_arrived_earlier() {
    // With 15 ms of latency our order lands after the market order at 10 ms and
    // after a new feed order at 12 ms, which is now ahead of us.
    let mut feed = queue_feed();
    feed.insert(4, at(12 * MS, limit(4, Side::Ask, 101, 3)));
    let mut s = Script::sending(&[(Side::Ask, OrderType::Limit, 101, 5)]);
    backtest::run(feed, &mut s, &sim(15 * MS, 0));
    // Ahead at arrival: 6 + 4 - 4 + 3 = 9 lots. Market orders at 20, 30, 40 ms take
    // 12 lots: 9 ahead, then 3 of ours; the one at 50 ms takes the last 2.
    let fills: Vec<_> = s
        .fills
        .iter()
        .map(|(_, f)| (f.exchange_ts, f.qty))
        .collect();
    assert_eq!(fills, vec![(40 * MS, 3), (50 * MS, 2)]);
}

// ---------------------------------------------------------------------------------
// Accounting
// ---------------------------------------------------------------------------------

fn mm_config(seed: u64) -> BacktestConfig {
    BacktestConfig {
        market: SyntheticConfig {
            seed,
            duration_s: 120.0,
            ..SyntheticConfig::default()
        },
        strategy: StrategyConfig::MarketMaker(MarketMakerConfig::default()),
        ..BacktestConfig::default()
    }
}

#[test]
fn pnl_accounting_identity_holds() {
    for cfg in [
        mm_config(1),
        mm_config(2),
        BacktestConfig {
            strategy: StrategyConfig::Momentum(MomentumConfig::default()),
            ..mm_config(3)
        },
    ] {
        let r = cfg.run().unwrap();
        let s = &r.summary;
        assert!(s.fills > 10, "strategy barely traded: {s}");
        let mark = s.final_mark.unwrap();

        // cash + inventory * mark == realized + unrealized - fees
        let lhs = s.cash + s.final_position as f64 * mark;
        let rhs = s.realized_pnl + s.unrealized_pnl - s.fees;
        assert!((lhs - rhs).abs() < 1e-6, "{lhs} != {rhs}");
        assert!((s.net_pnl - rhs).abs() < 1e-6);

        // Rebuild cash and position independently from the fill log.
        let (mut cash, mut pos, mut fees) = (0.0, 0i64, 0.0);
        for f in &r.fills {
            let fee = match f.liquidity {
                Liquidity::Maker => cfg.sim.maker_fee,
                Liquidity::Taker => cfg.sim.taker_fee,
            } * f.qty as f64;
            cash -= f.side.sign() as f64 * f.price as f64 * f.qty as f64 + fee;
            fees += fee;
            pos += f.side.sign() * f.qty as i64;
        }
        assert_eq!(pos, s.final_position);
        assert!((cash - s.cash).abs() < 1e-6);
        assert!((fees - s.fees).abs() < 1e-6);
        assert_eq!(
            r.fills.iter().map(|f| f.qty).sum::<Qty>(),
            s.maker_qty + s.taker_qty
        );
    }
}

#[test]
fn realized_pnl_is_price_difference_on_a_round_trip() {
    // Buy 5 at 101 (taking), sell 5 at 99 (taking): realized -10, flat, fees 2 * 5 * 0.5.
    let feed = vec![
        at(0, limit(1, Side::Ask, 101, 5)),
        at(0, limit(2, Side::Bid, 99, 5)),
        at(50 * MS, limit(3, Side::Bid, 98, 1)),
        at(50 * MS, limit(4, Side::Ask, 102, 1)),
    ];
    let mut s = Script::sending(&[
        (Side::Bid, OrderType::Ioc, 101, 5),
        (Side::Ask, OrderType::Ioc, 99, 5),
    ]);
    let cfg = SimConfig {
        taker_fee: 0.5,
        ..sim(MS, 0)
    };
    let r = backtest::run(feed, &mut s, &cfg);
    assert_eq!(r.summary.final_position, 0);
    assert_eq!(r.summary.realized_pnl, -10.0);
    assert_eq!(r.summary.fees, 5.0);
    assert_eq!(r.summary.net_pnl, -15.0);
    assert_eq!(r.summary.taker_qty, 10);
}

#[test]
fn strategy_position_catches_up_with_the_exchange() {
    let cfg = mm_config(4);
    let feed: Vec<_> = cfg.feed().unwrap().collect();

    struct Watch {
        inner: Box<dyn Strategy>,
        last_seen: i64,
    }
    impl Strategy for Watch {
        fn on_book_update(&mut self, ctx: &mut Ctx) {
            self.inner.on_book_update(ctx);
        }
        fn on_fill(&mut self, ctx: &mut Ctx, fill: &Fill) {
            self.last_seen = ctx.position();
            self.inner.on_fill(ctx, fill);
        }
    }
    let mut w = Watch {
        inner: cfg.strategy.build(),
        last_seen: 0,
    };
    let r = backtest::run(feed, &mut w, &cfg.sim);
    assert_eq!(w.last_seen, r.summary.final_position);
}

// ---------------------------------------------------------------------------------
// Determinism
// ---------------------------------------------------------------------------------

#[test]
fn same_seed_same_result() {
    let a = mm_config(11).run().unwrap();
    let b = mm_config(11).run().unwrap();
    assert_eq!(a, b);
    let c = mm_config(12).run().unwrap();
    assert_ne!(a.summary, c.summary);
}

#[test]
fn replaying_a_saved_feed_matches_the_live_generator() {
    let cfg = mm_config(5);
    let feed: Vec<_> = cfg.feed().unwrap().collect();
    let mut csv = Vec::new();
    lob::feed::write_csv(&mut csv, feed.iter().copied()).unwrap();
    let replayed = lob::feed::read_csv(csv.as_slice()).unwrap();
    assert_eq!(replayed, feed);

    let mut s1 = cfg.strategy.build();
    let mut s2 = cfg.strategy.build();
    let live = cfg.run_with(s1.as_mut()).unwrap();
    let from_csv = backtest::run(replayed, s2.as_mut(), &cfg.sim);
    assert_eq!(live, from_csv);
}

// ---------------------------------------------------------------------------------
// Misc
// ---------------------------------------------------------------------------------

#[test]
fn markouts_and_samples_are_produced() {
    let r = mm_config(6).run().unwrap();
    let s = &r.summary;
    assert_eq!(s.markouts.len(), 3);
    assert!(s.markouts[0].lots > 0);
    // Samples every second over 120 s, plus the start and the end.
    assert!(
        (120..=123).contains(&r.samples.len()),
        "{}",
        r.samples.len()
    );
    assert!(s.max_drawdown >= 0.0);
    assert!(s.fill_ratio > 0.0 && s.fill_ratio <= 1.0);
    assert!(s.max_abs_position <= 50);
}

#[test]
fn stop_ends_the_run_early() {
    struct Stopper;
    impl Strategy for Stopper {
        fn on_book_update(&mut self, ctx: &mut Ctx) {
            if ctx.now() > 5_000 * MS {
                ctx.stop();
            }
        }
        fn on_fill(&mut self, _: &mut Ctx, _: &Fill) {}
    }
    let r = mm_config(7).run_with(&mut Stopper).unwrap();
    assert!(r.summary.duration_s < 6.0);
}

#[test]
fn the_view_is_the_real_book_delayed() {
    // With zero latency and no strategy orders, the strategy's L2 view should match
    // the real book at every point it is told about.
    struct Check {
        mismatches: usize,
        checks: usize,
        book: lob::OrderBook,
        feed: std::vec::IntoIter<FeedEvent>,
        pending: Option<FeedEvent>,
    }
    impl Strategy for Check {
        fn on_book_update(&mut self, ctx: &mut Ctx) {
            // Advance our private copy of the book through the feed up to now.
            let mut ev = Vec::new();
            loop {
                let next = self.pending.take().or_else(|| self.feed.next());
                match next {
                    Some(e) if e.ts <= ctx.now() => self.book.process(&e.cmd, &mut ev),
                    other => {
                        self.pending = other;
                        break;
                    }
                }
            }
            // Several feed messages share t = 0 while the book is seeded; the view
            // only matches once all of them have been delivered.
            if ctx.now() == 0 {
                return;
            }
            self.checks += 1;
            if self.book.l2_snapshot(20) != ctx.book().l2_snapshot(20) {
                self.mismatches += 1;
            }
        }
        fn on_fill(&mut self, _: &mut Ctx, _: &Fill) {}
    }
    let mut cfg = mm_config(8);
    cfg.market.duration_s = 20.0;
    let feed: Vec<_> = cfg.feed().unwrap().collect();
    let mut c = Check {
        mismatches: 0,
        checks: 0,
        book: lob::OrderBook::default(),
        feed: feed.clone().into_iter(),
        pending: None,
    };
    backtest::run(feed, &mut c, &sim(0, 0));
    assert!(c.checks > 1000);
    assert_eq!(c.mismatches, 0);
}
