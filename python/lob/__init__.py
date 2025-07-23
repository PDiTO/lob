"""Limit order book matching engine and event-driven backtester, written in Rust.

>>> import lob
>>> book = lob.OrderBook()
>>> _ = book.limit(1, "ask", 10_001, 5, owner=1)
>>> [e["type"] for e in book.limit(2, "bid", 10_001, 3, owner=2)]
['accepted', 'trade', 'book_update']
>>> book.best_ask()
(10001, 2)
"""

from __future__ import annotations

import json
import os
from typing import Any

from lob._lob import Ctx, OrderBook, __version__, _generate_feed, _run_backtest

__all__ = [
    "Ctx",
    "OrderBook",
    "Strategy",
    "__version__",
    "generate_feed",
    "run_backtest",
]


class Strategy:
    """Optional base class for Python strategies.

    Any object with some of these methods works; missing ones are skipped. Every
    callback gets a :class:`Ctx`, which is only valid during that call.
    """

    def on_start(self, ctx: Ctx) -> None:
        """Called once at time zero, before any market data."""

    def on_book_update(self, ctx: Ctx) -> None:
        """The strategy's (delayed) view of the book changed."""

    def on_fill(self, ctx: Ctx, fill: dict[str, Any]) -> None:
        """One of the strategy's orders was filled."""

    def on_timer(self, ctx: Ctx) -> None:
        """The periodic timer fired (``sim.timer_interval_ns``)."""

    def on_trade(self, ctx: Ctx, trade: dict[str, Any]) -> None:
        """A public trade printed."""

    def on_order_update(self, ctx: Ctx, update: dict[str, Any]) -> None:
        """Ack, reject, cancel or amend confirmation for one of our orders."""


def run_backtest(
    config: dict[str, Any] | str | os.PathLike[str] | None = None,
    strategy: object | None = None,
) -> dict[str, Any]:
    """Runs a backtest.

    ``config`` is either a dict with the same shape as the TOML config files
    (``market``, ``feed_csv``, ``sim``, ``strategy``) or a path to a TOML file.
    With ``strategy=None`` the strategy named in the config runs natively in Rust;
    otherwise ``strategy`` is a Python object whose callbacks the simulator calls.

    Returns ``{"summary": {...}, "samples": [...], "fills": [...]}``.
    """
    if config is None:
        raw = _run_backtest(strategy=strategy)
    elif isinstance(config, dict):
        raw = _run_backtest(config_json=json.dumps(config), strategy=strategy)
    else:
        raw = _run_backtest(config_path=os.fspath(config), strategy=strategy)
    result: dict[str, Any] = json.loads(raw)
    return result


def generate_feed(**market: Any) -> list[dict[str, Any]]:
    """Generates a synthetic order-by-order feed.

    Keyword arguments are the ``[market]`` settings, e.g.
    ``generate_feed(seed=3, duration_s=10, volatility=1.5)``.
    """
    return _generate_feed(json.dumps(market) if market else None)
