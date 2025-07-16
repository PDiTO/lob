//! The interface between a strategy and the simulator.

use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};

use super::accounting::Liquidity;
use crate::market_data::L2Book;
use crate::types::{CancelReason, OrderId, OrderType, Price, Qty, RejectReason, RequestKind, Side};

/// Order ids the simulator hands out to the strategy start here, well clear of any
/// feed ids.
pub const STRATEGY_ID_BASE: OrderId = 1 << 60;

/// A strategy reacts to market data, its own fills and a timer. All callbacks see
/// the world through [`Ctx`], which is already delayed by market-data latency; any
/// orders it sends reach the exchange after order-entry latency.
pub trait Strategy {
    /// Called once at time zero, before any market data.
    fn on_start(&mut self, _ctx: &mut Ctx) {}

    /// The strategy's view of the book changed. Read it with [`Ctx::book`].
    fn on_book_update(&mut self, ctx: &mut Ctx);

    /// One of the strategy's orders was filled.
    fn on_fill(&mut self, ctx: &mut Ctx, fill: &Fill);

    /// The periodic timer fired (see `timer_interval_ns`).
    fn on_timer(&mut self, _ctx: &mut Ctx) {}

    /// A public trade printed. Delivered before the book update it caused.
    fn on_trade(&mut self, _ctx: &mut Ctx, _trade: &PublicTrade) {}

    /// Ack, reject, cancel or amend confirmation for one of the strategy's orders.
    fn on_order_update(&mut self, _ctx: &mut Ctx, _update: &OrderUpdate) {}
}

/// A strategy that never trades. Useful as a baseline and in tests.
#[derive(Clone, Copy, Debug, Default)]
pub struct Idle;

impl Strategy for Idle {
    fn on_book_update(&mut self, _ctx: &mut Ctx) {}
    fn on_fill(&mut self, _ctx: &mut Ctx, _fill: &Fill) {}
}

#[derive(Clone, Copy, Debug, PartialEq, Serialize, Deserialize)]
pub struct Fill {
    pub order_id: OrderId,
    pub side: Side,
    pub price: Price,
    pub qty: Qty,
    /// Quantity still open on the order after this fill.
    pub leaves: Qty,
    pub liquidity: Liquidity,
    /// Exchange time of the fill (the strategy learns about it later).
    pub exchange_ts: u64,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct PublicTrade {
    pub exchange_ts: u64,
    pub price: Price,
    pub qty: Qty,
    pub aggressor: Side,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum OrderUpdate {
    Accepted {
        id: OrderId,
    },
    Rejected {
        id: OrderId,
        request: RequestKind,
        reason: RejectReason,
    },
    Cancelled {
        id: OrderId,
        qty: Qty,
        reason: CancelReason,
    },
    Amended {
        id: OrderId,
        price: Price,
        qty: Qty,
        kept_priority: bool,
    },
}

/// What the strategy is waiting to hear back about.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Pending {
    None,
    New,
    Cancel,
    Amend,
}

/// An order as the strategy believes it to be, given the messages it has seen.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct OpenOrder {
    pub id: OrderId,
    pub side: Side,
    pub order_type: OrderType,
    pub price: Price,
    pub qty: Qty,
    pub pending: Pending,
}

/// An instruction from the strategy, queued until the callback returns.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Action {
    New {
        id: OrderId,
        side: Side,
        order_type: OrderType,
        price: Price,
        qty: Qty,
    },
    Cancel {
        id: OrderId,
    },
    Amend {
        id: OrderId,
        price: Price,
        qty: Qty,
    },
}

/// The strategy's handle on the simulation.
///
/// Everything here is the strategy's own delayed view: the book as of market-data
/// latency ago, and a position and order list built only from messages that have
/// arrived. Orders it sends are queued and reach the exchange after order-entry
/// latency.
#[derive(Debug)]
pub struct Ctx {
    pub(crate) now: u64,
    pub(crate) book: L2Book,
    pub(crate) next_id: OrderId,
    pub(crate) actions: Vec<Action>,
    pub(crate) open: BTreeMap<OrderId, OpenOrder>,
    pub(crate) position: i64,
    pub(crate) stopped: bool,
}

impl Ctx {
    pub(crate) fn new() -> Self {
        Self {
            now: 0,
            book: L2Book::new(),
            next_id: STRATEGY_ID_BASE,
            actions: Vec::new(),
            open: BTreeMap::new(),
            position: 0,
            stopped: false,
        }
    }

