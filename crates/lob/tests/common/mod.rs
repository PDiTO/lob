//! Shared proptest strategies for random command sequences.
//!
//! The id, price and owner ranges are kept small on purpose: small id spaces mean
//! cancels and amends usually hit live orders (and sometimes duplicates or dead
//! ids), a narrow price band means lots of crossing, and a handful of owners means
//! self-trade prevention triggers regularly.

#![allow(dead_code)]

use lob::{Command, NewOrder, OrderType, Side, StpMode};
use proptest::prelude::*;

pub const MAX_ID: u64 = 40;

pub fn side() -> impl Strategy<Value = Side> {
    prop_oneof![Just(Side::Bid), Just(Side::Ask)]
}

pub fn order_type() -> impl Strategy<Value = OrderType> {
    prop_oneof![
        6 => Just(OrderType::Limit),
        1 => Just(OrderType::Market),
        1 => Just(OrderType::Ioc),
        1 => Just(OrderType::Fok),
        2 => Just(OrderType::PostOnly),
    ]
}

pub fn new_order() -> impl Strategy<Value = NewOrder> {
    (
        1..=MAX_ID,
        1u32..=3,
        side(),
        order_type(),
        95i64..=105,
        0u64..=12,
    )
        .prop_map(|(id, owner, side, order_type, price, qty)| {
            NewOrder::new(id, owner, side, order_type, price, qty)
        })
}

pub fn command() -> impl Strategy<Value = Command> {
    prop_oneof![
        6 => new_order().prop_map(Command::New),
        2 => (1..=MAX_ID).prop_map(|id| Command::Cancel { id }),
        2 => (1..=MAX_ID, 95i64..=105, 0u64..=12)
            .prop_map(|(id, price, qty)| Command::Amend { id, price, qty }),
    ]
}

pub fn commands(max_len: usize) -> impl Strategy<Value = Vec<Command>> {
    prop::collection::vec(command(), 1..max_len)
}

pub fn stp_mode() -> impl Strategy<Value = StpMode> {
    prop_oneof![Just(StpMode::CancelResting), Just(StpMode::CancelIncoming)]
}
