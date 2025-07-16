//! A deterministic, event-driven backtester built on the matching engine.
//!
//! The market is an order-by-order feed (historical or from
//! [`synthetic`](crate::synthetic)) replayed into a real [`OrderBook`](crate::OrderBook).
//! The strategy's orders go into the same book, so they queue behind resting
//! orders, get filled only when the flow reaches them, and take liquidity other
//! feed orders would have hit. Latency is modelled on both legs: order entry and
//! market data (which also carries the strategy's own acks and fills).
//!
//! Given the same feed, strategy and config, a run is fully deterministic.
//!
//! # What this does not model
//!
//! Simulated fills are an approximation. The feed does not react to the strategy:
//! a feed order that would have been cancelled or repriced because of our quote is
//! not. Feed cancels for orders we already traded with are simply rejected. There
//! are no hidden or iceberg orders, no auction phases, and latency is a constant
//! rather than a distribution.

pub mod accounting;
pub mod metrics;
mod sim;
mod strategy;

pub use accounting::{Accounting, Liquidity, MONEY_SCALE};
pub use metrics::{FillRecord, Markout, Sample, Summary};
pub use sim::{BacktestResult, SimConfig, run};
pub use strategy::{
    Ctx, Fill, Idle, OpenOrder, OrderUpdate, Pending, PublicTrade, STRATEGY_ID_BASE, Strategy,
};
