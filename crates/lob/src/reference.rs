//! A deliberately naive matching engine, used as an oracle in differential tests.
//!
//! Every resting order sits in one flat `Vec`. Finding the next order to match is a
//! linear scan for the best price and then the lowest sequence number. A FOK order
//! is checked by running the match on a clone of the whole book and seeing whether
//! it filled. None of that is fast, but each rule is written down in the most
//! obvious way possible, so it is easy to convince yourself it is right. The real
//! engine must produce exactly the same events (apart from `BookUpdate`, which this
//! one does not emit) and the same resting orders.

use crate::book::{RestingOrder, StpMode};
use crate::types::{
    CancelReason, Command, Event, NewOrder, OrderId, OrderType, OwnerId, Price, Qty, RejectReason,
    RequestKind, Side,
};

#[derive(Clone, Debug, Default)]
pub struct ReferenceBook {
    stp: StpMode,
    orders: Vec<RestingOrder>,
    seq: u64,
}

impl ReferenceBook {
    pub fn new(stp: StpMode) -> Self {
        Self {
            stp,
            orders: Vec::new(),
            seq: 0,
        }
    }

    /// Resting orders on one side in priority order.
    pub fn orders(&self, side: Side) -> Vec<RestingOrder> {
        let mut v: Vec<_> = self
            .orders
            .iter()
            .filter(|o| o.side == side)
            .copied()
            .collect();
        v.sort_by(|a, b| match side {
            Side::Bid => b.price.cmp(&a.price).then(a.seq.cmp(&b.seq)),
            Side::Ask => a.price.cmp(&b.price).then(a.seq.cmp(&b.seq)),
        });
        v
    }

    pub fn process(&mut self, cmd: &Command, out: &mut Vec<Event>) {
        match *cmd {
            Command::New(o) => self.new_order(o, out),
            Command::Cancel { id } => self.cancel(id, out),
            Command::Amend { id, price, qty } => self.amend(id, price, qty, out),
        }
    }

    fn position(&self, id: OrderId) -> Option<usize> {
        self.orders.iter().position(|o| o.id == id)
    }

    fn would_cross(&self, side: Side, price: Price) -> bool {
        self.orders
            .iter()
            .any(|o| o.side == side.opposite() && side.accepts(price, o.price))
    }

    /// Index of the next order an incoming `side` order at `limit` would hit.
    fn next_match(&self, side: Side, limit: Option<Price>) -> Option<usize> {
        let mut best: Option<usize> = None;
        for (i, o) in self.orders.iter().enumerate() {
            if o.side != side.opposite() {
                continue;
            }
            if let Some(lim) = limit
                && !side.accepts(lim, o.price)
            {
                continue;
            }
            best = match best {
                None => Some(i),
                Some(j) => {
                    let b = &self.orders[j];
                    let better_price = match side {
                        Side::Bid => o.price < b.price,
                        Side::Ask => o.price > b.price,
                    };
                    if better_price || (o.price == b.price && o.seq < b.seq) {
                        Some(i)
                    } else {
                        Some(j)
                    }
                }
            };
        }
        best
    }

    fn run_match(
        &mut self,
        taker: OrderId,
        owner: OwnerId,
        side: Side,
        limit: Option<Price>,
        mut qty: Qty,
        out: &mut Vec<Event>,
    ) -> (Qty, bool) {
        while qty > 0 {
            let Some(i) = self.next_match(side, limit) else {
                break;
            };
            let maker = self.orders[i];
            if maker.owner == owner {
                match self.stp {
                    StpMode::CancelIncoming => return (qty, true),
                    StpMode::CancelResting => {
                        self.orders.remove(i);
                        out.push(Event::Cancelled {
                            id: maker.id,
                            owner: maker.owner,
                            side: maker.side,
                            price: maker.price,
                            qty: maker.qty,
                            reason: CancelReason::SelfTrade,
                        });
                        continue;
                    }
                }
            }
            let fill = qty.min(maker.qty);
            qty -= fill;
            self.orders[i].qty -= fill;
            out.push(Event::Trade {
                maker_id: maker.id,
                maker_owner: maker.owner,
                taker_id: taker,
                taker_owner: owner,
                taker_side: side,
                price: maker.price,
                qty: fill,
                maker_remaining: maker.qty - fill,
            });
            if self.orders[i].qty == 0 {
                self.orders.remove(i);
            }
        }
        (qty, false)
    }

