//! Position, cash and PnL bookkeeping in exact integer arithmetic.
//!
//! Money is held in micro-ticks (one tick = 1,000,000 units of money per lot) in
//! `i128`, so prices, fees in fractions of a tick and half-tick mids are all exact.
//! Realized PnL uses average cost: closing part of a position realizes the
//! difference between the fill price and that part's share of the cost basis.
//!
//! The books balance by construction:
//!
//! ```text
//! cash + position * mark == realized + unrealized - fees
//! ```
//!
//! where `cash` already has fees taken out and `unrealized = position * mark - basis`.

use serde::{Deserialize, Serialize};

use crate::types::{Price, Qty, Side};

/// Money units per tick.
pub const MONEY_SCALE: i128 = 1_000_000;

/// Converts a fee in ticks per lot to money units per lot, rounded to the nearest unit.
pub fn fee_to_money(ticks_per_lot: f64) -> i128 {
    (ticks_per_lot * MONEY_SCALE as f64).round() as i128
}

/// Converts a mark price in ticks (possibly a half tick) to money units per lot.
pub fn mark_to_money(mark: f64) -> i128 {
    (mark * MONEY_SCALE as f64).round() as i128
}

pub fn money_to_ticks(m: i128) -> f64 {
    m as f64 / MONEY_SCALE as f64
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Liquidity {
    Maker,
    Taker,
}

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Accounting {
    position: i64,
    /// Trade cash flows minus fees.
    cash: i128,
    /// Signed cost of the open position (negative when short).
    basis: i128,
    realized: i128,
    fees: i128,
}

impl Accounting {
    pub fn new() -> Self {
        Self::default()
    }

    /// Books one fill. `fee` is in money units per lot; negative for a rebate.
    pub fn fill(&mut self, side: Side, price: Price, qty: Qty, fee: i128) {
        let px = price as i128 * MONEY_SCALE;
        let dir = side.sign() as i128;
        let qty_i = qty as i128;

        self.cash -= dir * qty_i * px;
        let fee_total = fee * qty_i;
        self.fees += fee_total;
        self.cash -= fee_total;

        let pos = self.position as i128;
        let mut opening = qty_i;
        if pos != 0 && pos.signum() != dir {
            let closing = qty_i.min(pos.abs());
            let basis_removed = self.basis * closing / pos.abs();
            self.realized += -dir * closing * px - basis_removed;
            self.basis -= basis_removed;
            self.position += (dir * closing) as i64;
            opening -= closing;
        }
        if opening > 0 {
            self.basis += dir * opening * px;
            self.position += (dir * opening) as i64;
        }
    }

    pub fn position(&self) -> i64 {
        self.position
    }

    pub fn cash(&self) -> i128 {
        self.cash
    }

    pub fn realized(&self) -> i128 {
        self.realized
    }

    pub fn fees(&self) -> i128 {
        self.fees
    }

    pub fn basis(&self) -> i128 {
        self.basis
    }

    /// Unrealized PnL at `mark` (money units per lot).
    pub fn unrealized(&self, mark: i128) -> i128 {
        self.position as i128 * mark - self.basis
    }

    /// Net PnL: realized + unrealized - fees, which equals cash + position * mark.
    pub fn net(&self, mark: i128) -> i128 {
        self.cash + self.position as i128 * mark
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const S: i128 = MONEY_SCALE;

    #[test]
    fn round_trip_realizes_the_price_difference() {
        let mut a = Accounting::new();
        a.fill(Side::Bid, 100, 3, 0);
        a.fill(Side::Ask, 104, 3, 0);
        assert_eq!(a.position(), 0);
        assert_eq!(a.realized(), 12 * S);
        assert_eq!(a.basis(), 0);
        assert_eq!(a.cash(), 12 * S);
    }

    #[test]
    fn short_then_cover() {
        let mut a = Accounting::new();
        a.fill(Side::Ask, 100, 2, 0);
        a.fill(Side::Ask, 102, 2, 0);
        assert_eq!(a.position(), -4);
        // Average short price 101; cover 1 at 99 realizes 2.
        a.fill(Side::Bid, 99, 1, 0);
        assert_eq!(a.realized(), 2 * S);
        assert_eq!(a.unrealized(99 * S), 6 * S);
    }

    #[test]
    fn crossing_through_flat_splits_the_fill() {
        let mut a = Accounting::new();
        a.fill(Side::Bid, 100, 2, 0);
        a.fill(Side::Ask, 105, 5, 0); // close 2 (realize 10), open short 3 at 105
        assert_eq!(a.position(), -3);
        assert_eq!(a.realized(), 10 * S);
        assert_eq!(a.basis(), -315 * S);
        assert_eq!(a.unrealized(103 * S), 6 * S);
    }

    #[test]
    fn fees_and_rebates_hit_cash_not_realized() {
        let mut a = Accounting::new();
        a.fill(Side::Bid, 100, 10, fee_to_money(0.25)); // taker
        a.fill(Side::Ask, 100, 10, fee_to_money(-0.1)); // maker rebate
        assert_eq!(a.realized(), 0);
        assert_eq!(a.fees(), fee_to_money(1.5));
        assert_eq!(a.net(100 * S), -fee_to_money(1.5));
    }

    #[test]
    fn identity_holds_with_rounding_in_the_basis() {
        let mut a = Accounting::new();
        a.fill(Side::Bid, 100, 1, 0);
        a.fill(Side::Bid, 101, 2, 0);
        a.fill(Side::Ask, 107, 1, 7); // basis 302 / 3 lots does not divide evenly
        let mark = mark_to_money(103.5);
        assert_eq!(
            a.cash() + a.position() as i128 * mark,
            a.realized() + a.unrealized(mark) - a.fees()
        );
    }
}
