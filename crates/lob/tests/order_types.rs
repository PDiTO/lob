//! Behaviour of each order type and the edge cases around them.

use lob::{
    BookConfig, CancelReason, Command, DepthView, Event, NewOrder, OrderBook, OrderType, Qty,
    RejectReason, RequestKind, Side, StpMode,
};

use Side::{Ask, Bid};

/// Owners used below. STP only kicks in when two orders share an owner.
const ALICE: u32 = 1;
const BOB: u32 = 2;
const CAROL: u32 = 3;

struct Harness {
    book: OrderBook,
    events: Vec<Event>,
}

impl Harness {
    fn new() -> Self {
        Self::with_stp(StpMode::CancelResting)
    }

    fn with_stp(stp: StpMode) -> Self {
        Self {
            book: OrderBook::new(BookConfig {
                stp,
                ..BookConfig::default()
            }),
            events: Vec::new(),
        }
    }

    /// Runs one command and returns just its events.
    fn run(&mut self, cmd: Command) -> Vec<Event> {
        self.events.clear();
        self.book.process(&cmd, &mut self.events);
        self.events.clone()
    }

    fn new_order(&mut self, o: NewOrder) -> Vec<Event> {
        self.run(Command::New(o))
    }

    fn limit(&mut self, id: u64, owner: u32, side: Side, price: i64, qty: Qty) -> Vec<Event> {
        self.new_order(NewOrder::limit(id, owner, side, price, qty))
    }

    fn cancel(&mut self, id: u64) -> Vec<Event> {
        self.run(Command::Cancel { id })
    }

    fn amend(&mut self, id: u64, price: i64, qty: Qty) -> Vec<Event> {
        self.run(Command::Amend { id, price, qty })
    }

    fn queue(&self, side: Side, price: i64) -> Vec<(u64, Qty)> {
        self.book
            .orders_at(side, price)
            .map(|o| (o.id, o.qty))
            .collect()
    }
}

/// (maker, taker, price, qty) for each trade, in order.
fn trades(events: &[Event]) -> Vec<(u64, u64, i64, Qty)> {
    events
        .iter()
        .filter_map(|e| match *e {
            Event::Trade {
                maker_id,
                taker_id,
                price,
                qty,
                ..
            } => Some((maker_id, taker_id, price, qty)),
            _ => None,
        })
        .collect()
}

/// (id, qty, reason) for each cancel, in order.
fn cancels(events: &[Event]) -> Vec<(u64, Qty, CancelReason)> {
    events
        .iter()
        .filter_map(|e| match *e {
            Event::Cancelled {
                id, qty, reason, ..
            } => Some((id, qty, reason)),
            _ => None,
        })
        .collect()
}

fn rejection(events: &[Event]) -> Option<(RequestKind, RejectReason)> {
    match events {
        [
            Event::Rejected {
                request, reason, ..
            },
        ] => Some((*request, *reason)),
        _ => None,
    }
}

/// A book with asks 101 x 5 (id 1), 101 x 5 (id 2), 102 x 10 (id 3), bids 99 x 5 (id 4).
fn seeded() -> Harness {
    let mut h = Harness::new();
    h.limit(1, ALICE, Ask, 101, 5);
    h.limit(2, BOB, Ask, 101, 5);
    h.limit(3, ALICE, Ask, 102, 10);
    h.limit(4, BOB, Bid, 99, 5);
    h
}

// ---------------------------------------------------------------------------------
// Limit orders
// ---------------------------------------------------------------------------------

#[test]
fn non_crossing_limit_rests_and_publishes_its_level() {
    let mut h = Harness::new();
    let ev = h.limit(1, ALICE, Bid, 100, 7);
    assert_eq!(
        ev,
        vec![
            Event::Accepted {
                id: 1,
                owner: ALICE,
                side: Bid,
                order_type: OrderType::Limit,
                price: 100,
                qty: 7
            },
            Event::BookUpdate {
                side: Bid,
                price: 100,
                qty: 7,
                orders: 1
            },
        ]
    );
    assert_eq!(h.book.best_bid().unwrap().price, 100);
    assert_eq!(h.book.len(), 1);
}

