"""The order book through the Python bindings."""

from __future__ import annotations

import doctest

import pytest

import lob


def types(events: list[dict]) -> list[str]:
    return [e["type"] for e in events]


def trades(events: list[dict]) -> list[tuple[int, int, int, int]]:
    return [
        (e["maker_id"], e["taker_id"], e["price"], e["qty"]) for e in events if e["type"] == "trade"
    ]


@pytest.fixture
def book() -> lob.OrderBook:
    """Asks 101 x 5 (id 1, owner 1), 101 x 5 (id 2, owner 2), 102 x 10 (id 3, owner 1);
    bid 99 x 5 (id 4, owner 2)."""
    b = lob.OrderBook()
    b.limit(1, "ask", 101, 5, owner=1)
    b.limit(2, "ask", 101, 5, owner=2)
    b.limit(3, "ask", 102, 10, owner=1)
    b.limit(4, "bid", 99, 5, owner=2)
    return b


def test_module_docstring_examples() -> None:
    result = doctest.testmod(lob)
    assert result.attempted > 0
    assert result.failed == 0


def test_resting_limit_order_and_top_of_book(book: lob.OrderBook) -> None:
    assert book.best_bid() == (99, 5)
    assert book.best_ask() == (101, 10)
    assert book.spread() == 2
    assert book.mid() == 100.0
    assert len(book) == 4
    assert "OrderBook(" in repr(book)


def test_crossing_limit_sweeps_levels_at_maker_prices(book: lob.OrderBook) -> None:
    ev = book.limit(10, "bid", 102, 14, owner=3)
    assert trades(ev) == [(1, 10, 101, 5), (2, 10, 101, 5), (3, 10, 102, 4)]
    assert types(ev)[0] == "accepted"
    assert types(ev)[-1] == "book_update"
    assert book.best_ask() == (102, 6)


def test_market_order_cancels_unfilled_remainder(book: lob.OrderBook) -> None:
    ev = book.market(10, "buy", 25, owner=3)
    assert sum(t[3] for t in trades(ev)) == 20
    cancelled = [e for e in ev if e["type"] == "cancelled"]
    assert cancelled == [
        {
            "type": "cancelled",
            "id": 10,
            "owner": 3,
            "side": "bid",
            "price": 0,
            "qty": 5,
            "reason": "unfilled",
        }
    ]
    assert book.best_ask() is None


def test_ioc_never_rests(book: lob.OrderBook) -> None:
    ev = book.ioc(10, "bid", 101, 12, owner=3)
    assert sum(t[3] for t in trades(ev)) == 10
    assert book.order(10) is None


def test_fok_all_or_nothing(book: lob.OrderBook) -> None:
    before = book.l3()
    ev = book.fok(10, "bid", 102, 21, owner=3)
    assert ev == [{"type": "rejected", "id": 10, "request": "new", "reason": "fok_unfillable"}]
    assert book.l3() == before
    ev = book.fok(11, "bid", 102, 20, owner=3)
    assert sum(t[3] for t in trades(ev)) == 20


def test_post_only_rejects_instead_of_crossing(book: lob.OrderBook) -> None:
    ev = book.post_only(10, "bid", 101, 1, owner=3)
    assert ev[0]["reason"] == "post_only_would_cross"
    book.post_only(11, "bid", 100, 1, owner=3)
    assert book.order(11)["post_only"] is True


def test_self_trade_prevention_modes() -> None:
    resting = lob.OrderBook(stp="cancel_resting")
    resting.limit(1, "ask", 101, 5, owner=1)
    resting.limit(2, "ask", 101, 5, owner=2)
    ev = resting.limit(3, "bid", 101, 5, owner=1)
    assert [(e["id"], e["reason"]) for e in ev if e["type"] == "cancelled"] == [(1, "self_trade")]
    assert trades(ev) == [(2, 3, 101, 5)]

    incoming = lob.OrderBook(stp="cancel_incoming")
    incoming.limit(1, "ask", 101, 5, owner=1)
    ev = incoming.limit(3, "bid", 101, 5, owner=1)
    assert [(e["id"], e["reason"]) for e in ev if e["type"] == "cancelled"] == [(3, "self_trade")]
    assert incoming.best_ask() == (101, 5)


def test_cancel_and_unknown_cancel(book: lob.OrderBook) -> None:
    ev = book.cancel(4)
    assert ev[0]["reason"] == "requested"
    assert book.best_bid() is None
    assert book.cancel(4)[0] == {
        "type": "rejected",
        "id": 4,
        "request": "cancel",
        "reason": "unknown_order",
    }


def test_amend_priority_rules(book: lob.OrderBook) -> None:
    ev = book.amend(1, 101, 2)
    assert ev[0]["kept_priority"] is True
    assert [o["id"] for o in book.l3()["asks"]][:2] == [1, 2]

    ev = book.amend(1, 101, 6)
    assert ev[0]["kept_priority"] is False
    assert [o["id"] for o in book.l3()["asks"]][:2] == [2, 1]


def test_l2_matches_l3(book: lob.OrderBook) -> None:
    book.limit(5, "bid", 99, 3, owner=3)
    l2 = book.l2(depth=10)
    l3 = book.l3()
    for side in ("bids", "asks"):
        levels: dict[int, list[int]] = {}
        for o in l3[side]:
            agg = levels.setdefault(o["price"], [0, 0])
            agg[0] += o["qty"]
            agg[1] += 1
        assert [(p, q, n) for p, (q, n) in levels.items()] == l2[side]


def test_microprice_and_depth_weighted_mid() -> None:
    b = lob.OrderBook()
    b.limit(1, "bid", 100, 9, owner=1)
    b.limit(2, "ask", 102, 1, owner=2)
    assert b.microprice() == pytest.approx(101.8)
    assert b.depth_weighted_mid(1) == pytest.approx(101.0)


def test_bad_arguments_raise() -> None:
    b = lob.OrderBook()
    with pytest.raises(ValueError, match="side"):
        b.limit(1, "up", 100, 1)
    with pytest.raises(ValueError, match="order_type"):
        b.submit(1, "bid", 100, 1, order_type="stop")
    with pytest.raises(ValueError, match="stp"):
        lob.OrderBook(stp="nope")
    with pytest.raises(OverflowError):
        b.limit(-1, "bid", 100, 1)
