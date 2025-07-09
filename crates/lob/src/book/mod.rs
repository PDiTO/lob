//! The matching engine.
//!
//! # Layout
//!
//! * **Orders** live in a slab (`Vec<OrderNode>` plus a free list) and are addressed
//!   by `u32` index. Each node carries `prev`/`next` indices, so every price level is
//!   an intrusive doubly linked FIFO queue threaded through the slab. No per-order
//!   heap allocation, and a freed slot is reused by the next order.
//! * **Levels** live in a second slab. A level stores its price, aggregate quantity,
//!   order count and the head/tail of its queue.
//! * **Each side** is a [`Ladder`]: a sorted array of `(price, level)` pairs with the
//!   best price at the end (see `ladder.rs` for why an array rather than a tree).
//! * **An id index** (`FxHashMap<OrderId, u32>`) maps order ids to slab slots.
//!
//! Cancelling an order is a hash lookup plus an O(1) unlink. Only when the cancel
//! empties a level does it pay for removing the level from the ladder, which is a
//! `pop` at the touch and a binary search plus short shift elsewhere.

mod ladder;

use rustc_hash::FxHashMap;
use serde::{Deserialize, Serialize};

use crate::types::{
    CancelReason, Command, Event, NewOrder, OrderId, OrderType, OwnerId, Price, Qty, RejectReason,
    RequestKind, Side,
};
use ladder::Ladder;

const NIL: u32 = u32::MAX;

/// What happens when an incoming order would trade against a resting order with the
/// same [`OwnerId`].
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum StpMode {
    /// Cancel the resting order and keep matching the incoming one. This is the
    /// default: a market maker's aggressive hedge goes through and its own stale
    /// quote is pulled.
    #[default]
    CancelResting,
    /// Stop matching and cancel whatever is left of the incoming order. Fills that
    /// already happened against other owners stand.
    CancelIncoming,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct BookConfig {
    pub stp: StpMode,
    /// Emit [`Event::BookUpdate`] after each command. Turning this off saves a little
    /// work when nothing consumes L2 updates.
    pub emit_book_updates: bool,
}

impl Default for BookConfig {
    fn default() -> Self {
        Self {
            stp: StpMode::CancelResting,
            emit_book_updates: true,
        }
    }
}

/// Where a resting order joins its price level.
///
/// Real venues only do [`Placement::Back`]. `Front` exists so the backtester can
/// measure how much queue position is worth by comparing against a strategy that
/// always gets top priority. Do not use it for anything that pretends to be a venue.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Placement {
    #[default]
    Back,
    Front,
}

/// A resting order as seen from outside the book.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct RestingOrder {
    pub id: OrderId,
    pub owner: OwnerId,
    pub side: Side,
    pub price: Price,
    pub qty: Qty,
    /// Sequence number assigned when the order last gained queue priority.
    pub seq: u64,
    pub post_only: bool,
}

/// Aggregate state of one price level.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct LevelInfo {
    pub price: Price,
    pub qty: Qty,
    pub orders: u32,
}

#[derive(Clone, Copy, Debug)]
struct OrderNode {
    id: OrderId,
    seq: u64,
    qty: Qty,
    price: Price,
    owner: OwnerId,
    level: u32,
    prev: u32,
    next: u32,
    side: Side,
    post_only: bool,
}

#[derive(Clone, Copy, Debug)]
struct Level {
    price: Price,
    qty: Qty,
    count: u32,
    head: u32,
    tail: u32,
}

/// A price-time priority limit order book for one instrument.
#[derive(Clone, Debug)]
pub struct OrderBook {
    cfg: BookConfig,
    nodes: Vec<OrderNode>,
    free_nodes: Vec<u32>,
    levels: Vec<Level>,
    free_levels: Vec<u32>,
    bids: Ladder,
    asks: Ladder,
    index: FxHashMap<OrderId, u32>,
    seq: u64,
    /// Levels changed by the current command, for `BookUpdate` events.
    touched: Vec<(Side, Price)>,
}

impl Default for OrderBook {
    fn default() -> Self {
        Self::new(BookConfig::default())
    }
}

