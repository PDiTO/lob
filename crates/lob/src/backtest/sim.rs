//! The event loop.
//!
//! Two clocks matter: exchange time, when a message is processed by the matching
//! engine, and strategy time, when the strategy hears about it. The simulator keeps
//! a single time-ordered queue of everything that is due to happen:
//!
//! * feed messages reach the exchange at their own timestamps;
//! * strategy orders reach the exchange `order_latency_ns` after being sent;
//! * book updates, public trades, acks and fills reach the strategy
//!   `market_data_latency_ns` after the exchange produced them;
//! * timer callbacks and PnL samples fire on fixed intervals.
//!
//! Ties are broken deterministically: feed messages first, then everything else in
//! the order it was scheduled. So a strategy order that arrives at the same
//! nanosecond as a feed order loses the race.

use std::cmp::{Ordering, Reverse};
use std::collections::{BinaryHeap, VecDeque};
use std::iter::Peekable;

use serde::{Deserialize, Serialize};

use super::accounting::{Accounting, Liquidity, fee_to_money, mark_to_money, money_to_ticks};
use super::metrics::{self, FillRecord, Markout, Sample, Summary};
use super::strategy::{Action, Ctx, Fill, OrderUpdate, PublicTrade, STRATEGY_ID_BASE, Strategy};
use crate::book::{BookConfig, OrderBook, Placement, StpMode};
use crate::feed::FeedEvent;
use crate::market_data::DepthView;
use crate::synthetic::STRATEGY_OWNER;
use crate::types::{Command, Event, NewOrder, Side};

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct SimConfig {
    /// Strategy to exchange, for new orders, cancels and amends.
    pub order_latency_ns: u64,
    /// Exchange to strategy, for public market data and private acks and fills.
    pub market_data_latency_ns: u64,
    /// Where the strategy's resting orders join the queue. `back` is what a real
    /// venue does; `front` is an optimistic bound for comparison.
    pub queue_position: Placement,
    pub stp: StpMode,
    /// Fee per lot in ticks when adding liquidity. Negative for a rebate.
    pub maker_fee: f64,
    /// Fee per lot in ticks when taking liquidity.
    pub taker_fee: f64,
    /// Period of `on_timer`. Zero disables the timer.
    pub timer_interval_ns: u64,
    /// Period of PnL samples, which feed Sharpe and drawdown.
    pub sample_interval_ns: u64,
    pub markout_horizons_ns: Vec<u64>,
}

