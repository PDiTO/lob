//! A symmetric market maker with inventory skew.
//!
//! Quotes one post-only order per side around a reference price (mid by default,
//! optionally microprice). The reference is shifted against inventory, so a long position lowers
//! both quotes: the bid gets less aggressive and the ask more, which tends to work
//! the position back toward flat. Size on the side that would grow the position is
//! cut once inventory reaches the limit.
//!
//! The reference price is computed from the book with the strategy's own quotes
//! taken out. Without that, joining the touch changes the microprice, which moves
//! the target, which moves the quote, and the strategy ends up chasing itself.
//!
//! Quotes are only moved when the target price is at least `requote_threshold`
//! ticks away from the live one, and never while an earlier request for that side
//! is still in flight. Moving a quote is an amend, which costs queue priority.

use serde::{Deserialize, Serialize};

use crate::backtest::{Ctx, Fill, Pending, Strategy};
use crate::book::LevelInfo;
use crate::market_data::{DepthView, TopOfBook};
use crate::types::{OrderId, OrderType, Price, Qty, Side};

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ReferencePrice {
    #[default]
    Mid,
    Microprice,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct MarketMakerConfig {
    /// Distance of each quote from the (skewed) reference price, in ticks.
    pub half_spread: f64,
    pub quote_qty: Qty,
    /// Absolute inventory limit in lots.
    pub max_position: i64,
    /// Reference price shift per lot of inventory, in ticks.
    pub skew_per_lot: f64,
    /// Minimum move, in ticks, before a quote is repriced.
    pub requote_threshold: Price,
    pub reference: ReferencePrice,
}

impl Default for MarketMakerConfig {
    fn default() -> Self {
        Self {
            half_spread: 0.5,
            quote_qty: 5,
            max_position: 50,
            skew_per_lot: 0.05,
            requote_threshold: 1,
            reference: ReferencePrice::Mid,
        }
    }
}

#[derive(Clone, Debug)]
pub struct MarketMaker {
    cfg: MarketMakerConfig,
}

impl MarketMaker {
    pub fn new(cfg: MarketMakerConfig) -> Self {
        Self { cfg }
    }

    /// Best level on `side` after removing the strategy's own resting size.
    fn best_ex_own(ctx: &Ctx, side: Side) -> Option<LevelInfo> {
        ctx.book().depth(side).find_map(|l| {
            let own: Qty = ctx
                .open_orders()
                .filter(|o| o.side == side && o.price == l.price && o.pending != Pending::New)
                .map(|o| o.qty)
                .sum();
            let qty = l.qty.saturating_sub(own);
            (qty > 0).then_some(LevelInfo { qty, ..l })
        })
    }

    /// Target (price, qty) for each side given the current view and position.
    pub fn targets(&self, ctx: &Ctx) -> Option<[(Side, Price, Qty); 2]> {
        let book = ctx.book();
        let (best_bid, best_ask) = book.top_of_book().two_sided()?;
        let others = TopOfBook {
            bid: Self::best_ex_own(ctx, Side::Bid),
            ask: Self::best_ex_own(ctx, Side::Ask),
        };
        let reference = match self.cfg.reference {
            ReferencePrice::Mid => others.mid()?,
            ReferencePrice::Microprice => others.microprice()?,
        };
        let pos = ctx.position();
        let skewed = reference - self.cfg.skew_per_lot * pos as f64;
        // Never quote through the other side: these are post-only.
        let bid = ((skewed - self.cfg.half_spread).floor() as Price).min(best_ask.price - 1);
        let ask = ((skewed + self.cfg.half_spread).ceil() as Price).max(best_bid.price + 1);
        let q = self.cfg.quote_qty as i64;
        let max = self.cfg.max_position;
        let bid_qty = q.min(max - pos).max(0) as Qty;
        let ask_qty = q.min(max + pos).max(0) as Qty;
        Some([(Side::Bid, bid, bid_qty), (Side::Ask, ask, ask_qty)])
    }

    fn requote(&mut self, ctx: &mut Ctx) {
        let Some(targets) = self.targets(ctx) else {
            return;
        };
        for (side, price, qty) in targets {
            let working: Vec<(OrderId, Price, Qty, Pending)> = ctx
                .open_orders()
                .filter(|o| o.side == side && o.order_type == OrderType::PostOnly)
                .map(|o| (o.id, o.price, o.qty, o.pending))
                .collect();
            if working.iter().any(|w| w.3 != Pending::None) {
                continue; // wait for the last request on this side to land
            }
            match working.as_slice() {
                [] if qty > 0 => {
                    ctx.post_only(side, price, qty);
                }
                [] => {}
                [(id, live_price, live_qty, _)] => {
                    if qty == 0 {
                        ctx.cancel(*id);
                    } else if (live_price - price).abs() >= self.cfg.requote_threshold
                        || qty < *live_qty
                    {
                        ctx.amend(*id, price, qty);
                    }
                }
                many => {
                    // Should not happen, but tidy up if it does.
                    for (id, ..) in &many[1..] {
                        ctx.cancel(*id);
                    }
                }
            }
        }
    }
}

impl Strategy for MarketMaker {
    fn on_book_update(&mut self, ctx: &mut Ctx) {
        self.requote(ctx);
    }

    fn on_fill(&mut self, ctx: &mut Ctx, _fill: &Fill) {
        self.requote(ctx);
    }
}