impl OrderBook {
    pub fn new(cfg: BookConfig) -> Self {
        Self {
            cfg,
            nodes: Vec::with_capacity(1024),
            free_nodes: Vec::new(),
            levels: Vec::with_capacity(256),
            free_levels: Vec::new(),
            bids: Ladder::new(Side::Bid),
            asks: Ladder::new(Side::Ask),
            index: FxHashMap::default(),
            seq: 0,
            touched: Vec::with_capacity(16),
        }
    }

    pub fn config(&self) -> &BookConfig {
        &self.cfg
    }

    // ----------------------------------------------------------------------------
    // Commands
    // ----------------------------------------------------------------------------

    /// Applies one command, appending the resulting events to `out`.
    pub fn process(&mut self, cmd: &Command, out: &mut Vec<Event>) {
        self.process_with_placement(cmd, Placement::Back, out);
    }

    /// Like [`process`](Self::process), but chooses where a resting remainder joins
    /// its level. See [`Placement`].
    pub fn process_with_placement(
        &mut self,
        cmd: &Command,
        placement: Placement,
        out: &mut Vec<Event>,
    ) {
        match *cmd {
            Command::New(order) => self.new_order(order, placement, out),
            Command::Cancel { id } => self.cancel(id, out),
            Command::Amend { id, price, qty } => self.amend(id, price, qty, placement, out),
        }
    }

    /// Submits a new order.
    pub fn submit(&mut self, order: NewOrder, out: &mut Vec<Event>) {
        self.new_order(order, Placement::Back, out);
    }

    fn new_order(&mut self, o: NewOrder, placement: Placement, out: &mut Vec<Event>) {
        let reject = |reason| Event::Rejected {
            id: o.id,
            request: RequestKind::New,
            reason,
        };
        if o.qty == 0 {
            out.push(reject(RejectReason::ZeroQuantity));
            return;
        }
        if self.index.contains_key(&o.id) {
            out.push(reject(RejectReason::DuplicateOrderId));
            return;
        }
        let limit = o.limit_price();
        match o.order_type {
            OrderType::PostOnly if self.crosses(o.side, o.price) => {
                out.push(reject(RejectReason::PostOnlyWouldCross));
                return;
            }
            OrderType::Fok if self.fillable(o.side, limit, o.owner, o.qty) < o.qty => {
                out.push(reject(RejectReason::FokUnfillable));
                return;
            }
            _ => {}
        }

        out.push(Event::Accepted {
            id: o.id,
            owner: o.owner,
            side: o.side,
            order_type: o.order_type,
            price: if limit.is_some() { o.price } else { 0 },
            qty: o.qty,
        });

        let (remaining, stp_stop) = self.match_incoming(o.id, o.owner, o.side, limit, o.qty, out);
        if remaining > 0 {
            if stp_stop || !o.order_type.rests() {
                debug_assert!(o.order_type != OrderType::Fok, "FOK pre-check was wrong");
                out.push(Event::Cancelled {
                    id: o.id,
                    owner: o.owner,
                    side: o.side,
                    price: if limit.is_some() { o.price } else { 0 },
                    qty: remaining,
                    reason: if stp_stop {
                        CancelReason::SelfTrade
                    } else {
                        CancelReason::Unfilled
                    },
                });
            } else {
                let post_only = o.order_type == OrderType::PostOnly;
                self.rest(
                    o.id, o.owner, o.side, o.price, remaining, post_only, placement,
                );
            }
        }
        self.flush_updates(out);
    }

    /// Cancels a resting order.
    pub fn cancel(&mut self, id: OrderId, out: &mut Vec<Event>) {
        let Some(&idx) = self.index.get(&id) else {
            out.push(Event::Rejected {
                id,
                request: RequestKind::Cancel,
                reason: RejectReason::UnknownOrder,
            });
            return;
        };
        let node = self.remove_node(idx);
        out.push(Event::Cancelled {
            id,
            owner: node.owner,
            side: node.side,
            price: node.price,
            qty: node.qty,
            reason: CancelReason::Requested,
        });
        self.flush_updates(out);
    }