#[test]
fn crossing_limit_fills_across_levels_at_maker_prices_then_rests() {
    let mut h = seeded();
    let ev = h.limit(10, CAROL, Bid, 102, 14);
    assert_eq!(
        trades(&ev),
        vec![(1, 10, 101, 5), (2, 10, 101, 5), (3, 10, 102, 4)]
    );
    // Level 101 is gone, 102 is down to 6, and nothing of ours rests.
    assert_eq!(h.book.qty_at(Ask, 101), 0);
    assert_eq!(h.book.qty_at(Ask, 102), 6);
    assert!(h.book.order(10).is_none());

    // Book updates come after the trades, one per level touched.
    let updates: Vec<_> = ev
        .iter()
        .filter(|e| matches!(e, Event::BookUpdate { .. }))
        .collect();
    assert_eq!(updates.len(), 2);
    assert!(matches!(
        ev.last(),
        Some(Event::BookUpdate {
            side: Ask,
            price: 102,
            qty: 6,
            ..
        })
    ));
}

#[test]
fn crossing_limit_remainder_rests_at_its_own_price() {
    let mut h = seeded();
    let ev = h.limit(10, CAROL, Bid, 101, 12);
    assert_eq!(trades(&ev), vec![(1, 10, 101, 5), (2, 10, 101, 5)]);
    assert_eq!(h.book.best_bid().unwrap().price, 101);
    assert_eq!(h.book.best_bid().unwrap().qty, 2);
    assert_eq!(h.book.best_ask().unwrap().price, 102);
    assert!(!h.book.is_crossed());
}

#[test]
fn fifo_within_a_level() {
    let mut h = seeded();
    let ev = h.limit(10, CAROL, Bid, 101, 7);
    assert_eq!(trades(&ev), vec![(1, 10, 101, 5), (2, 10, 101, 2)]);
    assert_eq!(h.queue(Ask, 101), vec![(2, 3)]);
}

#[test]
fn trade_reports_maker_remaining() {
    let mut h = seeded();
    let ev = h.limit(10, CAROL, Bid, 101, 2);
    assert!(ev.iter().any(|e| matches!(
        e,
        Event::Trade {
            maker_id: 1,
            maker_remaining: 3,
            taker_side: Bid,
            ..
        }
    )));
}

#[test]
fn zero_quantity_and_duplicate_ids_are_rejected() {
    let mut h = seeded();
    assert_eq!(
        rejection(&h.limit(20, CAROL, Bid, 90, 0)),
        Some((RequestKind::New, RejectReason::ZeroQuantity))
    );
    assert_eq!(
        rejection(&h.limit(4, CAROL, Bid, 90, 1)),
        Some((RequestKind::New, RejectReason::DuplicateOrderId))
    );
    assert_eq!(h.book.order(4).unwrap().price, 99, "original untouched");
}

#[test]
fn ids_can_be_reused_once_the_order_is_gone() {
    let mut h = seeded();
    h.cancel(4);
    let ev = h.limit(4, CAROL, Bid, 98, 1);
    assert!(matches!(ev[0], Event::Accepted { id: 4, .. }));
}

#[test]
fn negative_prices_work() {
    let mut h = Harness::new();
    h.limit(1, ALICE, Ask, -5, 3);
    h.limit(2, ALICE, Ask, -2, 3);
    let ev = h.limit(3, BOB, Bid, -4, 4);
    assert_eq!(trades(&ev), vec![(1, 3, -5, 3)]);
    assert_eq!(h.book.best_bid().unwrap().price, -4);
    assert_eq!(h.book.best_ask().unwrap().price, -2);
}

// ---------------------------------------------------------------------------------
// Market and IOC
// ---------------------------------------------------------------------------------

#[test]
fn market_order_sweeps_and_cancels_the_rest() {
    let mut h = seeded();
    let ev = h.new_order(NewOrder::market(10, CAROL, Bid, 25));
    assert_eq!(
        trades(&ev),
        vec![(1, 10, 101, 5), (2, 10, 101, 5), (3, 10, 102, 10)]
    );
    assert_eq!(cancels(&ev), vec![(10, 5, CancelReason::Unfilled)]);
    assert!(h.book.best_ask().is_none());
    assert!(h.book.order(10).is_none());
}

#[test]
fn market_order_into_empty_side_is_accepted_then_cancelled() {
    let mut h = Harness::new();
    let ev = h.new_order(NewOrder::market(1, ALICE, Ask, 3));
    assert!(matches!(ev[0], Event::Accepted { qty: 3, .. }));
    assert_eq!(cancels(&ev), vec![(1, 3, CancelReason::Unfilled)]);
    assert!(h.book.is_empty());
}

#[test]
fn ioc_fills_up_to_its_limit_and_never_rests() {
    let mut h = seeded();
    let ev = h.new_order(NewOrder::ioc(10, CAROL, Bid, 101, 12));
    assert_eq!(trades(&ev), vec![(1, 10, 101, 5), (2, 10, 101, 5)]);
    assert_eq!(cancels(&ev), vec![(10, 2, CancelReason::Unfilled)]);
    assert!(h.book.best_bid().unwrap().price < 101);
}

