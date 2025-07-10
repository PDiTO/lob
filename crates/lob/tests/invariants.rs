//! Property tests: invariants that must hold after every command in any sequence.

mod common;

use std::collections::HashMap;

use lob::{
    BookConfig, CancelReason, Command, DepthView, Event, L2Book, L3Snapshot, OrderBook, OrderId,
    OwnerId, Qty, RestingOrder, Side, StpMode,
};
use proptest::prelude::*;

/// Running totals for the conservation check.
#[derive(Default, Debug)]
struct Ledger {
    submitted: u128,
    filled: u128,
    cancelled: u128,
}

impl Ledger {
    fn record(&mut self, events: &[Event]) {
        for e in events {
            match *e {
                Event::Accepted { qty, .. } => self.submitted += qty as u128,
                Event::Amended {
                    old_qty, new_qty, ..
                } => {
                    if new_qty > old_qty {
                        self.submitted += (new_qty - old_qty) as u128;
                    } else {
                        self.cancelled += (old_qty - new_qty) as u128;
                    }
                }
                // Both the maker and the taker fill `qty`.
                Event::Trade { qty, .. } => self.filled += 2 * qty as u128,
                Event::Cancelled { qty, .. } => self.cancelled += qty as u128,
                Event::Rejected { .. } | Event::BookUpdate { .. } => {}
            }
        }
    }
}

fn resting_qty(snap: &L3Snapshot) -> u128 {
    snap.bids
        .iter()
        .chain(&snap.asks)
        .map(|o| o.qty as u128)
        .sum()
}

/// The side and limit an incoming order (or crossing amend) matches with.
fn aggressor(cmd: &Command, before: &L3Snapshot) -> Option<(OrderId, OwnerId, Side)> {
    match *cmd {
        Command::New(o) => Some((o.id, o.owner, o.side)),
        Command::Amend { id, .. } => before
            .bids
            .iter()
            .chain(&before.asks)
            .find(|o| o.id == id)
            .map(|o| (id, o.owner, o.side)),
        Command::Cancel { .. } => None,
    }
}

/// Checks one command's events against the book before and after it.
fn check_step(
    stp: StpMode,
    cmd: &Command,
    before: &L3Snapshot,
    after: &L3Snapshot,
    events: &[Event],
) -> Result<(), TestCaseError> {
    let Some((taker_id, taker_owner, taker_side)) = aggressor(cmd, before) else {
        prop_assert!(
            !events.iter().any(|e| matches!(e, Event::Trade { .. })),
            "a cancel produced a trade"
        );
        return Ok(());
    };
    let contra: &[RestingOrder] = match taker_side {
        Side::Bid => &before.asks,
        Side::Ask => &before.bids,
    };
    let by_id: HashMap<OrderId, &RestingOrder> = contra.iter().map(|o| (o.id, o)).collect();

    // Everything the aggressor consumed on the other side, in the order it happened:
    // makers it traded with and own orders removed by STP.
    let mut consumed: Vec<OrderId> = Vec::new();
    for e in events {
        match *e {
            Event::Trade {
                maker_id,
                taker_id: t,
                taker_side: ts,
                price,
                qty,
                ..
            } => {
                prop_assert_eq!(t, taker_id);
                prop_assert_eq!(ts, taker_side);
                prop_assert!(qty > 0);
                let maker = by_id.get(&maker_id);
                prop_assert!(
                    maker.is_some(),
                    "maker {} was not resting on the other side",
                    maker_id
                );
                let maker = maker.unwrap();
                prop_assert_eq!(maker.side, taker_side.opposite());
                prop_assert_eq!(price, maker.price, "fill not at the maker's price");
                if consumed.last() != Some(&maker_id) {
                    consumed.push(maker_id);
                }
            }
            Event::Cancelled {
                id,
                reason: CancelReason::SelfTrade,
                ..
            } if id != taker_id => {
                prop_assert!(by_id.contains_key(&id));
                consumed.push(id);
            }
            _ => {}
        }
    }

    // Price-time priority: what was consumed is exactly a prefix of the other side
    // in priority order, and everything but the last one is gone.
    prop_assert!(consumed.len() <= contra.len());
    for (i, id) in consumed.iter().enumerate() {
        prop_assert_eq!(*id, contra[i].id, "consumed out of priority order");
        if i + 1 < consumed.len() {
            let still_there = after.bids.iter().chain(&after.asks).any(|o| o.id == *id);
            prop_assert!(!still_there, "skipped past an order that was not used up");
        }
    }

    // With cancel-incoming STP, a taker cancelled for self-trade must have stopped
    // right in front of one of its own orders.
    let taker_stp = events.iter().any(|e| {
        matches!(e, Event::Cancelled { id, reason: CancelReason::SelfTrade, .. } if *id == taker_id)
    });
    if taker_stp {
        prop_assert_eq!(stp, StpMode::CancelIncoming);
        // Every maker before the stop was used up, so the stop is at the next order.
        let next = consumed.len();
        prop_assert!(next < contra.len());
        prop_assert_eq!(contra[next].owner, taker_owner);
    }
    Ok(())
}

