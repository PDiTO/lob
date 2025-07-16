//! Summary statistics for a backtest run.

use std::fmt;

use serde::{Deserialize, Serialize};

use super::accounting::Liquidity;
use crate::types::{Price, Qty, Side};

/// Net PnL and inventory sampled at a fixed interval.
#[derive(Clone, Copy, Debug, PartialEq, Serialize, Deserialize)]
pub struct Sample {
    pub ts: u64,
    /// Net PnL in ticks x lots (realized + unrealized - fees), marked at mid.
    pub pnl: f64,
    pub position: i64,
    pub mid: Option<f64>,
}

/// One strategy fill, as it happened at the exchange.
#[derive(Clone, Copy, Debug, PartialEq, Serialize, Deserialize)]
pub struct FillRecord {
    pub ts: u64,
    pub order_id: u64,
    pub side: Side,
    pub price: Price,
    pub qty: Qty,
    pub liquidity: Liquidity,
    /// Mid just before the command that caused the fill.
    pub mid_before: Option<f64>,
}

/// Average markout at one horizon, in ticks per lot. Positive is good for us.
#[derive(Clone, Copy, Debug, PartialEq, Serialize, Deserialize)]
pub struct Markout {
    pub horizon_ns: u64,
    /// Lots whose horizon fell inside the run.
    pub lots: Qty,
    /// `side * (mid(t + h) - fill price)`: what the fill was worth h later.
    pub vs_fill_price: f64,
    /// `side * (mid(t + h) - mid(t))`: how far the market moved our way after the
    /// fill. Negative means adverse selection.
    pub mid_move: f64,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Summary {
    pub duration_s: f64,
    pub feed_events: u64,
    /// Feed messages the exchange rejected, almost always cancels of orders the
    /// strategy had already traded with.
    pub feed_rejects: u64,
    pub orders_sent: u64,
    pub cancels_sent: u64,
    pub amends_sent: u64,
    pub rejects: u64,
    pub fills: u64,
    /// Lots accepted by the exchange across new orders and upsizing amends.
    pub submitted_qty: Qty,
    pub filled_qty: Qty,
    pub maker_qty: Qty,
    pub taker_qty: Qty,
    /// `filled_qty / submitted_qty`.
    pub fill_ratio: f64,
    pub final_position: i64,
    pub max_abs_position: i64,
    /// Trade cash flows net of fees. All money figures are in ticks x lots.
    pub cash: f64,
    /// Mid used to mark the final position.
    pub final_mark: Option<f64>,
    pub realized_pnl: f64,
    pub unrealized_pnl: f64,
    pub fees: f64,
    pub net_pnl: f64,
    /// `net_pnl / filled_qty`.
    pub net_pnl_per_lot: f64,
    pub sample_interval_s: f64,
    /// Mean over standard deviation of per-interval PnL changes. Not annualized.
    pub sharpe_per_interval: f64,
    /// Largest peak-to-trough drop in sampled net PnL, as a positive number.
    pub max_drawdown: f64,
    pub markouts: Vec<Markout>,
}

/// Mean over sample standard deviation of successive differences.
pub fn sharpe(series: &[f64]) -> f64 {
    if series.len() < 3 {
        return 0.0;
    }
    let diffs: Vec<f64> = series.windows(2).map(|w| w[1] - w[0]).collect();
    let n = diffs.len() as f64;
    let mean = diffs.iter().sum::<f64>() / n;
    let var = diffs.iter().map(|d| (d - mean).powi(2)).sum::<f64>() / (n - 1.0);
    if var <= 0.0 { 0.0 } else { mean / var.sqrt() }
}

/// Largest peak-to-trough decline, as a non-negative number.
pub fn max_drawdown(series: &[f64]) -> f64 {
    let mut peak = f64::NEG_INFINITY;
    let mut worst: f64 = 0.0;
    for &x in series {
        peak = peak.max(x);
        worst = worst.max(peak - x);
    }
    worst
}

impl fmt::Display for Summary {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        writeln!(f, "duration            {:>12.1} s", self.duration_s)?;
        writeln!(
            f,
            "feed events         {:>12} ({} rejected)",
            self.feed_events, self.feed_rejects
        )?;
        writeln!(
            f,
            "orders / cancels / amends  {} / {} / {} ({} rejected)",
            self.orders_sent, self.cancels_sent, self.amends_sent, self.rejects
        )?;
        writeln!(
            f,
            "filled              {:>12} lots in {} fills (maker {}, taker {})",
            self.filled_qty, self.fills, self.maker_qty, self.taker_qty
        )?;
        writeln!(
            f,
            "fill ratio          {:>12.4} ({} lots submitted)",
            self.fill_ratio, self.submitted_qty
        )?;
        writeln!(
            f,
            "position            {:>12} final, {} max abs",
            self.final_position, self.max_abs_position
        )?;
        writeln!(f, "realized pnl        {:>12.1}", self.realized_pnl)?;
        writeln!(f, "unrealized pnl      {:>12.1}", self.unrealized_pnl)?;
        writeln!(f, "fees                {:>12.1}", self.fees)?;
        writeln!(
            f,
            "net pnl             {:>12.1} ticks x lots ({:.3} per lot)",
            self.net_pnl, self.net_pnl_per_lot
        )?;
        writeln!(
            f,
            "sharpe              {:>12.4} per {}s interval",
            self.sharpe_per_interval, self.sample_interval_s
        )?;
        writeln!(f, "max drawdown        {:>12.1}", self.max_drawdown)?;
        for m in &self.markouts {
            writeln!(
                f,
                "markout {:>7.3}s     {:>+12.3} vs fill, {:+.3} mid move ({} lots)",
                m.horizon_ns as f64 / 1e9,
                m.vs_fill_price,
                m.mid_move,
                m.lots
            )?;
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn drawdown_is_peak_to_trough() {
        assert_eq!(max_drawdown(&[0.0, 5.0, 2.0, 7.0, 1.0, 3.0]), 6.0);
        assert_eq!(max_drawdown(&[1.0, 2.0, 3.0]), 0.0);
        assert_eq!(max_drawdown(&[]), 0.0);
    }

    #[test]
    fn sharpe_of_steady_gains_is_large_and_of_flat_is_zero() {
        assert_eq!(sharpe(&[0.0, 0.0, 0.0, 0.0]), 0.0);
        let s = sharpe(&[0.0, 1.0, 2.1, 3.0, 4.05]);
        assert!(s > 10.0, "{s}");
        let alt = sharpe(&[0.0, 1.0, 0.0, 1.0, 0.0]);
        assert!(alt.abs() < 0.5, "{alt}");
    }
}