    /// Simulation time in nanoseconds.
    pub fn now(&self) -> u64 {
        self.now
    }

    /// The strategy's (delayed) view of the book.
    pub fn book(&self) -> &L2Book {
        &self.book
    }

    /// Position from the fills the strategy has been told about.
    pub fn position(&self) -> i64 {
        self.position
    }

    /// Orders the strategy believes are open or in flight, by id.
    pub fn open_orders(&self) -> impl Iterator<Item = &OpenOrder> {
        self.open.values()
    }

    pub fn open_order(&self, id: OrderId) -> Option<&OpenOrder> {
        self.open.get(&id)
    }

    /// The id the next submitted order will get.
    pub fn peek_next_id(&self) -> OrderId {
        self.next_id
    }

    /// Sends a new order and returns its id.
    pub fn submit(&mut self, side: Side, order_type: OrderType, price: Price, qty: Qty) -> OrderId {
        let id = self.next_id;
        self.next_id += 1;
        self.actions.push(Action::New {
            id,
            side,
            order_type,
            price,
            qty,
        });
        self.open.insert(
            id,
            OpenOrder {
                id,
                side,
                order_type,
                price,
                qty,
                pending: Pending::New,
            },
        );
        id
    }

    pub fn limit(&mut self, side: Side, price: Price, qty: Qty) -> OrderId {
        self.submit(side, OrderType::Limit, price, qty)
    }

    pub fn post_only(&mut self, side: Side, price: Price, qty: Qty) -> OrderId {
        self.submit(side, OrderType::PostOnly, price, qty)
    }

    pub fn ioc(&mut self, side: Side, price: Price, qty: Qty) -> OrderId {
        self.submit(side, OrderType::Ioc, price, qty)
    }

    pub fn market(&mut self, side: Side, qty: Qty) -> OrderId {
        self.submit(side, OrderType::Market, 0, qty)
    }

    /// Requests a cancel. Returns false if the order is unknown or already being
    /// cancelled.
    pub fn cancel(&mut self, id: OrderId) -> bool {
        match self.open.get_mut(&id) {
            Some(o) if o.pending != Pending::Cancel => {
                o.pending = Pending::Cancel;
                self.actions.push(Action::Cancel { id });
                true
            }
            _ => false,
        }
    }

    /// Requests a cancel-replace. Returns false if the order is unknown or a cancel
    /// is already in flight.
    pub fn amend(&mut self, id: OrderId, price: Price, qty: Qty) -> bool {
        match self.open.get_mut(&id) {
            Some(o) if o.pending != Pending::Cancel => {
                o.pending = Pending::Amend;
                self.actions.push(Action::Amend { id, price, qty });
                true
            }
            _ => false,
        }
    }

    /// Cancels every open order not already being cancelled.
    pub fn cancel_all(&mut self) {
        let ids: Vec<_> = self.open.keys().copied().collect();
        for id in ids {
            self.cancel(id);
        }
    }

    /// Ends the backtest after the current callback.
    pub fn stop(&mut self) {
        self.stopped = true;
    }

    pub(crate) fn apply_fill(&mut self, fill: &Fill) {
        self.position += fill.side.sign() * fill.qty as i64;
        if fill.leaves == 0 {
            self.open.remove(&fill.order_id);
        } else if let Some(o) = self.open.get_mut(&fill.order_id) {
            o.qty = fill.leaves;
        }
    }

    pub(crate) fn apply_update(&mut self, update: &OrderUpdate) {
        match *update {
            OrderUpdate::Accepted { id } => {
                if let Some(o) = self.open.get_mut(&id)
                    && o.pending == Pending::New
                {
                    o.pending = Pending::None;
                }
            }
            OrderUpdate::Rejected {
                id,
                request,
                reason,
            } => match request {
                RequestKind::New => {
                    self.open.remove(&id);
                }
                _ if reason == RejectReason::UnknownOrder => {
                    self.open.remove(&id);
                }
                _ => {
                    if let Some(o) = self.open.get_mut(&id) {
                        o.pending = Pending::None;
                    }
                }
            },
            OrderUpdate::Cancelled { id, .. } => {
                self.open.remove(&id);
            }
            OrderUpdate::Amended { id, price, qty, .. } => {
                if let Some(o) = self.open.get_mut(&id) {
                    o.price = price;
                    o.qty = qty;
                    if o.pending == Pending::Amend {
                        o.pending = Pending::None;
                    }
                }
            }
        }
    }
}
