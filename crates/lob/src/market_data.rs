//! Market data views: L2 snapshots and incremental updates, the L3 order-by-order
//! view, top of book and a few standard fair-price estimates.
//!
//! Floats appear here for the first time, and only in derived analytics (mid,
//! microprice). Book state itself stays in integer ticks and lots.

use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};

use crate::book::{LevelInfo, OrderBook, RestingOrder};
use crate::types::{Event, Price, Qty, Side};

/// Aggregated depth, best level first on each side.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct L2Snapshot {
    pub bids: Vec<LevelInfo>,
    pub asks: Vec<LevelInfo>,
}

/// Every resting order, in priority order (best price first, then FIFO).
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct L3Snapshot {
    pub bids: Vec<RestingOrder>,
    pub asks: Vec<RestingOrder>,
}

impl L3Snapshot {
    /// Aggregates the orders into levels. Must equal the book's own L2 view.
    pub fn to_l2(&self) -> L2Snapshot {
        fn agg(orders: &[RestingOrder]) -> Vec<LevelInfo> {
            let mut out: Vec<LevelInfo> = Vec::new();
            for o in orders {
                match out.last_mut() {
                    Some(l) if l.price == o.price => {
                        l.qty += o.qty;
                        l.orders += 1;
                    }
                    _ => out.push(LevelInfo {
                        price: o.price,
                        qty: o.qty,
                        orders: 1,
                    }),
                }
            }
            out
        }
        L2Snapshot {
            bids: agg(&self.bids),
            asks: agg(&self.asks),
        }
    }
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct TopOfBook {
    pub bid: Option<LevelInfo>,
    pub ask: Option<LevelInfo>,
}

impl TopOfBook {
    /// Both sides present.
    pub fn two_sided(&self) -> Option<(LevelInfo, LevelInfo)> {
        Some((self.bid?, self.ask?))
    }

    pub fn spread(&self) -> Option<Price> {
        self.two_sided().map(|(b, a)| a.price - b.price)
    }

    pub fn mid(&self) -> Option<f64> {
        self.two_sided()
            .map(|(b, a)| (b.price as f64 + a.price as f64) / 2.0)
    }

    /// Size-weighted mid: leans toward the side with less size, since that side is
    /// the one more likely to be taken out next.
    ///
    /// `(bid * ask_qty + ask * bid_qty) / (bid_qty + ask_qty)`
    pub fn microprice(&self) -> Option<f64> {
        self.two_sided().map(|(b, a)| {
            let (bq, aq) = (b.qty as f64, a.qty as f64);
            (b.price as f64 * aq + a.price as f64 * bq) / (bq + aq)
        })
    }

    /// `(bid_qty - ask_qty) / (bid_qty + ask_qty)`, in [-1, 1].
    pub fn imbalance(&self) -> Option<f64> {
        self.two_sided().map(|(b, a)| {
            let (bq, aq) = (b.qty as f64, a.qty as f64);
            (bq - aq) / (bq + aq)
        })
    }
}

/// Read access to aggregated depth, implemented by the matching book itself and by
/// [`L2Book`], the consumer-side book rebuilt from updates.
pub trait DepthView {
    /// Levels on one side, best first.
    fn depth(&self, side: Side) -> impl Iterator<Item = LevelInfo> + '_;

    fn top_of_book(&self) -> TopOfBook {
        TopOfBook {
            bid: self.depth(Side::Bid).next(),
            ask: self.depth(Side::Ask).next(),
        }
    }

    fn mid(&self) -> Option<f64> {
        self.top_of_book().mid()
    }

    fn microprice(&self) -> Option<f64> {
        self.top_of_book().microprice()
    }

    /// Average of the volume-weighted prices of the top `levels` levels on each side.
    fn depth_weighted_mid(&self, levels: usize) -> Option<f64> {
        let vwap = |side| {
            let (mut pq, mut q) = (0.0, 0.0);
            for l in self.depth(side).take(levels) {
                pq += l.price as f64 * l.qty as f64;
                q += l.qty as f64;
            }
            (q > 0.0).then(|| pq / q)
        };
        Some((vwap(Side::Bid)? + vwap(Side::Ask)?) / 2.0)
    }

    /// Top `depth` levels per side.
    fn l2_snapshot(&self, depth: usize) -> L2Snapshot {
        L2Snapshot {
            bids: self.depth(Side::Bid).take(depth).collect(),
            asks: self.depth(Side::Ask).take(depth).collect(),
        }
    }

    /// Whether the best bid is at or above the best ask.
    fn is_crossed(&self) -> bool {
        self.top_of_book()
            .two_sided()
            .is_some_and(|(b, a)| b.price >= a.price)
    }
}

impl DepthView for OrderBook {
    fn depth(&self, side: Side) -> impl Iterator<Item = LevelInfo> + '_ {
        self.levels(side)
    }
}

impl OrderBook {
    pub fn l3_snapshot(&self) -> L3Snapshot {
        L3Snapshot {
            bids: self.orders(Side::Bid).collect(),
            asks: self.orders(Side::Ask).collect(),
        }
    }
}

