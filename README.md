<p align="center">
  <img src="docs/logo.png" alt="lob logo" width="200">
</p>

# lob

A limit order book matching engine in Rust, an event-driven backtester built on top of it, and Python bindings for both.

I've spent a lot of my career around trading systems and market making, and I wanted a small, clean, well-tested matching engine and simulator of my own to try market-making ideas against. I also wanted to put numbers on something that usually gets hand-waved in backtests: how much of a passive strategy's result comes from where it sits in the queue, and how much latency takes away. This repo is that tool. It is deliberately a single-instrument, single-venue engine with no network layer. The matching and the simulation are the point.

What's here:

- A price-time priority book in integer ticks and lots. It handles limit, market, IOC, FOK and post-only orders, cancel and cancel-replace, and self-trade prevention. Every command returns an event stream covering acks, rejects with reasons, trades, cancels and L2 level updates.
- Market data views. L2 snapshots, an L2 book rebuilt from incremental updates, the L3 order-by-order view, top of book, microprice and depth-weighted mid.
- A deterministic backtester. It replays an order-by-order feed into the real matching engine alongside the strategy's orders, with separate order-entry and market-data latency, and it tracks fees, PnL, drawdown, Sharpe and markouts.
- A synthetic market generator, so everything runs offline.
- Two example strategies, a market maker with inventory skew and a momentum taker.
- Python bindings via PyO3. You get the book, the backtester, and strategies written in Python that the Rust simulator calls back into.
- A `lob` CLI with `bench`, `backtest` and `generate`.

## Quickstart

### Rust

```sh
cargo test                                   # unit, property, differential, backtester, CLI
cargo bench -p lob                           # criterion benchmarks
```

```rust
use lob::{Event, NewOrder, OrderBook, Side};

let mut book = OrderBook::default();
let mut events = Vec::new();
book.submit(NewOrder::limit(1, 7, Side::Ask, 10_001, 5), &mut events);
book.submit(NewOrder::limit(2, 8, Side::Bid, 10_001, 3), &mut events);
// events: Accepted, BookUpdate, Accepted, Trade { maker_id: 1, taker_id: 2, price: 10_001, qty: 3, .. }, BookUpdate
assert_eq!(book.best_ask().unwrap().qty, 2);
```

A strategy implements `Strategy`. It sees the world through `Ctx`, which already lags the exchange by the market-data latency:

```rust
use lob::backtest::{Ctx, Fill, Strategy};
use lob::{DepthView, Side};

struct JoinTheTouch;

impl Strategy for JoinTheTouch {
    fn on_book_update(&mut self, ctx: &mut Ctx) {
        let Some((bid, _ask)) = ctx.book().top_of_book().two_sided() else { return };
        if ctx.open_orders().next().is_none() {
            ctx.post_only(Side::Bid, bid.price, 1);
        }
    }
    fn on_fill(&mut self, _ctx: &mut Ctx, _fill: &Fill) {}
}
```

### CLI

```sh
cargo build --release
./target/release/lob bench
./target/release/lob backtest --config configs/market_maker.toml
./target/release/lob backtest --config configs/market_maker.toml --queue front --latency-us 10000
./target/release/lob backtest --config configs/momentum.toml --seed 3 --json
./target/release/lob generate --seed 42 --duration 60 --out feed.csv
python3 scripts/sweep.py configs/market_maker.toml --seeds 5   # the table further down
```

`configs/replay.toml` replays `data/sample_feed.csv`, a 20-second recorded feed, to show the CSV path. Any L3 message log with the columns `ts_ns,action,id,owner,side,type,price,qty` works.

### Python

```sh
uv sync
uv run maturin develop --uv --release
uv run pytest
```

```python
import lob

book = lob.OrderBook()
book.limit(1, "ask", 10_001, 5, owner=1)
events = book.limit(2, "bid", 10_001, 3, owner=2)  # list of dicts
book.l2(depth=5)  # {"bids": [(price, qty, orders)], "asks": [...]}


class Quoter(lob.Strategy):
    def on_book_update(self, ctx):
        bid, ask = ctx.best_bid(), ctx.best_ask()
        if bid and ask and not ctx.open_orders():
            ctx.post_only("bid", bid[0], 1)
            ctx.post_only("ask", ask[0], 1)


result = lob.run_backtest(
    {"market": {"seed": 1, "duration_s": 600}, "sim": {"order_latency_ns": 50_000}},
    strategy=Quoter(),
)
result["summary"]["net_pnl"], len(result["fills"])
```