#[test]
fn ioc_that_does_not_cross_is_cancelled_in_full() {
    let mut h = seeded();
    let ev = h.new_order(NewOrder::ioc(10, CAROL, Bid, 100, 4));
    assert!(trades(&ev).is_empty());
    assert_eq!(cancels(&ev), vec![(10, 4, CancelReason::Unfilled)]);
}

// ---------------------------------------------------------------------------------
// FOK
// ---------------------------------------------------------------------------------

#[test]
fn fok_that_cannot_fill_is_rejected_without_side_effects() {
    let mut h = seeded();
    let before = h.book.l3_snapshot();
    // 20 available at <= 102, ask for 21.
    let ev = h.new_order(NewOrder::fok(10, CAROL, Bid, 102, 21));
    assert_eq!(
        rejection(&ev),
        Some((RequestKind::New, RejectReason::FokUnfillable))
    );
    assert_eq!(h.book.l3_snapshot(), before);
}

#[test]
fn fok_respects_its_limit_price() {
    let mut h = seeded();
    // Plenty of size overall, but only 10 at or below 101.
    let ev = h.new_order(NewOrder::fok(10, CAROL, Bid, 101, 11));
    assert_eq!(
        rejection(&ev),
        Some((RequestKind::New, RejectReason::FokUnfillable))
    );
}

#[test]
fn fok_that_can_fill_exactly_does_so_across_levels() {
    let mut h = seeded();
    let ev = h.new_order(NewOrder::fok(10, CAROL, Bid, 102, 20));
    assert_eq!(trades(&ev).iter().map(|t| t.3).sum::<Qty>(), 20);
    assert!(cancels(&ev).is_empty());
    assert!(h.book.best_ask().is_none());
}

#[test]
fn fok_does_not_count_own_orders_as_liquidity() {
    // Alice owns 15 of the 20 lots on offer. With cancel-resting STP her orders would
    // be pulled rather than filled, so only Bob's 5 count.
    let mut h = seeded();
    let ev = h.new_order(NewOrder::fok(10, ALICE, Bid, 102, 6));
    assert_eq!(
        rejection(&ev),
        Some((RequestKind::New, RejectReason::FokUnfillable))
    );
    let ev = h.new_order(NewOrder::fok(11, ALICE, Bid, 102, 5));
    assert_eq!(trades(&ev), vec![(2, 11, 101, 5)]);
    // Alice's order 1 was ahead of Bob's at 101, so STP cancelled it on the way.
    assert_eq!(cancels(&ev), vec![(1, 5, CancelReason::SelfTrade)]);
}

#[test]
fn fok_with_cancel_incoming_stp_stops_at_the_first_own_order() {
    let mut h = Harness::with_stp(StpMode::CancelIncoming);
    h.limit(1, BOB, Ask, 101, 5);
    h.limit(2, ALICE, Ask, 101, 5);
    h.limit(3, BOB, Ask, 101, 5);
    // Matching would stop at Alice's order 2, so only 5 is reachable.
    let ev = h.new_order(NewOrder::fok(10, ALICE, Bid, 101, 6));
    assert_eq!(
        rejection(&ev),
        Some((RequestKind::New, RejectReason::FokUnfillable))
    );
    let ev = h.new_order(NewOrder::fok(11, ALICE, Bid, 101, 5));
    assert_eq!(trades(&ev), vec![(1, 11, 101, 5)]);
}

// ---------------------------------------------------------------------------------
// Post-only
// ---------------------------------------------------------------------------------

#[test]
fn post_only_that_would_cross_is_rejected() {
    let mut h = seeded();
    let before = h.book.l3_snapshot();
    let ev = h.new_order(NewOrder::post_only(10, CAROL, Bid, 101, 1));
    assert_eq!(
        rejection(&ev),
        Some((RequestKind::New, RejectReason::PostOnlyWouldCross))
    );
    assert_eq!(h.book.l3_snapshot(), before);
    let ev = h.new_order(NewOrder::post_only(11, CAROL, Ask, 99, 1));
    assert_eq!(
        rejection(&ev),
        Some((RequestKind::New, RejectReason::PostOnlyWouldCross))
    );
}

#[test]
fn post_only_inside_the_spread_rests() {
    let mut h = seeded();
    let ev = h.new_order(NewOrder::post_only(10, CAROL, Bid, 100, 3));
    assert!(trades(&ev).is_empty());
    assert_eq!(h.book.best_bid().unwrap().price, 100);
    assert!(h.book.order(10).unwrap().post_only);
}

