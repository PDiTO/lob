//! A naive momentum taker.
//!
//! On each timer tick it records the mid. When the mid has moved at least
//! `threshold` ticks over the lookback window it wants to be `max_position` long
//! (or short) and crosses the spread with IOC orders to get there, paying up to
//! `max_slippage` ticks through the touch. It holds until the signal flips.
//!
//! There is no reason to expect this to make money after taker fees; it exists
//! to exercise the taking side of the simulator.

use std::collections::VecDeque;

use serde::{Deserialize, Serialize};

use crate::backtest::{Ctx, Fill, Strategy};
use crate::market_data::DepthView;
use crate::types::{Price, Qty, Side};

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct MomentumConfig {
    /// Lookback in timer ticks.
    pub lookback: usize,
    /// Mid move over the lookback, in ticks, that counts as a signal.
    pub threshold: f64,
    pub trade_qty: Qty,
    pub max_position: i64,
    /// How far through the touch an IOC may reach, in ticks.
    pub max_slippage: Price,
}

impl Default for MomentumConfig {
    fn default() -> Self {
        Self {
            lookback: 20,
            threshold: 3.0,
            trade_qty: 5,
            max_position: 20,
            max_slippage: 1,
        }
    }
}

#[derive(Clone, Debug)]
pub struct Momentum {
    cfg: MomentumConfig,
    mids: VecDeque<f64>,
    target: i64,
}

impl Momentum {
    pub fn new(cfg: MomentumConfig) -> Self {
        Self {
            mids: VecDeque::with_capacity(cfg.lookback + 1),
            cfg,
            target: 0,
        }
    }
}

impl Strategy for Momentum {
    fn on_book_update(&mut self, _ctx: &mut Ctx) {}

    fn on_fill(&mut self, _ctx: &mut Ctx, _fill: &Fill) {}

    fn on_timer(&mut self, ctx: &mut Ctx) {
        let tob = ctx.book().top_of_book();
        let Some((bid, ask)) = tob.two_sided() else {
            return;
        };
        let mid = (bid.price + ask.price) as f64 / 2.0;
        self.mids.push_back(mid);
        if self.mids.len() > self.cfg.lookback + 1 {
            self.mids.pop_front();
        }
        if self.mids.len() <= self.cfg.lookback {
            return;
        }
        let change = mid - self.mids[0];
        if change >= self.cfg.threshold {
            self.target = self.cfg.max_position;
        } else if change <= -self.cfg.threshold {
            self.target = -self.cfg.max_position;
        }

        // Only one order in flight at a time.
        if ctx.open_orders().next().is_some() {
            return;
        }
        let gap = self.target - ctx.position();
        if gap == 0 {
            return;
        }
        let qty = gap.unsigned_abs().min(self.cfg.trade_qty);
        if gap > 0 {
            ctx.ioc(Side::Bid, ask.price + self.cfg.max_slippage, qty);
        } else {
            ctx.ioc(Side::Ask, bid.price - self.cfg.max_slippage, qty);
        }
    }
}