`run_backtest` also takes a path to a TOML config, and with no `strategy` it runs the built-in one named in the config natively. Results come back as plain lists and dicts; there is no numpy dependency, though `numpy.asarray` or `pandas.DataFrame` on `result["samples"]` works fine. Type stubs ship in `python/lob/_lob.pyi`.

## How the book is built

Prices are `i64` ticks and quantities are `u64` lots. There are no floats anywhere in the matching path, so the same commands always give bit-identical books.

**Orders live in a slab.** A `Vec<OrderNode>` with a free list, addressed by `u32` index. Each node carries `prev` and `next` indices, so every price level is an intrusive doubly linked FIFO queue threaded through the slab. There is no allocation per order, and freed slots get reused.

**Levels live in a second slab.** Each level holds its price, aggregate quantity, order count, and the head and tail of its queue.

**Each side is a sorted array of `(price, level index)` pairs with the best price at the end.** I picked this over a `BTreeMap<Price, Level>` because nearly all the action in a real book is within a few ticks of the touch. Reading or removing the best level is `last()` or `pop()`. A new level near the touch shifts only the few entries on the better side of it. Finding any level is a binary search over a contiguous array of 16-byte entries, and the search checks the best level before anything else because most new orders join or improve the touch. The cost shows up when a new level appears far from the touch in a deep book. That shifts most of the array, which is a memmove of a few KB for a few hundred levels. Asks are keyed by `!price` so both sides share the same ascending layout without the overflow edge case of negating `i64::MIN`.

**An `FxHashMap<OrderId, u32>` maps order ids to slab slots.** Cancel is a hash lookup and an O(1) unlink. If that empties the level, it also removes the level from the ladder, which is a `pop` at the touch and a binary search plus a short shift elsewhere.

**Events go into a caller-supplied `Vec<Event>`.** `Event` is `Copy`, so a steady-state caller that clears and reuses the vector allocates nothing. Level updates are collected while a command runs and emitted once at the end, one per touched level, so consumers see the net change per command rather than every intermediate state.

Order ids belong to the caller. The book rejects an id only if a live order already has it, so ids can be reused once an order is done.

## Order types and rules

| Type | On entry | Unfilled remainder |
|---|---|---|
| `Limit` | matches against the other side up to its price | rests at its price |
| `Market` | matches at any price | cancelled, reason `Unfilled` |
| `Ioc` | matches up to its price | cancelled, reason `Unfilled` |
| `Fok` | fills completely up to its price, or nothing happens | rejected with `FokUnfillable`, book untouched |
| `PostOnly` | never matches | rests; rejected with `PostOnlyWouldCross` if it would have crossed |

Fills are always at the resting order's price.

**Post-only rejects rather than slides.** A slide silently changes the price you asked for, and in a backtest I'd rather see the reject and decide what to do about it.

**Cancel-replace** takes a new price and a new open quantity. Reducing size at the same price keeps queue priority. A price change or a size increase sends the order to the back of the queue at the new price, and if the new price crosses, it matches like a new order. A post-only order stays post-only through amends, and an amend that would make it cross is rejected, leaving the original order as it was. Cancelling or amending an unknown or finished order is rejected with `UnknownOrder`.

**Self-trade prevention** applies when an incoming order meets a resting order with the same owner id. There are two modes:

| Mode | What happens |
|---|---|
| `CancelResting` (default) | the resting order is cancelled with reason `SelfTrade` and matching continues |
| `CancelIncoming` | matching stops and the rest of the incoming order is cancelled; fills already done against others stand |

I made cancel-resting the default because it is what a market maker usually wants. The hedge goes through and the stale quote comes out. FOK's all-or-nothing check honours STP, so under cancel-resting your own orders don't count as liquidity, and under cancel-incoming only the liquidity ahead of your first own order counts.

## What the tests enforce

`cargo test` runs 96 Rust tests, counting the README examples as doctests, and `uv run pytest` runs 24 Python tests. They all pass locally, and CI runs the same commands.

