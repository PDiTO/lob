//! Core vocabulary shared by the book, the market data views and the simulator.
//!
//! Prices are integer ticks and quantities are integer lots. Nothing inside the
//! matching engine ever touches a float, so two runs over the same commands always
//! produce bit-identical books and fills.

use serde::{Deserialize, Serialize};

/// Price in ticks. Signed so spreads and rates products with negative prices work.
pub type Price = i64;
/// Quantity in lots.
pub type Qty = u64;
/// Order id, chosen by the caller. The book rejects duplicates of live orders.
pub type OrderId = u64;
/// Participant id used for self-trade prevention.
pub type OwnerId = u32;

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Side {
    Bid,
    Ask,
}

impl Side {
    #[inline]
    pub fn opposite(self) -> Side {
        match self {
            Side::Bid => Side::Ask,
            Side::Ask => Side::Bid,
        }
    }

    /// +1 for bids (buys), -1 for asks (sells).
    #[inline]
    pub fn sign(self) -> i64 {
        match self {
            Side::Bid => 1,
            Side::Ask => -1,
        }
    }

    /// True if a `self`-side order at `limit` is willing to trade at `price`.
    #[inline]
    pub fn accepts(self, limit: Price, price: Price) -> bool {
        match self {
            Side::Bid => price <= limit,
            Side::Ask => price >= limit,
        }
    }
}

/// How an incoming order behaves against the book.
///
/// | type        | matches on entry | remainder         |
/// |-------------|------------------|-------------------|
/// | `Limit`     | up to its price  | rests             |
/// | `Market`    | any price        | cancelled         |
/// | `Ioc`       | up to its price  | cancelled         |
/// | `Fok`       | all or nothing   | rejected if short |
/// | `PostOnly`  | never            | rests, or rejected if it would cross |
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum OrderType {
    Limit,
    Market,
    Ioc,
    Fok,
    PostOnly,
}

impl OrderType {
    /// Whether an unfilled remainder rests on the book.
    #[inline]
    pub fn rests(self) -> bool {
        matches!(self, OrderType::Limit | OrderType::PostOnly)
    }
}

/// A new order request.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct NewOrder {
    pub id: OrderId,
    pub owner: OwnerId,
    pub side: Side,
    pub order_type: OrderType,
    /// Limit price in ticks. Ignored for market orders.
    pub price: Price,
    pub qty: Qty,
}

impl NewOrder {
    pub fn limit(id: OrderId, owner: OwnerId, side: Side, price: Price, qty: Qty) -> Self {
        Self::new(id, owner, side, OrderType::Limit, price, qty)
    }

    pub fn market(id: OrderId, owner: OwnerId, side: Side, qty: Qty) -> Self {
        Self::new(id, owner, side, OrderType::Market, 0, qty)
    }

    pub fn ioc(id: OrderId, owner: OwnerId, side: Side, price: Price, qty: Qty) -> Self {
        Self::new(id, owner, side, OrderType::Ioc, price, qty)
    }

    pub fn fok(id: OrderId, owner: OwnerId, side: Side, price: Price, qty: Qty) -> Self {
        Self::new(id, owner, side, OrderType::Fok, price, qty)
    }

    pub fn post_only(id: OrderId, owner: OwnerId, side: Side, price: Price, qty: Qty) -> Self {
        Self::new(id, owner, side, OrderType::PostOnly, price, qty)
    }

    pub fn new(
        id: OrderId,
        owner: OwnerId,
        side: Side,
        order_type: OrderType,
        price: Price,
        qty: Qty,
    ) -> Self {
        Self {
            id,
            owner,
            side,
            order_type,
            price,
            qty,
        }
    }

    /// The price limit used for matching, or `None` for a market order.
    #[inline]
    pub fn limit_price(&self) -> Option<Price> {
        match self.order_type {
            OrderType::Market => None,
            _ => Some(self.price),
        }
    }
}

/// Everything the book accepts as input.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum Command {
    New(NewOrder),
    Cancel {
        id: OrderId,
    },
    /// Cancel-replace. `qty` is the new open (remaining) quantity.
    ///
    /// Reducing size at the same price keeps queue priority. Any price change or
    /// size increase sends the order to the back of the queue at its new price, and
    /// if the new price crosses the spread the order matches like a fresh one.
    Amend {
        id: OrderId,
        price: Price,
        qty: Qty,
    },
}

impl Command {
    pub fn order_id(&self) -> OrderId {
        match self {
            Command::New(o) => o.id,
            Command::Cancel { id } | Command::Amend { id, .. } => *id,
        }
    }
}

/// What a rejected request was.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RequestKind {
    New,
    Cancel,
    Amend,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RejectReason {
    /// A live order already has this id.
    DuplicateOrderId,
    /// Quantity was zero.
    ZeroQuantity,
    /// Cancel or amend of an id that is not resting (never existed, filled or cancelled).
    UnknownOrder,
    /// A post-only order (or an amend of one) would have taken liquidity.
    PostOnlyWouldCross,
    /// A fill-or-kill order could not be filled in full.
    FokUnfillable,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CancelReason {
    /// The owner asked for it.
    Requested,
    /// Unfilled remainder of a market or IOC order.
    Unfilled,
    /// Removed by self-trade prevention.
    SelfTrade,
}

/// Output of the book. A single command produces zero or more events, in this order:
/// the ack (`Accepted`, `Rejected` or `Amended`), then trades and cancels in the
/// order they happened, then one `BookUpdate` per price level whose aggregate changed.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum Event {
    Accepted {
        id: OrderId,
        owner: OwnerId,
        side: Side,
        order_type: OrderType,
        price: Price,
        qty: Qty,
    },
    Rejected {
        id: OrderId,
        request: RequestKind,
        reason: RejectReason,
    },
    /// One fill between a resting maker and an incoming taker, at the maker's price.
    Trade {
        maker_id: OrderId,
        maker_owner: OwnerId,
        taker_id: OrderId,
        taker_owner: OwnerId,
        /// Side of the taker (the aggressor). The maker is on the other side.
        taker_side: Side,
        price: Price,
        qty: Qty,
        /// Maker quantity left after this fill; zero means the maker is done.
        maker_remaining: Qty,
    },
    Cancelled {
        id: OrderId,
        owner: OwnerId,
        side: Side,
        price: Price,
        /// Quantity removed.
        qty: Qty,
        reason: CancelReason,
    },
    Amended {
        id: OrderId,
        side: Side,
        old_price: Price,
        old_qty: Qty,
        new_price: Price,
        new_qty: Qty,
        kept_priority: bool,
    },
    /// New aggregate state of one price level. `qty == 0` means the level is gone.
    BookUpdate {
        side: Side,
        price: Price,
        qty: Qty,
        orders: u32,
    },
}
