//! Criterion benchmarks for the matching engine.
//!
//! Each benchmark clones a prepared book in the (untimed) setup and times only
//! the operations on it. Run with `cargo bench -p lob`.

use std::hint::black_box;

use criterion::{BatchSize, BenchmarkId, Criterion, Throughput, criterion_group, criterion_main};
use lob::reference::ReferenceBook;
use lob::rng::Rng;
use lob::workload::{self, WorkloadConfig};
use lob::{BookConfig, Command, Event, NewOrder, OrderBook, OrderId, Side, StpMode};

const BATCH: u64 = 1_000;

/// A book with `n` resting orders spread over ~40 levels per side around 100_000.
fn resting_book(n: usize, seed: u64) -> (OrderBook, Vec<OrderId>) {
    let mut rng = Rng::new(seed);
    let mut book = OrderBook::default();
    let mut ev = Vec::new();
    let mut ids = Vec::with_capacity(n);
    for i in 0..n as u64 {
        let side = if i % 2 == 0 { Side::Bid } else { Side::Ask };
        let price = 100_000 - side.sign() * (1 + rng.below(40) as i64);
        let id = i + 1;
        book.submit(
            NewOrder::limit(id, 1 + (i % 64) as u32, side, price, rng.range(1, 10)),
            &mut ev,
        );
        ids.push(id);
    }
    (book, ids)
}

fn add(c: &mut Criterion) {
    let mut g = c.benchmark_group("add");
    g.throughput(Throughput::Elements(BATCH));
    for depth in [1_000usize, 10_000, 100_000] {
        let (book, _) = resting_book(depth, 1);
        let mut rng = Rng::new(2);
        let orders: Vec<NewOrder> = (0..BATCH)
            .map(|i| {
                let side = if rng.chance(0.5) {
                    Side::Bid
                } else {
                    Side::Ask
                };
                let price = 100_000 - side.sign() * (1 + rng.below(40) as i64);
                NewOrder::limit(10_000_000 + i, 99, side, price, rng.range(1, 10))
            })
            .collect();
        g.bench_with_input(
            BenchmarkId::new("passive_limit", depth),
            &orders,
            |b, orders| {
                b.iter_batched_ref(
                    || (book.clone(), Vec::with_capacity(4 * BATCH as usize)),
                    |(book, ev)| {
                        for o in orders {
                            book.submit(*o, ev);
                        }
                        black_box(ev.len())
                    },
                    BatchSize::LargeInput,
                )
            },
        );
    }
    g.finish();
}

fn cancel(c: &mut Criterion) {
    let mut g = c.benchmark_group("cancel");
    g.throughput(Throughput::Elements(BATCH));
    for depth in [1_000usize, 10_000, 100_000] {
        let (book, mut ids) = resting_book(depth, 3);
        // Cancel a random sample, in random order.
        let mut rng = Rng::new(4);
        for i in (1..ids.len()).rev() {
            ids.swap(i, rng.below(i as u64 + 1) as usize);
        }
        ids.truncate(BATCH as usize);
        g.bench_with_input(BenchmarkId::new("random", depth), &ids, |b, ids| {
            b.iter_batched_ref(
                || (book.clone(), Vec::with_capacity(4 * BATCH as usize)),
                |(book, ev)| {
                    for &id in ids {
                        book.cancel(id, ev);
                    }
                    black_box(ev.len())
                },
                BatchSize::LargeInput,
            )
        });
    }
    g.finish();
}

fn amend(c: &mut Criterion) {
    let mut g = c.benchmark_group("amend");
    g.throughput(Throughput::Elements(BATCH));
    let (book, ids) = resting_book(10_000, 5);
    let targets: Vec<_> = ids[..BATCH as usize]
        .iter()
        .map(|&id| book.order(id).unwrap())
        .collect();
    g.bench_function("size_down_keeps_priority", |b| {
        b.iter_batched_ref(
            || (book.clone(), Vec::with_capacity(4 * BATCH as usize)),
            |(book, ev)| {
                for o in &targets {
                    book.process(
                        &Command::Amend {
                            id: o.id,
                            price: o.price,
                            qty: 1,
                        },
                        ev,
                    );
                }
                black_box(ev.len())
            },
            BatchSize::LargeInput,
        )
    });
    g.bench_function("reprice_loses_priority", |b| {
        b.iter_batched_ref(
            || (book.clone(), Vec::with_capacity(4 * BATCH as usize)),
            |(book, ev)| {
                for o in &targets {
                    book.process(
                        &Command::Amend {
                            id: o.id,
                            price: o.price - o.side.sign(),
                            qty: o.qty,
                        },
                        ev,
                    );
                }
                black_box(ev.len())
            },
            BatchSize::LargeInput,
        )
    });
    g.finish();
}