impl Default for SimConfig {
    fn default() -> Self {
        Self {
            order_latency_ns: 50_000,
            market_data_latency_ns: 50_000,
            queue_position: Placement::Back,
            stp: StpMode::CancelResting,
            maker_fee: -0.1,
            taker_fee: 0.3,
            timer_interval_ns: 100_000_000,
            sample_interval_ns: 1_000_000_000,
            markout_horizons_ns: vec![100_000_000, 1_000_000_000, 10_000_000_000],
        }
    }
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct BacktestResult {
    pub summary: Summary,
    pub samples: Vec<Sample>,
    pub fills: Vec<FillRecord>,
}

/// Something a strategy is told about. Private messages and public data share a
/// delivery queue so they arrive in exchange order.
#[derive(Debug)]
enum Delivery {
    Fill(Fill),
    Update(OrderUpdate),
    Trade(PublicTrade),
    Book(Side, i64, u64, u32),
}

#[derive(Debug)]
enum Due {
    /// A strategy command reaching the exchange.
    Arrive(Command),
    /// A batch of messages reaching the strategy.
    Deliver(Vec<Delivery>),
    Timer,
    Sample,
}

struct Scheduled {
    ts: u64,
    seq: u64,
    due: Due,
}

impl PartialEq for Scheduled {
    fn eq(&self, other: &Self) -> bool {
        (self.ts, self.seq) == (other.ts, other.seq)
    }
}
impl Eq for Scheduled {}
impl PartialOrd for Scheduled {
    fn partial_cmp(&self, other: &Self) -> Option<Ordering> {
        Some(self.cmp(other))
    }
}
impl Ord for Scheduled {
    fn cmp(&self, other: &Self) -> Ordering {
        (self.ts, self.seq).cmp(&(other.ts, other.seq))
    }
}

struct PendingMarkout {
    due: u64,
    side: Side,
    price: i64,
    qty: u64,
    mid_before: f64,
}

#[derive(Default)]
struct Counters {
    feed_events: u64,
    feed_rejects: u64,
    orders_sent: u64,
    cancels_sent: u64,
    amends_sent: u64,
    rejects: u64,
    fills: u64,
    submitted_qty: u64,
    filled_qty: u64,
    maker_qty: u64,
    taker_qty: u64,
    max_abs_position: i64,
}

/// Runs a backtest: replays `feed` into a fresh book alongside `strategy`.
pub fn run<S, I>(feed: I, strategy: &mut S, cfg: &SimConfig) -> BacktestResult
where
    S: Strategy + ?Sized,
    I: IntoIterator<Item = FeedEvent>,
{
    Sim::new(cfg, strategy, feed.into_iter()).run()
}

struct Sim<'a, S: Strategy + ?Sized, I: Iterator<Item = FeedEvent>> {
    cfg: &'a SimConfig,
    strategy: &'a mut S,
    feed: Peekable<I>,
    book: OrderBook,
    ctx: Ctx,
    queue: BinaryHeap<Reverse<Scheduled>>,
    seq: u64,
    now: u64,
    events: Vec<Event>,
    acct: Accounting,
    maker_fee: i128,
    taker_fee: i128,
    last_mid: Option<f64>,
    counters: Counters,
    samples: Vec<Sample>,
    fills: Vec<FillRecord>,
    markouts: Vec<VecDeque<PendingMarkout>>,
    markout_sums: Vec<(u64, f64, f64)>,
}

impl<'a, S: Strategy + ?Sized, I: Iterator<Item = FeedEvent>> Sim<'a, S, I> {
    fn new(cfg: &'a SimConfig, strategy: &'a mut S, feed: I) -> Self {
        let horizons = cfg.markout_horizons_ns.len();
        Self {
            cfg,
            strategy,
            feed: feed.peekable(),
            book: OrderBook::new(BookConfig {
                stp: cfg.stp,
                emit_book_updates: true,
            }),
            ctx: Ctx::new(),
            queue: BinaryHeap::new(),
            seq: 0,
            now: 0,
            events: Vec::with_capacity(64),
            acct: Accounting::new(),
            maker_fee: fee_to_money(cfg.maker_fee),
            taker_fee: fee_to_money(cfg.taker_fee),
            last_mid: None,
            counters: Counters::default(),
            samples: Vec::new(),
            fills: Vec::new(),
            markouts: (0..horizons).map(|_| VecDeque::new()).collect(),
            markout_sums: vec![(0, 0.0, 0.0); horizons],
        }
    }

    fn schedule(&mut self, ts: u64, due: Due) {
        self.seq += 1;
        self.queue.push(Reverse(Scheduled {
            ts,
            seq: self.seq,
            due,
        }));
    }