**Order types.** 37 unit tests cover each order type and the edge cases: partial fills across levels, FIFO within a level, market orders into an empty side, FOK that can't fill or is limited by price or by STP, post-only that would cross, both STP modes, cancel of unknown, cancelled and filled ids, every amend priority rule, amends that cross, negative prices, and slot reuse under churn.

**Invariants, with proptest.** Random sequences of up to 200 new orders, cancels and amends across every order type and both STP modes, on a narrow price band with a small id space and a few owners, so crossing, STP, duplicates and dead ids all come up often. After every single command:

- the book is not crossed;
- quantity is conserved, so submitted equals resting plus filled plus cancelled, with amends counted as adds or cancels of the difference;
- the aggressor consumed the other side in exact price-time order. The makers it traded with and any own orders STP removed form a prefix of the pre-command L3 queue in priority order, and every order in that prefix but the last is gone;
- each fill is at the maker's price, with maker and taker on opposite sides and different owners;
- under cancel-incoming STP, a taker stopped for self-trade stopped right in front of its own order;
- L2 equals the sum of L3 orders, both from the book directly and from an `L2Book` rebuilt only from the `BookUpdate` stream;
- queue order within a level matches arrival sequence.

Replaying the same log gives identical events and an identical book. A seeded 5,000-command run per STP mode covers deeper books than proptest's cases reach.

**Differential testing.** `lob::reference` is a deliberately naive engine. It keeps every order in one `Vec`, finds the next match with a linear scan, and checks FOK by matching on a clone of the whole book. The real engine must produce exactly the same events, `BookUpdate` aside, and the same queues after every command. That runs over 1,024 proptest cases of up to 300 commands and eight seeded runs of 5,000. I mutation-tested this suite by hand. Putting some orders at the front of the queue, skipping the STP stop in the FOK check, or letting a size-up amend keep priority each broke it immediately.

**Backtester.** 17 tests, including:

- order latency delays arrival at the exchange, and market-data latency delays what the strategy sees, fills included;
- a slow order misses liquidity that a fast one gets, and the feed wins ties at the same nanosecond;
- a passive order doesn't fill until the queue ahead of it is used up, with exact fill times and sizes asserted. Cancels ahead of it move it up, and an order that arrives later queues behind orders that got there first;
- `cash + inventory * mark == realized + unrealized - fees` holds, and cash, position and fees rebuilt independently from the fill log agree;
- the strategy's delayed L2 view equals the real book at every point it is updated when latency is zero;
- a seeded run is reproducible, and saving the feed to CSV and replaying it gives an identical result.

**Python.** Book behaviour through the bindings, config handling, error propagation from Python callbacks, and a Python port of the momentum strategy that has to produce exactly the same backtest result as the Rust one.

## The backtester

The feed is an order-by-order message log from the rest of the market, either generated or read from CSV. The simulator replays it into a real `OrderBook`, and the strategy's orders go into the same book. So a passive order joins the back of its level behind whatever was already there, it moves up as orders ahead of it are cancelled or filled, and it only trades when the flow reaches it. Queue position isn't estimated from L2. It falls out of the matching.

There are two latency legs:

- **Order entry.** New orders, cancels and amends reach the exchange `order_latency_ns` after the strategy sends them.
- **Market data.** Book updates, public trades, and the strategy's own acks and fills reach it `market_data_latency_ns` after the exchange produced them. The strategy's book is an `L2Book` rebuilt from those delayed updates, and its idea of its own position and open orders comes only from messages that have arrived.

The strategy trades as owner id 0 with order ids from 2^60 up. Feed orders that use owner 0 are given another owner so they can't be mistaken for the strategy's.

Everything is one time-ordered event queue. Ties go to the feed first, then everything else in the order it was scheduled, so a strategy order that lands at the same nanosecond as a feed order loses the race. The run ends at the last feed message.

Accounting is exact integer arithmetic in micro-ticks, using average cost for realized PnL, with maker and taker fees per lot; a negative fee is a rebate. The summary covers net, realized and unrealized PnL, fees, fill ratio, maker and taker volume, max inventory, Sharpe of per-interval PnL changes, max drawdown, and markouts at configurable horizons. A markout reports both what each fill was worth h later against the mid and how far the mid moved after it. That second number is the adverse selection.

