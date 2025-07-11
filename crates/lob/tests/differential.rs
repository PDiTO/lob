//! Differential test: the real engine against the naive reference engine.
//!
//! Both engines see the same commands. After every command they must have emitted
//! the same events (the reference does not produce `BookUpdate`s, so those are
//! filtered out) and hold the same resting orders in the same priority order.

mod common;

use lob::reference::ReferenceBook;
use lob::{BookConfig, Command, Event, OrderBook, Side, StpMode};
use proptest::prelude::*;

fn without_book_updates(events: &[Event]) -> Vec<Event> {
    events
        .iter()
        .filter(|e| !matches!(e, Event::BookUpdate { .. }))
        .copied()
        .collect()
}

fn compare(stp: StpMode, cmds: &[Command]) -> Result<(), TestCaseError> {
    let mut book = OrderBook::new(BookConfig {
        stp,
        ..BookConfig::default()
    });
    let mut reference = ReferenceBook::new(stp);
    let (mut got, mut want) = (Vec::new(), Vec::new());
    for (step, cmd) in cmds.iter().enumerate() {
        got.clear();
        want.clear();
        book.process(cmd, &mut got);
        reference.process(cmd, &mut want);
        prop_assert_eq!(
            without_book_updates(&got),
            want.clone(),
            "events differ at step {} ({:?})",
            step,
            cmd
        );
        for side in [Side::Bid, Side::Ask] {
            let ours: Vec<_> = book.orders(side).collect();
            prop_assert_eq!(
                ours,
                reference.orders(side),
                "{:?} side differs at step {}",
                side,
                step
            );
        }
    }
    Ok(())
}

proptest! {
    #![proptest_config(ProptestConfig::with_cases(1024))]

    #[test]
    fn engine_matches_reference(stp in common::stp_mode(), cmds in common::commands(300)) {
        compare(stp, &cmds)?;
    }
}

#[test]
fn engine_matches_reference_over_long_runs() {
    use proptest::strategy::ValueTree;
    use proptest::test_runner::{Config, RngAlgorithm, TestRng, TestRunner};

    let mut runner = TestRunner::new_with_rng(
        Config::default(),
        TestRng::deterministic_rng(RngAlgorithm::ChaCha),
    );
    for stp in [StpMode::CancelResting, StpMode::CancelIncoming] {
        for _ in 0..4 {
            let cmds = prop::collection::vec(common::command(), 5_000)
                .new_tree(&mut runner)
                .unwrap()
                .current();
            compare(stp, &cmds).unwrap();
        }
    }
}