#[test]
fn post_only_into_an_empty_book_rests() {
    let mut h = Harness::new();
    h.new_order(NewOrder::post_only(1, ALICE, Ask, 50, 1));
    assert_eq!(h.book.best_ask().unwrap().price, 50);
}

// ---------------------------------------------------------------------------------
// Self-trade prevention
// ---------------------------------------------------------------------------------

#[test]
fn stp_cancel_resting_pulls_own_order_and_keeps_matching() {
    let mut h = seeded();
    // Alice buys 8 at 101. Her own order 1 is first in the queue; it is cancelled and
    // she trades with Bob's order 2 behind it, then rests the remainder.
    let ev = h.limit(10, ALICE, Bid, 101, 8);
    assert_eq!(cancels(&ev), vec![(1, 5, CancelReason::SelfTrade)]);
    assert_eq!(trades(&ev), vec![(2, 10, 101, 5)]);
    assert_eq!(h.book.best_bid().unwrap().price, 101);
    assert_eq!(h.book.best_bid().unwrap().qty, 3);
    assert!(h.book.order(1).is_none());
}

#[test]
fn stp_cancel_incoming_keeps_earlier_fills_and_cancels_the_rest() {
    let mut h = Harness::with_stp(StpMode::CancelIncoming);
    h.limit(1, BOB, Ask, 101, 5);
    h.limit(2, ALICE, Ask, 101, 5);
    h.limit(3, BOB, Ask, 102, 5);
    let ev = h.limit(10, ALICE, Bid, 102, 12);
    assert_eq!(trades(&ev), vec![(1, 10, 101, 5)]);
    assert_eq!(cancels(&ev), vec![(10, 7, CancelReason::SelfTrade)]);
    // Alice's resting ask is untouched and her bid does not rest.
    assert_eq!(h.queue(Ask, 101), vec![(2, 5)]);
    assert!(h.book.order(10).is_none());
    assert!(h.book.best_bid().is_none());
}

#[test]
fn stp_applies_to_market_orders() {
    let mut h = seeded();
    let ev = h.new_order(NewOrder::market(10, ALICE, Bid, 30));
    // Alice's 1 and 3 are pulled; only Bob's 2 trades.
    assert_eq!(trades(&ev), vec![(2, 10, 101, 5)]);
    assert_eq!(
        cancels(&ev),
        vec![
            (1, 5, CancelReason::SelfTrade),
            (3, 10, CancelReason::SelfTrade),
            (10, 25, CancelReason::Unfilled)
        ]
    );
    assert!(h.book.best_ask().is_none());
}

// ---------------------------------------------------------------------------------
// Cancel
// ---------------------------------------------------------------------------------

#[test]
fn cancel_removes_the_order_and_publishes_the_level() {
    let mut h = seeded();
    let ev = h.cancel(1);
    assert_eq!(cancels(&ev), vec![(1, 5, CancelReason::Requested)]);
    assert!(ev.contains(&Event::BookUpdate {
        side: Ask,
        price: 101,
        qty: 5,
        orders: 1
    }));
    assert_eq!(h.queue(Ask, 101), vec![(2, 5)]);
}

#[test]
fn cancel_of_last_order_removes_the_level() {
    let mut h = seeded();
    let ev = h.cancel(4);
    assert!(ev.contains(&Event::BookUpdate {
        side: Bid,
        price: 99,
        qty: 0,
        orders: 0
    }));
    assert!(h.book.best_bid().is_none());
    assert_eq!(h.book.level_count(Bid), 0);
}

#[test]
fn cancel_of_unknown_or_finished_order_is_rejected() {
    let mut h = seeded();
    let unknown = Some((RequestKind::Cancel, RejectReason::UnknownOrder));
    assert_eq!(rejection(&h.cancel(999)), unknown);

    h.cancel(4);
    assert_eq!(rejection(&h.cancel(4)), unknown, "double cancel");

    h.limit(10, CAROL, Bid, 101, 5); // fills order 1 completely
    assert_eq!(rejection(&h.cancel(1)), unknown, "cancel after fill");
}

// ---------------------------------------------------------------------------------
// Amend (cancel-replace)
// ---------------------------------------------------------------------------------

#[test]
fn amend_down_at_same_price_keeps_priority() {
    let mut h = seeded();
    let ev = h.amend(1, 101, 2);
    assert!(matches!(
        ev[0],
        Event::Amended {
            kept_priority: true,
            new_qty: 2,
            ..
        }
    ));
    assert_eq!(h.queue(Ask, 101), vec![(1, 2), (2, 5)]);
    assert_eq!(h.book.qty_at(Ask, 101), 7);
    // Still first in line.
    let ev = h.limit(10, CAROL, Bid, 101, 1);
    assert_eq!(trades(&ev), vec![(1, 10, 101, 1)]);
}