The synthetic market is Poisson order flow around a latent fair value that follows a random walk or an OU process. Limit orders land at an exponentially distributed distance from fair value, each resting order is cancelled at a fixed rate, and half of the market orders are informed. They trade toward fair value. When fair value drifts past resting orders, new limit orders cross them, and that is how the price moves. It runs its own copy of the matching engine so cancels always name live orders, and it uses a small in-tree xoshiro PRNG so a seed means the same feed regardless of dependency versions.

### Limitations

Simulated fills are an approximation, and this one has the usual holes:

- The feed doesn't react to the strategy. A real participant would cancel or reprice after our order took their liquidity or joined their level; here the feed carries on as recorded. Feed cancels for orders we already traded with are just rejected and counted.
- Latency is a constant per leg, not a distribution, and private and public messages share one latency.
- One instrument, one venue. No hidden or iceberg orders, auctions, or trading halts.
- The strategy only sees L2, not L3, so it can't estimate its own queue position the way a real system watching an L3 feed could.
- Markouts whose horizon runs past the end of the feed are dropped.
- The synthetic market is simple. Its informed flow pushes the mid toward fair value with a lag, which gives a momentum strategy something to find that may not exist in a real market.

## Example results

All from `configs/`, 600 simulated seconds per run. In this market the spread is 1 tick about 63% of the time and 2 ticks for most of the rest. The maker rebate is 0.1 ticks per lot and the taker fee 0.3. PnL is in ticks times lots.

**Market maker, one run** with `lob backtest --config configs/market_maker.toml` at 50 µs each way:

```text
filled                      4805 lots in 1393 fills (maker 4805, taker 0)
position                       3 final, 14 max abs
net pnl                   1525.0 ticks x lots (0.317 per lot)
sharpe                    0.4203 per 1s interval
max drawdown                47.0
markout   1.000s           +0.343 vs fill, -0.363 mid move (4805 lots)
```

**Market maker, queue position and latency**, averaged over seeds 1 to 5 with `scripts/sweep.py`. "Front" puts the strategy's resting orders at the head of their level. No venue does that; it is there as an upper bound.

| queue | latency each way | net PnL, mean ± sd | lots filled | PnL per lot | 1s mid move per lot | Sharpe per 1s |
|---|---|---|---|---|---|---|
| back | 0 µs | 1,831 ± 303 | 5,153 | +0.355 | -0.423 | +0.497 |
| back | 100 µs | 1,833 ± 302 | 5,151 | +0.356 | -0.423 | +0.498 |
| back | 1 ms | 1,810 ± 296 | 5,159 | +0.351 | -0.415 | +0.485 |
| back | 10 ms | 1,501 ± 219 | 5,079 | +0.295 | -0.434 | +0.396 |
| back | 100 ms | -63 ± 413 | 4,344 | -0.015 | -0.571 | -0.001 |
| front | 0 µs | 6,630 ± 741 | 14,882 | +0.446 | -0.266 | +0.723 |
| front | 100 µs | 6,630 ± 746 | 14,861 | +0.446 | -0.268 | +0.721 |
| front | 1 ms | 6,498 ± 793 | 14,764 | +0.440 | -0.265 | +0.707 |
| front | 10 ms | 6,083 ± 718 | 14,049 | +0.433 | -0.265 | +0.674 |
| front | 100 ms | 3,084 ± 466 | 9,451 | +0.326 | -0.315 | +0.346 |

This is the result I built the thing to see. Putting the strategy at the head of the queue roughly triples its volume and multiplies PnL by about 3.6. It also makes each fill better. The mid moves against a back-of-queue fill by 0.42 ticks within a second, against 0.27 at the front. At the back of the queue you mostly get filled when the level is being swept, which is exactly when the price is about to go through you. A backtest that fills passive orders whenever the price touches them is making the front-of-queue assumption without saying so.

Latency barely matters below a millisecond here and wipes out the edge by 100 ms. That scale comes from the generator: fair value moves 2 ticks per square-root second, so about 0.6 ticks in 100 ms, which is more than the half spread. Don't read the absolute PnL as meaningful. This market is kind to a market maker, since half the market orders are noise and there is a rebate on every fill. The comparisons between rows are what I trust.