    fn rest(&mut self, order: RestingOrder) {
        self.seq += 1;
        self.orders.push(RestingOrder {
            seq: self.seq,
            ..order
        });
    }

    fn new_order(&mut self, o: NewOrder, out: &mut Vec<Event>) {
        let reject = |reason| Event::Rejected {
            id: o.id,
            request: RequestKind::New,
            reason,
        };
        if o.qty == 0 {
            return out.push(reject(RejectReason::ZeroQuantity));
        }
        if self.position(o.id).is_some() {
            return out.push(reject(RejectReason::DuplicateOrderId));
        }
        let limit = o.limit_price();
        if o.order_type == OrderType::PostOnly && self.would_cross(o.side, o.price) {
            return out.push(reject(RejectReason::PostOnlyWouldCross));
        }
        if o.order_type == OrderType::Fok {
            let mut probe = self.clone();
            let (left, _) = probe.run_match(o.id, o.owner, o.side, limit, o.qty, &mut Vec::new());
            if left > 0 {
                return out.push(reject(RejectReason::FokUnfillable));
            }
        }
        let price = if limit.is_some() { o.price } else { 0 };
        out.push(Event::Accepted {
            id: o.id,
            owner: o.owner,
            side: o.side,
            order_type: o.order_type,
            price,
            qty: o.qty,
        });
        let (left, stp) = self.run_match(o.id, o.owner, o.side, limit, o.qty, out);
        if left == 0 {
            return;
        }
        if stp || !o.order_type.rests() {
            out.push(Event::Cancelled {
                id: o.id,
                owner: o.owner,
                side: o.side,
                price,
                qty: left,
                reason: if stp {
                    CancelReason::SelfTrade
                } else {
                    CancelReason::Unfilled
                },
            });
        } else {
            self.rest(RestingOrder {
                id: o.id,
                owner: o.owner,
                side: o.side,
                price: o.price,
                qty: left,
                seq: 0,
                post_only: o.order_type == OrderType::PostOnly,
            });
        }
    }

    fn cancel(&mut self, id: OrderId, out: &mut Vec<Event>) {
        match self.position(id) {
            None => out.push(Event::Rejected {
                id,
                request: RequestKind::Cancel,
                reason: RejectReason::UnknownOrder,
            }),
            Some(i) => {
                let o = self.orders.remove(i);
                out.push(Event::Cancelled {
                    id,
                    owner: o.owner,
                    side: o.side,
                    price: o.price,
                    qty: o.qty,
                    reason: CancelReason::Requested,
                });
            }
        }
    }

    fn amend(&mut self, id: OrderId, price: Price, qty: Qty, out: &mut Vec<Event>) {
        let reject = |reason| Event::Rejected {
            id,
            request: RequestKind::Amend,
            reason,
        };
        let Some(i) = self.position(id) else {
            return out.push(reject(RejectReason::UnknownOrder));
        };
        if qty == 0 {
            return out.push(reject(RejectReason::ZeroQuantity));
        }
        let o = self.orders[i];
        let keep = price == o.price && qty <= o.qty;
        if !keep && o.post_only && self.would_cross(o.side, price) {
            return out.push(reject(RejectReason::PostOnlyWouldCross));
        }
        out.push(Event::Amended {
            id,
            side: o.side,
            old_price: o.price,
            old_qty: o.qty,
            new_price: price,
            new_qty: qty,
            kept_priority: keep,
        });
        if keep {
            self.orders[i].qty = qty;
            return;
        }
        self.orders.remove(i);
        let (left, stp) = self.run_match(id, o.owner, o.side, Some(price), qty, out);
        if left == 0 {
            return;
        }
        if stp {
            out.push(Event::Cancelled {
                id,
                owner: o.owner,
                side: o.side,
                price,
                qty: left,
                reason: CancelReason::SelfTrade,
            });
        } else {
            self.rest(RestingOrder {
                price,
                qty: left,
                ..o
            });
        }
    }
}