    fn run(mut self) -> BacktestResult {
        if self.cfg.timer_interval_ns > 0 {
            self.schedule(self.cfg.timer_interval_ns, Due::Timer);
        }
        if self.cfg.sample_interval_ns > 0 {
            self.schedule(0, Due::Sample);
        }
        self.ctx.now = 0;
        self.strategy.on_start(&mut self.ctx);
        self.flush_actions();

        let mut last_feed_ts = 0;
        loop {
            let feed_ts = self.feed.peek().map(|e| e.ts);
            let queue_ts = self.queue.peek().map(|s| s.0.ts);
            let take_feed = match (feed_ts, queue_ts) {
                (None, None) => break,
                (Some(_), None) => true,
                (None, Some(q)) => {
                    // Feed exhausted: finish anything due up to its last message.
                    if q > last_feed_ts {
                        break;
                    }
                    false
                }
                (Some(f), Some(q)) => f <= q,
            };
            if take_feed {
                let e = self.feed.next().expect("peeked");
                debug_assert!(e.ts >= self.now, "feed went back in time");
                self.advance(e.ts);
                last_feed_ts = e.ts;
                self.counters.feed_events += 1;
                self.exchange(e.cmd, false);
            } else {
                let Reverse(s) = self.queue.pop().expect("peeked");
                self.advance(s.ts);
                match s.due {
                    Due::Arrive(cmd) => self.exchange(cmd, true),
                    Due::Deliver(batch) => self.deliver(batch),
                    Due::Timer => {
                        self.ctx.now = self.now;
                        self.strategy.on_timer(&mut self.ctx);
                        self.flush_actions();
                        self.schedule(self.now + self.cfg.timer_interval_ns, Due::Timer);
                    }
                    Due::Sample => {
                        self.sample();
                        self.schedule(self.now + self.cfg.sample_interval_ns, Due::Sample);
                    }
                }
            }
            if self.ctx.stopped {
                break;
            }
        }
        self.finish()
    }

    /// Moves the clock forward, settling any markouts that came due before `ts`.
    fn advance(&mut self, ts: u64) {
        self.now = self.now.max(ts);
        let Some(mid) = self.last_mid else { return };
        for (h, pending) in self.markouts.iter_mut().enumerate() {
            while pending.front().is_some_and(|m| m.due <= ts) {
                let m = pending.pop_front().expect("checked");
                let dir = m.side.sign() as f64;
                let q = m.qty as f64;
                let sums = &mut self.markout_sums[h];
                sums.0 += m.qty;
                sums.1 += dir * (mid - m.price as f64) * q;
                sums.2 += dir * (mid - m.mid_before) * q;
            }
        }
    }

    fn mark(&self) -> i128 {
        self.last_mid.map_or(0, mark_to_money)
    }

    fn sample(&mut self) {
        let pnl = if self.acct.position() == 0 || self.last_mid.is_some() {
            money_to_ticks(self.acct.net(self.mark()))
        } else {
            f64::NAN
        };
        self.samples.push(Sample {
            ts: self.now,
            pnl,
            position: self.acct.position(),
            mid: self.last_mid,
        });
    }