/// 100 levels per side, 5 orders of 10 lots each.
fn ladder_book() -> OrderBook {
    let mut book = OrderBook::default();
    let mut ev = Vec::new();
    let mut id = 0;
    for level in 1..=100 {
        for side in [Side::Bid, Side::Ask] {
            for _ in 0..5 {
                id += 1;
                let price = 100_000 - side.sign() * level;
                book.submit(
                    NewOrder::limit(id, (id % 64) as u32, side, price, 10),
                    &mut ev,
                );
            }
        }
    }
    book
}

fn market_sweep(c: &mut Criterion) {
    let mut g = c.benchmark_group("market_sweep");
    let book = ladder_book();
    for levels in [1u64, 10, 50] {
        let order = NewOrder::market(1_000_000, 999, Side::Bid, levels * 50);
        g.throughput(Throughput::Elements(levels * 5));
        g.bench_with_input(BenchmarkId::new("levels", levels), &order, |b, order| {
            b.iter_batched_ref(
                || (book.clone(), Vec::with_capacity(1024)),
                |(book, ev)| {
                    book.submit(*order, ev);
                    black_box(ev.len())
                },
                BatchSize::LargeInput,
            )
        });
    }
    g.finish();
}

fn mixed(c: &mut Criterion) {
    let mut g = c.benchmark_group("mixed");
    let w = workload::generate(&WorkloadConfig {
        ops: 100_000,
        target_depth: 5_000,
        ..WorkloadConfig::default()
    });
    let mut ev = Vec::new();
    g.throughput(Throughput::Elements(w.ops.len() as u64));
    for (name, updates) in [("with_book_updates", true), ("no_book_updates", false)] {
        let mut book = OrderBook::new(BookConfig {
            emit_book_updates: updates,
            ..BookConfig::default()
        });
        for cmd in &w.warmup {
            book.process(cmd, &mut ev);
        }
        g.bench_function(BenchmarkId::new("workload_100k", name), |b| {
            b.iter_batched_ref(
                || (book.clone(), Vec::with_capacity(64)),
                |(book, ev)| {
                    let mut n = 0;
                    for cmd in &w.ops {
                        ev.clear();
                        book.process(cmd, ev);
                        n += ev.len();
                    }
                    black_box(n)
                },
                BatchSize::LargeInput,
            )
        });
    }
    g.finish();
}

/// The naive reference engine against the real one on a small book, to show
/// what the data structures buy.
fn versus_reference(c: &mut Criterion) {
    let mut g = c.benchmark_group("versus_reference");
    let w = workload::generate(&WorkloadConfig {
        ops: 10_000,
        target_depth: 500,
        ..WorkloadConfig::default()
    });
    let all: Vec<Command> = w.warmup.iter().chain(&w.ops).copied().collect();
    g.throughput(Throughput::Elements(all.len() as u64));
    g.bench_function("engine_depth_500", |b| {
        b.iter(|| {
            let mut book = OrderBook::default();
            let mut ev: Vec<Event> = Vec::with_capacity(64);
            for cmd in &all {
                ev.clear();
                book.process(cmd, &mut ev);
            }
            black_box(book.len())
        })
    });
    g.bench_function("reference_depth_500", |b| {
        b.iter(|| {
            let mut book = ReferenceBook::new(StpMode::CancelResting);
            let mut ev: Vec<Event> = Vec::with_capacity(64);
            for cmd in &all {
                ev.clear();
                book.process(cmd, &mut ev);
            }
            black_box(ev.len())
        })
    });
    g.finish();
}

criterion_group!(
    benches,
    add,
    cancel,
    amend,
    market_sweep,
    mixed,
    versus_reference
);
criterion_main!(benches);
