"""The backtester through the Python bindings, including Python strategies."""

from __future__ import annotations

from collections import deque
from itertools import pairwise
from pathlib import Path
from typing import Any

import pytest

import lob

MS = 1_000_000


def config(**overrides: Any) -> dict[str, Any]:
    cfg: dict[str, Any] = {
        "market": {"seed": 7, "duration_s": 60},
        "sim": {"order_latency_ns": 50_000, "market_data_latency_ns": 50_000},
    }
    cfg.update(overrides)
    return cfg


def test_builtin_strategy_runs_and_reports() -> None:
    r = lob.run_backtest(config(strategy={"type": "market_maker"}))
    s = r["summary"]
    assert s["fills"] > 0
    assert s["filled_qty"] == s["maker_qty"] + s["taker_qty"]
    lhs = s["cash"] + s["final_position"] * s["final_mark"]
    assert lhs == pytest.approx(s["realized_pnl"] + s["unrealized_pnl"] - s["fees"])
    assert len(r["samples"]) > 50
    assert {"ts", "pnl", "position", "mid"} <= set(r["samples"][0])
    assert {"horizon_ns", "lots", "vs_fill_price", "mid_move"} <= set(s["markouts"][0])


def test_same_config_same_result() -> None:
    cfg = config(strategy={"type": "market_maker"})
    assert lob.run_backtest(cfg) == lob.run_backtest(cfg)
    other = config(strategy={"type": "market_maker"}, market={"seed": 8, "duration_s": 60})
    assert lob.run_backtest(other)["summary"] != lob.run_backtest(cfg)["summary"]


def test_toml_path_config() -> None:
    root = Path(__file__).resolve().parents[2]
    r = lob.run_backtest(root / "configs" / "replay.toml")
    assert r["summary"]["feed_events"] == 2832


def test_bad_config_raises() -> None:
    with pytest.raises(ValueError, match="invalid backtest config"):
        lob.run_backtest({"sim": {"latency": 1}})


class Momentum(lob.Strategy):
    """A line-for-line port of the Rust momentum strategy."""

    def __init__(
        self,
        lookback: int = 20,
        threshold: float = 3.0,
        trade_qty: int = 5,
        max_position: int = 20,
        max_slippage: int = 1,
    ) -> None:
        self.lookback = lookback
        self.threshold = threshold
        self.trade_qty = trade_qty
        self.max_position = max_position
        self.max_slippage = max_slippage
        self.mids: deque[float] = deque(maxlen=lookback + 1)
        self.target = 0

    def on_timer(self, ctx: lob.Ctx) -> None:
        bid, ask = ctx.best_bid(), ctx.best_ask()
        if bid is None or ask is None:
            return
        mid = (bid[0] + ask[0]) / 2
        self.mids.append(mid)
        if len(self.mids) <= self.lookback:
            return
        change = mid - self.mids[0]
        if change >= self.threshold:
            self.target = self.max_position
        elif change <= -self.threshold:
            self.target = -self.max_position
        if ctx.open_orders():
            return
        gap = self.target - ctx.position
        if gap == 0:
            return
        qty = min(abs(gap), self.trade_qty)
        if gap > 0:
            ctx.ioc("bid", ask[0] + self.max_slippage, qty)
        else:
            ctx.ioc("ask", bid[0] - self.max_slippage, qty)


def test_python_strategy_matches_the_rust_one() -> None:
    cfg = config(strategy={"type": "momentum"})
    native = lob.run_backtest(cfg)
    in_python = lob.run_backtest(cfg, strategy=Momentum())
    assert native["summary"]["fills"] > 10
    assert in_python == native