    /// Runs one command through the matching engine and routes what comes out.
    fn exchange(&mut self, cmd: Command, ours: bool) {
        let mid_before = self.last_mid;
        let placement = if ours {
            self.cfg.queue_position
        } else {
            Placement::Back
        };
        self.events.clear();
        self.book
            .process_with_placement(&cmd, placement, &mut self.events);

        // Running open quantity of our aggressing order, for fill `leaves`.
        let mut taker_leaves = match cmd {
            Command::New(o) if ours => o.qty,
            Command::Amend { qty, .. } if ours => qty,
            _ => 0,
        };
        let mut out: Vec<Delivery> = Vec::new();
        for i in 0..self.events.len() {
            match self.events[i] {
                Event::Trade {
                    maker_id,
                    maker_owner,
                    taker_id,
                    taker_owner,
                    taker_side,
                    price,
                    qty,
                    maker_remaining,
                } => {
                    if maker_owner == STRATEGY_OWNER {
                        let fill = Fill {
                            order_id: maker_id,
                            side: taker_side.opposite(),
                            price,
                            qty,
                            leaves: maker_remaining,
                            liquidity: Liquidity::Maker,
                            exchange_ts: self.now,
                        };
                        self.book_fill(&fill, mid_before);
                        out.push(Delivery::Fill(fill));
                    }
                    if taker_owner == STRATEGY_OWNER {
                        taker_leaves -= qty;
                        let fill = Fill {
                            order_id: taker_id,
                            side: taker_side,
                            price,
                            qty,
                            leaves: taker_leaves,
                            liquidity: Liquidity::Taker,
                            exchange_ts: self.now,
                        };
                        self.book_fill(&fill, mid_before);
                        out.push(Delivery::Fill(fill));
                    }
                    out.push(Delivery::Trade(PublicTrade {
                        exchange_ts: self.now,
                        price,
                        qty,
                        aggressor: taker_side,
                    }));
                }
                Event::BookUpdate {
                    side,
                    price,
                    qty,
                    orders,
                } => out.push(Delivery::Book(side, price, qty, orders)),
                Event::Accepted { id, qty, .. } if ours => {
                    self.counters.submitted_qty += qty;
                    out.push(Delivery::Update(OrderUpdate::Accepted { id }));
                }
                Event::Rejected {
                    id,
                    request,
                    reason,
                } => {
                    if ours {
                        self.counters.rejects += 1;
                        out.push(Delivery::Update(OrderUpdate::Rejected {
                            id,
                            request,
                            reason,
                        }));
                    } else {
                        self.counters.feed_rejects += 1;
                    }
                }
                Event::Cancelled {
                    id,
                    owner,
                    qty,
                    reason,
                    ..
                } if owner == STRATEGY_OWNER => {
                    out.push(Delivery::Update(OrderUpdate::Cancelled { id, qty, reason }));
                }
                Event::Amended {
                    id,
                    old_qty,
                    new_price,
                    new_qty,
                    kept_priority,
                    ..
                } if ours => {
                    self.counters.submitted_qty += new_qty.saturating_sub(old_qty);
                    out.push(Delivery::Update(OrderUpdate::Amended {
                        id,
                        price: new_price,
                        qty: new_qty,
                        kept_priority,
                    }));
                }
                _ => {}
            }
        }
        if let Some(mid) = self.book.mid() {
            self.last_mid = Some(mid);
        }
        if !out.is_empty() {
            let at = self.now + self.cfg.market_data_latency_ns;
            self.schedule(at, Due::Deliver(out));
        }
    }

    /// Books a strategy fill at exchange time: accounting, stats, markouts.
    fn book_fill(&mut self, fill: &Fill, mid_before: Option<f64>) {
        let fee = match fill.liquidity {
            Liquidity::Maker => self.maker_fee,
            Liquidity::Taker => self.taker_fee,
        };
        self.acct.fill(fill.side, fill.price, fill.qty, fee);
        let c = &mut self.counters;
        c.fills += 1;
        c.filled_qty += fill.qty;
        match fill.liquidity {
            Liquidity::Maker => c.maker_qty += fill.qty,
            Liquidity::Taker => c.taker_qty += fill.qty,
        }
        c.max_abs_position = c.max_abs_position.max(self.acct.position().abs());
        self.fills.push(FillRecord {
            ts: self.now,
            order_id: fill.order_id,
            side: fill.side,
            price: fill.price,
            qty: fill.qty,
            liquidity: fill.liquidity,
            mid_before,
        });
        if let Some(mid_before) = mid_before {
            for (h, &horizon) in self.cfg.markout_horizons_ns.iter().enumerate() {
                self.markouts[h].push_back(PendingMarkout {
                    due: self.now + horizon,
                    side: fill.side,
                    price: fill.price,
                    qty: fill.qty,
                    mid_before,
                });
            }
        }
    }