    /// Cancel-replace. See [`Command::Amend`] for the priority rules.
    pub fn amend(
        &mut self,
        id: OrderId,
        price: Price,
        qty: Qty,
        placement: Placement,
        out: &mut Vec<Event>,
    ) {
        let reject = |reason| Event::Rejected {
            id,
            request: RequestKind::Amend,
            reason,
        };
        let Some(&idx) = self.index.get(&id) else {
            out.push(reject(RejectReason::UnknownOrder));
            return;
        };
        if qty == 0 {
            out.push(reject(RejectReason::ZeroQuantity));
            return;
        }
        let node = self.nodes[idx as usize];

        if price == node.price && qty <= node.qty {
            // Size down (or no change) at the same price: keep our place in the queue.
            let reduced = node.qty - qty;
            if reduced > 0 {
                self.nodes[idx as usize].qty = qty;
                self.levels[node.level as usize].qty -= reduced;
                self.touch(node.side, node.price);
            }
            out.push(Event::Amended {
                id,
                side: node.side,
                old_price: node.price,
                old_qty: node.qty,
                new_price: price,
                new_qty: qty,
                kept_priority: true,
            });
            self.flush_updates(out);
            return;
        }

        if node.post_only && self.crosses(node.side, price) {
            out.push(reject(RejectReason::PostOnlyWouldCross));
            return;
        }

        // Anything else loses priority: pull the order and send it back in at the new
        // price, matching first if it now crosses.
        self.remove_node(idx);
        out.push(Event::Amended {
            id,
            side: node.side,
            old_price: node.price,
            old_qty: node.qty,
            new_price: price,
            new_qty: qty,
            kept_priority: false,
        });
        let (remaining, stp_stop) =
            self.match_incoming(id, node.owner, node.side, Some(price), qty, out);
        if remaining > 0 {
            if stp_stop {
                out.push(Event::Cancelled {
                    id,
                    owner: node.owner,
                    side: node.side,
                    price,
                    qty: remaining,
                    reason: CancelReason::SelfTrade,
                });
            } else {
                self.rest(
                    id,
                    node.owner,
                    node.side,
                    price,
                    remaining,
                    node.post_only,
                    placement,
                );
            }
        }
        self.flush_updates(out);
    }

    // ----------------------------------------------------------------------------
    // Queries
    // ----------------------------------------------------------------------------

    /// Number of resting orders.
    pub fn len(&self) -> usize {
        self.index.len()
    }

    pub fn is_empty(&self) -> bool {
        self.index.is_empty()
    }

    /// Number of price levels on one side.
    pub fn level_count(&self, side: Side) -> usize {
        self.ladder(side).len()
    }

    pub fn best_bid(&self) -> Option<LevelInfo> {
        self.best(Side::Bid)
    }

    pub fn best_ask(&self) -> Option<LevelInfo> {
        self.best(Side::Ask)
    }

    pub fn best(&self, side: Side) -> Option<LevelInfo> {
        self.ladder(side).best().map(|(_, l)| self.level_info(l))
    }

    /// Aggregate quantity resting at `price` on `side`.
    pub fn qty_at(&self, side: Side, price: Price) -> Qty {
        self.ladder(side)
            .find(price)
            .map_or(0, |l| self.levels[l as usize].qty)
    }