class Quoter:
    """A plain object (no base class) that quotes one lot either side of the touch."""

    def __init__(self) -> None:
        self.fills: list[dict[str, Any]] = []
        self.updates: list[dict[str, Any]] = []
        self.trades = 0
        self.started = False

    def on_start(self, ctx: lob.Ctx) -> None:
        self.started = True
        assert ctx.now == 0

    def on_book_update(self, ctx: lob.Ctx) -> None:
        bid, ask = ctx.best_bid(), ctx.best_ask()
        if bid is None or ask is None:
            return
        live = {o["side"] for o in ctx.open_orders()}
        if "bid" not in live and ctx.position < 10:
            ctx.post_only("bid", bid[0], 1)
        if "ask" not in live and ctx.position > -10:
            ctx.post_only("ask", ask[0], 1)

    def on_fill(self, ctx: lob.Ctx, fill: dict[str, Any]) -> None:
        self.fills.append(fill)

    def on_order_update(self, ctx: lob.Ctx, update: dict[str, Any]) -> None:
        self.updates.append(update)

    def on_trade(self, ctx: lob.Ctx, trade: dict[str, Any]) -> None:
        self.trades += 1


def test_python_strategy_callbacks() -> None:
    q = Quoter()
    r = lob.run_backtest(config(), strategy=q)
    s = r["summary"]
    assert q.started
    assert q.trades > 100
    assert len(q.fills) == s["fills"] > 0
    assert all(f["liquidity"] == "maker" for f in q.fills)
    assert {u["type"] for u in q.updates} >= {"accepted"}
    assert (
        sum(f["qty"] if f["side"] == "bid" else -f["qty"] for f in q.fills) == s["final_position"]
    )


def test_latency_is_visible_from_python(tmp_path: Path) -> None:
    feed = tmp_path / "feed.csv"
    feed.write_text(
        "ts_ns,action,id,owner,side,type,price,qty\n"
        "0,new,1,11,ask,limit,101,5\n"
        f"{100 * MS},new,2,12,bid,limit,90,1\n"
    )

    class Taker:
        def __init__(self) -> None:
            self.seen: list[tuple[int, int]] = []

        def on_start(self, ctx: lob.Ctx) -> None:
            ctx.ioc("bid", 101, 2)

        def on_fill(self, ctx: lob.Ctx, fill: dict[str, Any]) -> None:
            self.seen.append((fill["exchange_ts"], ctx.now))

    t = Taker()
    lob.run_backtest(
        {
            "feed_csv": str(feed),
            "sim": {"order_latency_ns": 3 * MS, "market_data_latency_ns": 5 * MS},
        },
        strategy=t,
    )
    # Reaches the exchange at 3 ms, the fill report arrives at 8 ms.
    assert t.seen == [(3 * MS, 8 * MS)]


def test_exceptions_in_callbacks_propagate() -> None:
    class Broken:
        def on_book_update(self, ctx: lob.Ctx) -> None:
            raise KeyError("boom")

    with pytest.raises(KeyError, match="boom"):
        lob.run_backtest(config(), strategy=Broken())


def test_ctx_is_only_valid_inside_its_callback() -> None:
    kept: list[lob.Ctx] = []

    class Keeper:
        def on_book_update(self, ctx: lob.Ctx) -> None:
            if not kept:
                kept.append(ctx)
            ctx.stop()

    lob.run_backtest(config(), strategy=Keeper())
    with pytest.raises(RuntimeError, match="outside the callback"):
        _ = kept[0].position


def test_stop_ends_the_run() -> None:
    class Stopper:
        def on_book_update(self, ctx: lob.Ctx) -> None:
            if ctx.now > 2_000 * MS:
                ctx.stop()

    r = lob.run_backtest(config(), strategy=Stopper())
    assert r["summary"]["duration_s"] < 3


def test_generate_feed() -> None:
    feed = lob.generate_feed(seed=3, duration_s=5)
    assert len(feed) > 100
    assert feed == lob.generate_feed(seed=3, duration_s=5)
    assert {e["action"] for e in feed} == {"new", "cancel"}
    assert all(a["ts"] <= b["ts"] for a, b in pairwise(feed))
    with pytest.raises(ValueError, match="invalid market config"):
        lob.generate_feed(sead=3)