fn run_with_invariants(stp: StpMode, cmds: &[Command]) -> Result<Vec<Event>, TestCaseError> {
    let mut book = OrderBook::new(BookConfig {
        stp,
        ..BookConfig::default()
    });
    let mut l2 = L2Book::new();
    let mut ledger = Ledger::default();
    let mut all = Vec::new();
    let mut events = Vec::new();

    for cmd in cmds {
        let before = book.l3_snapshot();
        events.clear();
        book.process(cmd, &mut events);
        let after = book.l3_snapshot();

        // The book is never crossed once a command has been processed.
        prop_assert!(!book.is_crossed(), "crossed after {:?}", cmd);

        // Quantity is conserved: submitted = resting + filled + cancelled.
        ledger.record(&events);
        prop_assert_eq!(
            ledger.submitted,
            resting_qty(&after) + ledger.filled + ledger.cancelled,
            "quantity leaked on {:?}: {:?}",
            cmd,
            ledger
        );

        // L2 aggregation always equals the sum of L3 orders, both from the book and
        // from the incremental update stream.
        let l2_snap = book.l2_snapshot(usize::MAX);
        prop_assert_eq!(&after.to_l2(), &l2_snap);
        for e in &events {
            l2.apply(e);
        }
        prop_assert_eq!(&l2.l2_snapshot(usize::MAX), &l2_snap);
        prop_assert_eq!(book.len(), after.bids.len() + after.asks.len());

        // Priority, fill prices and STP.
        check_step(stp, cmd, &before, &after, &events)?;

        // Within a level, queue order matches arrival (seq) order.
        for side in [&after.bids, &after.asks] {
            for w in side.windows(2) {
                if w[0].price == w[1].price {
                    prop_assert!(w[0].seq < w[1].seq);
                }
            }
        }
        all.extend_from_slice(&events);
    }
    Ok(all)
}

proptest! {
    #![proptest_config(ProptestConfig::with_cases(512))]

    #[test]
    fn invariants_hold_after_every_command(stp in common::stp_mode(), cmds in common::commands(200)) {
        run_with_invariants(stp, &cmds)?;
    }

    #[test]
    fn replaying_the_same_log_gives_the_same_book(stp in common::stp_mode(), cmds in common::commands(200)) {
        let run = || {
            let mut book = OrderBook::new(BookConfig { stp, ..BookConfig::default() });
            let mut events = Vec::new();
            for c in &cmds {
                book.process(c, &mut events);
            }
            (events, book.l3_snapshot())
        };
        let (e1, s1) = run();
        let (e2, s2) = run();
        prop_assert_eq!(e1, e2);
        prop_assert_eq!(s1, s2);
    }

    #[test]
    fn every_trade_has_maker_and_taker_on_opposite_sides(cmds in common::commands(200)) {
        let mut book = OrderBook::default();
        let mut events = Vec::new();
        let mut sides: HashMap<OrderId, Side> = HashMap::new();
        for c in &cmds {
            events.clear();
            book.process(c, &mut events);
            for e in &events {
                match *e {
                    Event::Accepted { id, side, .. } => { sides.insert(id, side); }
                    Event::Trade { maker_id, taker_id, taker_side, maker_owner, taker_owner, .. } => {
                        prop_assert_ne!(maker_id, taker_id);
                        prop_assert_ne!(maker_owner, taker_owner, "self trade slipped through STP");
                        prop_assert_eq!(sides[&taker_id], taker_side);
                        prop_assert_eq!(sides[&maker_id], taker_side.opposite());
                    }
                    _ => {}
                }
            }
        }
    }
}

/// A longer seeded run than proptest generates, to reach deeper books.
#[test]
fn invariants_hold_over_a_long_random_run() {
    use proptest::strategy::ValueTree;
    use proptest::test_runner::{Config, TestRng, TestRunner};

    let mut runner = TestRunner::new_with_rng(
        Config::default(),
        TestRng::deterministic_rng(proptest::test_runner::RngAlgorithm::ChaCha),
    );
    for stp in [StpMode::CancelResting, StpMode::CancelIncoming] {
        let cmds = prop::collection::vec(common::command(), 5_000)
            .new_tree(&mut runner)
            .unwrap()
            .current();
        run_with_invariants(stp, &cmds).unwrap();
    }
}

#[test]
fn ledger_counts_both_sides_of_a_trade() {
    let mut ledger = Ledger::default();
    ledger.record(&[Event::Trade {
        maker_id: 1,
        maker_owner: 1,
        taker_id: 2,
        taker_owner: 2,
        taker_side: Side::Bid,
        price: 100,
        qty: 3 as Qty,
        maker_remaining: 0,
    }]);
    assert_eq!(ledger.filled, 6);
}