**Momentum taker.** Seed 1 made 1,276 on 1,580 lots. Over seeds 1 to 5 at 50 µs the mean is 83 ± 673, which is zero. The signal is real in this generator, with the mid moving 0.56 ticks per lot in the trade's direction within a second on average, but crossing the spread and paying 0.3 per lot eats it. At 100 ms the mean drops to -477 ± 884.

## Benchmarks

Measured on an Apple Silicon laptop, an M4 Pro, with `cargo bench -p lob` in criterion and `lob bench`, release build. Each criterion case clones a prepared book in untimed setup and times only the operations. The numbers below are criterion's point estimates; the confidence intervals were within a few percent.

| Benchmark | Time | Per operation |
|---|---|---|
| add 1,000 passive limits, 1k resting | 29.7 µs | 30 ns |
| add 1,000 passive limits, 10k resting | 29.1 µs | 29 ns |
| add 1,000 passive limits, 100k resting | 27.7 µs | 28 ns |
| cancel 1,000 random orders, 1k resting | 23.4 µs | 23 ns |
| cancel 1,000 random orders, 10k resting | 22.6 µs | 23 ns |
| cancel 1,000 random orders, 100k resting | 36.2 µs | 36 ns |
| amend 1,000, size down, keeps priority | 15.9 µs | 16 ns |
| amend 1,000, reprice, loses priority | 51.3 µs | 51 ns |
| market order sweeping 1 level, 5 fills | 119 ns | |
| market order sweeping 10 levels, 50 fills | 744 ns | |
| market order sweeping 50 levels, 250 fills | 2.91 µs | |
| mixed workload, 100k messages, with L2 updates | 3.00 ms | 30 ns, 33.4 M msgs/s |
| mixed workload, 100k messages, no L2 updates | 2.44 ms | 24 ns, 41.0 M msgs/s |
| naive reference engine, 10.5k messages, ~500 resting | 5.78 ms | 550 ns |
| this engine, same stream | 221 µs | 21 ns |

The mixed workload is about 46% adds at or behind the touch, 38% cancels, 8% amends and 8% marketable IOC or market orders, hovering around 5,000 resting orders. `lob bench` runs 2 million messages of it:

```text
throughput: 28.28 M msgs/s (best of 5; 35.4 ns/msg), 4376621 events, 282113 trades, 4999 resting at end
latency per message (ns): p50 41  p90 42  p99 84  p99.9 167  p99.99 417  max 36167
```

With `--no-book-updates` it does 34.0 M msgs/s, or 29.4 ns per message. Read the latency line with care. The clock on this machine ticks every 41.7 ns, so p50 and p90 are one tick, p99 is two, and every reading includes the roughly 36 ns cost of taking the timestamps. The tail percentiles are the useful part. `lob bench` comes out a little slower than criterion's mixed case, partly because its loop also tallies events and trades for every message.

Backtests are quick. The 600-second market-maker run above is 90,382 feed messages and simulates in about 30 ms. With a Python strategy, each callback costs about a microsecond. A Python strategy that reacts to every book update took 95 ms on the same feed.

## Layout

```text
crates/lob/              the library
  src/book/              matching engine: slab, intrusive queues, price ladder
  src/market_data.rs     L2/L3 snapshots, L2Book, microprice, depth-weighted mid
  src/reference.rs       naive reference engine for differential tests
  src/backtest/          simulator, accounting, metrics, Strategy and Ctx
  src/strategies/        market maker and momentum taker
  src/synthetic.rs       synthetic market generator
  src/feed.rs            feed events and the CSV format
  src/workload.rs        benchmark message stream
  tests/                 order types, invariants, differential, backtester
  benches/book.rs        criterion benchmarks
crates/lob-cli/          the lob binary
crates/lob-python/       PyO3 bindings
python/lob/              Python package and type stubs
python/tests/            pytest suite
configs/                 example backtest configs
data/sample_feed.csv     20 seconds of recorded synthetic flow
scripts/sweep.py         seeds x latency x queue position table
```

## License

MIT. See [LICENSE](LICENSE).