#[test]
fn amend_up_loses_priority() {
    let mut h = seeded();
    let ev = h.amend(1, 101, 6);
    assert!(matches!(
        ev[0],
        Event::Amended {
            kept_priority: false,
            ..
        }
    ));
    assert_eq!(h.queue(Ask, 101), vec![(2, 5), (1, 6)]);
}

#[test]
fn amend_price_change_loses_priority_even_when_moving_back() {
    let mut h = seeded();
    h.amend(1, 102, 5);
    h.amend(1, 101, 5);
    assert_eq!(h.queue(Ask, 101), vec![(2, 5), (1, 5)]);
}

#[test]
fn amend_to_a_crossing_price_trades() {
    let mut h = seeded();
    h.limit(5, CAROL, Bid, 98, 3);
    let ev = h.amend(5, 101, 7);
    assert_eq!(trades(&ev), vec![(1, 5, 101, 5), (2, 5, 101, 2)]);
    assert!(h.book.order(5).is_none());
    assert!(!h.book.is_crossed());
}

#[test]
fn amend_of_post_only_that_would_cross_is_rejected_and_leaves_the_order() {
    let mut h = seeded();
    h.new_order(NewOrder::post_only(10, CAROL, Bid, 100, 3));
    let ev = h.amend(10, 101, 3);
    assert_eq!(
        rejection(&ev),
        Some((RequestKind::Amend, RejectReason::PostOnlyWouldCross))
    );
    assert_eq!(h.book.order(10).unwrap().price, 100);
    // A non-crossing amend of a post-only order is fine and it stays post-only.
    h.amend(10, 100, 4);
    assert!(h.book.order(10).unwrap().post_only);
}

#[test]
fn amend_of_unknown_order_or_to_zero_is_rejected() {
    let mut h = seeded();
    assert_eq!(
        rejection(&h.amend(999, 100, 1)),
        Some((RequestKind::Amend, RejectReason::UnknownOrder))
    );
    assert_eq!(
        rejection(&h.amend(1, 101, 0)),
        Some((RequestKind::Amend, RejectReason::ZeroQuantity))
    );
    assert_eq!(h.book.order(1).unwrap().qty, 5);
}

#[test]
fn amend_with_no_change_keeps_priority_and_publishes_nothing() {
    let mut h = seeded();
    let ev = h.amend(1, 101, 5);
    assert_eq!(ev.len(), 1);
    assert!(matches!(
        ev[0],
        Event::Amended {
            kept_priority: true,
            ..
        }
    ));
}

#[test]
fn amend_crossing_into_own_order_triggers_stp() {
    let mut h = seeded();
    // Bob's bid moves up to 101, where Bob's order 2 sits behind Alice's order 1.
    let ev = h.amend(4, 101, 10);
    assert_eq!(trades(&ev), vec![(1, 4, 101, 5)]);
    assert_eq!(cancels(&ev), vec![(2, 5, CancelReason::SelfTrade)]);
    assert_eq!(h.book.order(4).unwrap().qty, 5);
    assert_eq!(h.book.best_bid().unwrap().price, 101);
}

// ---------------------------------------------------------------------------------
// Book updates and config
// ---------------------------------------------------------------------------------

#[test]
fn sweep_publishes_removed_levels_with_zero_qty() {
    let mut h = seeded();
    let ev = h.new_order(NewOrder::market(10, CAROL, Bid, 20));
    let updates: Vec<_> = ev
        .iter()
        .filter_map(|e| match *e {
            Event::BookUpdate { price, qty, .. } => Some((price, qty)),
            _ => None,
        })
        .collect();
    assert_eq!(updates, vec![(101, 0), (102, 0)]);
}

#[test]
fn book_updates_can_be_turned_off() {
    let mut book = OrderBook::new(BookConfig {
        emit_book_updates: false,
        ..BookConfig::default()
    });
    let mut ev = Vec::new();
    book.submit(NewOrder::limit(1, ALICE, Bid, 10, 1), &mut ev);
    assert_eq!(ev.len(), 1);
    assert!(matches!(ev[0], Event::Accepted { .. }));
}

#[test]
fn slots_are_reused_after_heavy_churn() {
    let mut h = Harness::new();
    for round in 0..100u64 {
        for i in 0..50 {
            h.limit(round * 100 + i, ALICE, Bid, 100 - (i as i64 % 7), 1);
        }
        for i in 0..50 {
            h.cancel(round * 100 + i);
        }
    }
    assert!(h.book.is_empty());
    assert_eq!(h.book.level_count(Bid), 0);
}