    /// Price levels on one side, best first.
    pub fn levels(&self, side: Side) -> impl Iterator<Item = LevelInfo> + '_ {
        self.ladder(side).iter().map(|(_, l)| self.level_info(l))
    }

    /// Resting orders at one price level, in queue order.
    pub fn orders_at(&self, side: Side, price: Price) -> impl Iterator<Item = RestingOrder> + '_ {
        let head = self
            .ladder(side)
            .find(price)
            .map_or(NIL, |l| self.levels[l as usize].head);
        self.queue_from(head)
    }

    /// All resting orders on one side, in priority order (best price first, then FIFO).
    pub fn orders(&self, side: Side) -> impl Iterator<Item = RestingOrder> + '_ {
        self.ladder(side)
            .iter()
            .flat_map(move |(_, l)| self.queue_from(self.levels[l as usize].head))
    }

    pub fn order(&self, id: OrderId) -> Option<RestingOrder> {
        self.index
            .get(&id)
            .map(|&idx| self.resting(&self.nodes[idx as usize]))
    }

    /// Whether a `side` order at `price` would take liquidity.
    pub fn crosses(&self, side: Side, price: Price) -> bool {
        self.ladder(side.opposite())
            .best()
            .is_some_and(|(best, _)| side.accepts(price, best))
    }

    // ----------------------------------------------------------------------------
    // Internals
    // ----------------------------------------------------------------------------

    #[inline]
    fn ladder(&self, side: Side) -> &Ladder {
        match side {
            Side::Bid => &self.bids,
            Side::Ask => &self.asks,
        }
    }

    #[inline]
    fn ladder_mut(&mut self, side: Side) -> &mut Ladder {
        match side {
            Side::Bid => &mut self.bids,
            Side::Ask => &mut self.asks,
        }
    }

    fn level_info(&self, l: u32) -> LevelInfo {
        let lvl = &self.levels[l as usize];
        LevelInfo {
            price: lvl.price,
            qty: lvl.qty,
            orders: lvl.count,
        }
    }

    fn resting(&self, n: &OrderNode) -> RestingOrder {
        RestingOrder {
            id: n.id,
            owner: n.owner,
            side: n.side,
            price: n.price,
            qty: n.qty,
            seq: n.seq,
            post_only: n.post_only,
        }
    }

    fn queue_from(&self, head: u32) -> impl Iterator<Item = RestingOrder> + '_ {
        let mut cur = head;
        std::iter::from_fn(move || {
            if cur == NIL {
                return None;
            }
            let n = &self.nodes[cur as usize];
            cur = n.next;
            Some(self.resting(n))
        })
    }

    #[inline]
    fn touch(&mut self, side: Side, price: Price) {
        if self.touched.last() != Some(&(side, price)) {
            self.touched.push((side, price));
        }
    }

    fn flush_updates(&mut self, out: &mut Vec<Event>) {
        if self.cfg.emit_book_updates {
            for i in 0..self.touched.len() {
                let (side, price) = self.touched[i];
                if self.touched[..i].contains(&(side, price)) {
                    continue;
                }
                let (qty, orders) = match self.ladder(side).find(price) {
                    Some(l) => {
                        let lvl = &self.levels[l as usize];
                        (lvl.qty, lvl.count)
                    }
                    None => (0, 0),
                };
                out.push(Event::BookUpdate {
                    side,
                    price,
                    qty,
                    orders,
                });
            }
        }
        self.touched.clear();
    }

    /// How much of `want` could fill right now, honouring STP. Used by FOK.
    fn fillable(&self, side: Side, limit: Option<Price>, owner: OwnerId, want: Qty) -> Qty {
        let mut got: Qty = 0;
        for (price, l) in self.ladder(side.opposite()).iter() {
            if limit.is_some_and(|lim| !side.accepts(lim, price)) {
                break;
            }
            let mut cur = self.levels[l as usize].head;
            while cur != NIL {
                let n = &self.nodes[cur as usize];
                if n.owner == owner {
                    match self.cfg.stp {
                        StpMode::CancelResting => {
                            cur = n.next;
                            continue;
                        }
                        StpMode::CancelIncoming => return got,
                    }
                }
                got += n.qty;
                if got >= want {
                    return got;
                }
                cur = n.next;
            }
        }
        got
    }

    /// Matches an incoming order against the opposite side. Returns the unfilled
    /// quantity and whether matching stopped because of self-trade prevention.
    fn match_incoming(
        &mut self,
        taker_id: OrderId,
        owner: OwnerId,
        side: Side,
        limit: Option<Price>,
        mut remaining: Qty,
        out: &mut Vec<Event>,
    ) -> (Qty, bool) {
        let contra = side.opposite();
        while remaining > 0 {
            let Some((price, l)) = self.ladder(contra).best() else {
                break;
            };
            if limit.is_some_and(|lim| !side.accepts(lim, price)) {
                break;
            }
            self.touch(contra, price);
            let mut cur = self.levels[l as usize].head;
            while remaining > 0 && cur != NIL {
                let n = self.nodes[cur as usize];
                let next = n.next;
                if n.owner == owner {
                    match self.cfg.stp {
                        StpMode::CancelResting => {
                            self.unlink_and_free(cur);
                            out.push(Event::Cancelled {
                                id: n.id,
                                owner: n.owner,
                                side: n.side,
                                price: n.price,
                                qty: n.qty,
                                reason: CancelReason::SelfTrade,
                            });
                            cur = next;
                            continue;
                        }
                        StpMode::CancelIncoming => {
                            self.drop_level_if_empty(contra, l);
                            return (remaining, true);
                        }
                    }
                }
                let fill = remaining.min(n.qty);
                remaining -= fill;
                let maker_remaining = n.qty - fill;
                self.levels[l as usize].qty -= fill;
                out.push(Event::Trade {
                    maker_id: n.id,
                    maker_owner: n.owner,
                    taker_id,
                    taker_owner: owner,
                    taker_side: side,
                    price,
                    qty: fill,
                    maker_remaining,
                });
                if maker_remaining == 0 {
                    // `unlink_and_free` subtracts the node's qty from the level again,
                    // so zero it first.
                    self.nodes[cur as usize].qty = 0;
                    self.unlink_and_free(cur);
                } else {
                    self.nodes[cur as usize].qty = maker_remaining;
                }
                cur = next;
            }
            self.drop_level_if_empty(contra, l);
        }
        (remaining, false)
    }

    fn drop_level_if_empty(&mut self, side: Side, l: u32) {
        let lvl = self.levels[l as usize];
        if lvl.count == 0 {
            self.ladder_mut(side).remove(lvl.price);
            self.free_levels.push(l);
        }
    }

    #[allow(clippy::too_many_arguments)]
    fn rest(
        &mut self,
        id: OrderId,
        owner: OwnerId,
        side: Side,
        price: Price,
        qty: Qty,
        post_only: bool,
        placement: Placement,
    ) {
        let levels = &mut self.levels;
        let free_levels = &mut self.free_levels;
        let ladder = match side {
            Side::Bid => &mut self.bids,
            Side::Ask => &mut self.asks,
        };
        let l = ladder.find_or_insert_with(price, || {
            let fresh = Level {
                price,
                qty: 0,
                count: 0,
                head: NIL,
                tail: NIL,
            };
            match free_levels.pop() {
                Some(l) => {
                    levels[l as usize] = fresh;
                    l
                }
                None => {
                    levels.push(fresh);
                    (levels.len() - 1) as u32
                }
            }
        });

        self.seq += 1;
        let node = OrderNode {
            id,
            seq: self.seq,
            qty,
            price,
            owner,
            level: l,
            prev: NIL,
            next: NIL,
            side,
            post_only,
        };
        let idx = match self.free_nodes.pop() {
            Some(i) => {
                self.nodes[i as usize] = node;
                i
            }
            None => {
                self.nodes.push(node);
                (self.nodes.len() - 1) as u32
            }
        };

        let lvl = &mut self.levels[l as usize];
        match placement {
            Placement::Back => {
                let tail = lvl.tail;
                self.nodes[idx as usize].prev = tail;
                if tail == NIL {
                    lvl.head = idx;
                } else {
                    self.nodes[tail as usize].next = idx;
                }
                lvl.tail = idx;
            }
            Placement::Front => {
                let head = lvl.head;
                self.nodes[idx as usize].next = head;
                if head == NIL {
                    lvl.tail = idx;
                } else {
                    self.nodes[head as usize].prev = idx;
                }
                lvl.head = idx;
            }
        }
        lvl.qty += qty;
        lvl.count += 1;
        self.index.insert(id, idx);
        self.touch(side, price);
    }

    /// Unlinks a node from its level queue, drops it from the index and frees the
    /// slot. Leaves an empty level in place (callers decide when to drop it).
    fn unlink_and_free(&mut self, idx: u32) {
        let n = self.nodes[idx as usize];
        let lvl = &mut self.levels[n.level as usize];
        if n.prev == NIL {
            lvl.head = n.next;
        } else {
            self.nodes[n.prev as usize].next = n.next;
        }
        if n.next == NIL {
            lvl.tail = n.prev;
        } else {
            self.nodes[n.next as usize].prev = n.prev;
        }
        lvl.qty -= n.qty;
        lvl.count -= 1;
        self.index.remove(&n.id);
        self.free_nodes.push(idx);
    }

    /// Removes an order entirely, dropping its level if it empties. Returns a copy.
    fn remove_node(&mut self, idx: u32) -> OrderNode {
        let n = self.nodes[idx as usize];
        self.unlink_and_free(idx);
        self.drop_level_if_empty(n.side, n.level);
        self.touch(n.side, n.price);
        n
    }
}