/// An L2 book maintained from [`Event::BookUpdate`]s, the way a market data
/// consumer would build one. The backtester gives strategies one of these,
/// updated with market-data latency, rather than a reference to the real book.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct L2Book {
    bids: BTreeMap<Price, (Qty, u32)>,
    asks: BTreeMap<Price, (Qty, u32)>,
}

impl L2Book {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn from_snapshot(snap: &L2Snapshot) -> Self {
        let mut book = Self::new();
        for l in &snap.bids {
            book.bids.insert(l.price, (l.qty, l.orders));
        }
        for l in &snap.asks {
            book.asks.insert(l.price, (l.qty, l.orders));
        }
        book
    }

    /// Applies one incremental update.
    pub fn update(&mut self, side: Side, price: Price, qty: Qty, orders: u32) {
        let map = match side {
            Side::Bid => &mut self.bids,
            Side::Ask => &mut self.asks,
        };
        if qty == 0 {
            map.remove(&price);
        } else {
            map.insert(price, (qty, orders));
        }
    }

    /// Applies an event if it is a `BookUpdate`; ignores anything else. Returns
    /// whether the book changed.
    pub fn apply(&mut self, event: &Event) -> bool {
        if let Event::BookUpdate {
            side,
            price,
            qty,
            orders,
        } = *event
        {
            self.update(side, price, qty, orders);
            true
        } else {
            false
        }
    }

    pub fn level_count(&self, side: Side) -> usize {
        match side {
            Side::Bid => self.bids.len(),
            Side::Ask => self.asks.len(),
        }
    }

    pub fn qty_at(&self, side: Side, price: Price) -> Qty {
        let map = match side {
            Side::Bid => &self.bids,
            Side::Ask => &self.asks,
        };
        map.get(&price).map_or(0, |&(q, _)| q)
    }
}

impl DepthView for L2Book {
    fn depth(&self, side: Side) -> impl Iterator<Item = LevelInfo> + '_ {
        let info =
            |(&price, &(qty, orders)): (&Price, &(Qty, u32))| LevelInfo { price, qty, orders };
        // Boxing keeps the two branches one type; this is not a hot path.
        let it: Box<dyn Iterator<Item = LevelInfo> + '_> = match side {
            Side::Bid => Box::new(self.bids.iter().rev().map(info)),
            Side::Ask => Box::new(self.asks.iter().map(info)),
        };
        it
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::types::NewOrder;

    fn lvl(price: Price, qty: Qty) -> LevelInfo {
        LevelInfo {
            price,
            qty,
            orders: 1,
        }
    }

    #[test]
    fn microprice_leans_toward_thin_side() {
        let tob = TopOfBook {
            bid: Some(lvl(100, 9)),
            ask: Some(lvl(102, 1)),
        };
        assert_eq!(tob.mid(), Some(101.0));
        // Lots of bids, one lot offered: the ask is about to go, fair is near 102.
        assert!((tob.microprice().unwrap() - 101.8).abs() < 1e-12);
        assert!((tob.imbalance().unwrap() - 0.8).abs() < 1e-12);
        assert_eq!(tob.spread(), Some(2));
    }

    #[test]
    fn one_sided_book_has_no_mid() {
        let tob = TopOfBook {
            bid: Some(lvl(100, 1)),
            ask: None,
        };
        assert_eq!(tob.mid(), None);
        assert_eq!(tob.microprice(), None);
    }

    #[test]
    fn depth_weighted_mid_uses_top_n_levels() {
        let mut book = OrderBook::default();
        let mut ev = Vec::new();
        book.submit(NewOrder::limit(1, 1, Side::Bid, 100, 1), &mut ev);
        book.submit(NewOrder::limit(2, 1, Side::Bid, 99, 3), &mut ev);
        book.submit(NewOrder::limit(3, 2, Side::Ask, 101, 1), &mut ev);
        book.submit(NewOrder::limit(4, 2, Side::Ask, 104, 1), &mut ev);
        // bid vwap = (100 + 297) / 4 = 99.25, ask vwap = 102.5
        let dwm = book.depth_weighted_mid(2).unwrap();
        assert!((dwm - 100.875).abs() < 1e-12);
        assert_eq!(book.depth_weighted_mid(1), book.mid());
    }

    #[test]
    fn incremental_updates_rebuild_the_snapshot() {
        let mut book = OrderBook::default();
        let mut l2 = L2Book::new();
        let mut ev = Vec::new();
        book.submit(NewOrder::limit(1, 1, Side::Bid, 100, 5), &mut ev);
        book.submit(NewOrder::limit(2, 1, Side::Ask, 103, 5), &mut ev);
        book.submit(NewOrder::limit(3, 2, Side::Ask, 102, 2), &mut ev);
        book.submit(NewOrder::market(4, 3, Side::Bid, 4), &mut ev);
        book.cancel(1, &mut ev);
        for e in &ev {
            l2.apply(e);
        }
        assert_eq!(l2.l2_snapshot(10), book.l2_snapshot(10));
        assert_eq!(book.l3_snapshot().to_l2(), book.l2_snapshot(usize::MAX));
        assert_eq!(L2Book::from_snapshot(&book.l2_snapshot(10)), l2);
    }
}