    /// Hands a batch of messages to the strategy.
    fn deliver(&mut self, batch: Vec<Delivery>) {
        self.ctx.now = self.now;
        let mut book_changed = false;
        for d in batch {
            match d {
                Delivery::Fill(fill) => {
                    self.ctx.apply_fill(&fill);
                    self.strategy.on_fill(&mut self.ctx, &fill);
                }
                Delivery::Update(u) => {
                    self.ctx.apply_update(&u);
                    self.strategy.on_order_update(&mut self.ctx, &u);
                }
                Delivery::Trade(t) => self.strategy.on_trade(&mut self.ctx, &t),
                Delivery::Book(side, price, qty, orders) => {
                    self.ctx.book.update(side, price, qty, orders);
                    book_changed = true;
                }
            }
        }
        if book_changed {
            self.strategy.on_book_update(&mut self.ctx);
        }
        self.flush_actions();
    }

    /// Sends whatever the strategy queued during a callback.
    fn flush_actions(&mut self) {
        if self.ctx.actions.is_empty() {
            return;
        }
        let at = self.now + self.cfg.order_latency_ns;
        let actions = std::mem::take(&mut self.ctx.actions);
        for a in &actions {
            let cmd = match *a {
                Action::New {
                    id,
                    side,
                    order_type,
                    price,
                    qty,
                } => {
                    debug_assert!(id >= STRATEGY_ID_BASE);
                    self.counters.orders_sent += 1;
                    Command::New(NewOrder::new(
                        id,
                        STRATEGY_OWNER,
                        side,
                        order_type,
                        price,
                        qty,
                    ))
                }
                Action::Cancel { id } => {
                    self.counters.cancels_sent += 1;
                    Command::Cancel { id }
                }
                Action::Amend { id, price, qty } => {
                    self.counters.amends_sent += 1;
                    Command::Amend { id, price, qty }
                }
            };
            self.schedule(at, Due::Arrive(cmd));
        }
        self.ctx.actions = actions;
        self.ctx.actions.clear();
    }

    fn finish(mut self) -> BacktestResult {
        self.sample();
        let mark = self.mark();
        let c = &self.counters;
        let series: Vec<f64> = self
            .samples
            .iter()
            .map(|s| s.pnl)
            .filter(|p| p.is_finite())
            .collect();
        let net = money_to_ticks(self.acct.net(mark));
        let markouts = self
            .cfg
            .markout_horizons_ns
            .iter()
            .zip(&self.markout_sums)
            .map(|(&h, &(lots, vs_fill, mid_move))| {
                let per = |x: f64| if lots > 0 { x / lots as f64 } else { 0.0 };
                Markout {
                    horizon_ns: h,
                    lots,
                    vs_fill_price: per(vs_fill),
                    mid_move: per(mid_move),
                }
            })
            .collect();
        let summary = Summary {
            duration_s: self.now as f64 / 1e9,
            feed_events: c.feed_events,
            feed_rejects: c.feed_rejects,
            orders_sent: c.orders_sent,
            cancels_sent: c.cancels_sent,
            amends_sent: c.amends_sent,
            rejects: c.rejects,
            fills: c.fills,
            submitted_qty: c.submitted_qty,
            filled_qty: c.filled_qty,
            maker_qty: c.maker_qty,
            taker_qty: c.taker_qty,
            fill_ratio: if c.submitted_qty > 0 {
                c.filled_qty as f64 / c.submitted_qty as f64
            } else {
                0.0
            },
            final_position: self.acct.position(),
            max_abs_position: c.max_abs_position,
            cash: money_to_ticks(self.acct.cash()),
            final_mark: self.last_mid,
            realized_pnl: money_to_ticks(self.acct.realized()),
            unrealized_pnl: money_to_ticks(self.acct.unrealized(mark)),
            fees: money_to_ticks(self.acct.fees()),
            net_pnl: net,
            net_pnl_per_lot: if c.filled_qty > 0 {
                net / c.filled_qty as f64
            } else {
                0.0
            },
            sample_interval_s: self.cfg.sample_interval_ns as f64 / 1e9,
            sharpe_per_interval: metrics::sharpe(&series),
            max_drawdown: metrics::max_drawdown(&series),
            markouts,
        };
        BacktestResult {
            summary,
            samples: self.samples,
            fills: self.fills,
        }
    }
}
