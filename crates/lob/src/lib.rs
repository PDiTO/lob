//! A price-time priority limit order book, market data views built on it, and an
//! event-driven backtester.
//!
//! ```
//! use lob::{Event, NewOrder, OrderBook, Side};
//!
//! let mut book = OrderBook::default();
//! let mut events = Vec::new();
//! book.submit(NewOrder::limit(1, 7, Side::Ask, 10_001, 5), &mut events);
//! book.submit(NewOrder::limit(2, 8, Side::Bid, 10_001, 3), &mut events);
//!
//! let fills: Vec<_> = events
//!     .iter()
//!     .filter(|e| matches!(e, Event::Trade { .. }))
//!     .collect();
//! assert_eq!(fills.len(), 1);
//! assert_eq!(book.best_ask().unwrap().qty, 2);
//! ```

pub mod book;
pub mod feed;
pub mod market_data;
pub mod reference;
pub mod rng;
pub mod synthetic;
pub mod types;

pub use book::{BookConfig, LevelInfo, OrderBook, Placement, RestingOrder, StpMode};
pub use market_data::{DepthView, L2Book, L2Snapshot, L3Snapshot, TopOfBook};
pub use types::{
    CancelReason, Command, Event, NewOrder, OrderId, OrderType, OwnerId, Price, Qty, RejectReason,
    RequestKind, Side,
};
