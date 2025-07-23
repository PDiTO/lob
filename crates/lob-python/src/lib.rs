//! Python bindings: the order book, the backtester, and strategies written in
//! Python that the Rust simulator calls back into.

use std::path::PathBuf;

use lob::backtest::{self, Ctx, Fill, OpenOrder, OrderUpdate, Pending, PublicTrade, Strategy};
use lob::config::BacktestConfig;
use lob::feed::FeedEvent;
use lob::synthetic::{SyntheticConfig, SyntheticMarket};
use lob::{
    BookConfig, CancelReason, Command, DepthView, Event, LevelInfo, NewOrder, OrderBook, OrderType,
    RejectReason, RequestKind, RestingOrder, Side, StpMode,
};
use pyo3::exceptions::{PyRuntimeError, PyValueError};
use pyo3::prelude::*;
use pyo3::types::{PyDict, PyList};

// ------------------------------------------------------------------------------------
// Conversions
// ------------------------------------------------------------------------------------

fn parse_side(s: &str) -> PyResult<Side> {
    match s.to_ascii_lowercase().as_str() {
        "bid" | "buy" | "b" => Ok(Side::Bid),
        "ask" | "sell" | "s" | "a" => Ok(Side::Ask),
        _ => Err(PyValueError::new_err(format!(
            "side must be 'bid'/'buy' or 'ask'/'sell', got {s:?}"
        ))),
    }
}

fn parse_order_type(s: &str) -> PyResult<OrderType> {
    match s.to_ascii_lowercase().as_str() {
        "limit" => Ok(OrderType::Limit),
        "market" => Ok(OrderType::Market),
        "ioc" => Ok(OrderType::Ioc),
        "fok" => Ok(OrderType::Fok),
        "post_only" => Ok(OrderType::PostOnly),
        _ => Err(PyValueError::new_err(format!(
            "order_type must be one of limit, market, ioc, fok, post_only; got {s:?}"
        ))),
    }
}

fn parse_stp(s: &str) -> PyResult<StpMode> {
    match s {
        "cancel_resting" => Ok(StpMode::CancelResting),
        "cancel_incoming" => Ok(StpMode::CancelIncoming),
        _ => Err(PyValueError::new_err(format!(
            "stp must be 'cancel_resting' or 'cancel_incoming', got {s:?}"
        ))),
    }
}

fn side_str(s: Side) -> &'static str {
    match s {
        Side::Bid => "bid",
        Side::Ask => "ask",
    }
}

fn order_type_str(t: OrderType) -> &'static str {
    match t {
        OrderType::Limit => "limit",
        OrderType::Market => "market",
        OrderType::Ioc => "ioc",
        OrderType::Fok => "fok",
        OrderType::PostOnly => "post_only",
    }
}

fn request_str(r: RequestKind) -> &'static str {
    match r {
        RequestKind::New => "new",
        RequestKind::Cancel => "cancel",
        RequestKind::Amend => "amend",
    }
}

fn reject_str(r: RejectReason) -> &'static str {
    match r {
        RejectReason::DuplicateOrderId => "duplicate_order_id",
        RejectReason::ZeroQuantity => "zero_quantity",
        RejectReason::UnknownOrder => "unknown_order",
        RejectReason::PostOnlyWouldCross => "post_only_would_cross",
        RejectReason::FokUnfillable => "fok_unfillable",
    }
}

fn cancel_str(r: CancelReason) -> &'static str {
    match r {
        CancelReason::Requested => "requested",
        CancelReason::Unfilled => "unfilled",
        CancelReason::SelfTrade => "self_trade",
    }
}

fn pending_str(p: Pending) -> &'static str {
    match p {
        Pending::None => "none",
        Pending::New => "new",
        Pending::Cancel => "cancel",
        Pending::Amend => "amend",
    }
}

/// Builds a dict from `(key, value)` pairs.
macro_rules! dict {
    ($py:expr, { $($k:literal : $v:expr),* $(,)? }) => {{
        let d = PyDict::new($py);
        $( d.set_item($k, $v)?; )*
        d
    }};
}

fn event_dict<'py>(py: Python<'py>, e: &Event) -> PyResult<Bound<'py, PyDict>> {
    Ok(match *e {
        Event::Accepted {
            id,
            owner,
            side,
            order_type,
            price,
            qty,
        } => dict!(py, {
            "type": "accepted", "id": id, "owner": owner, "side": side_str(side),
            "order_type": order_type_str(order_type), "price": price, "qty": qty,
        }),
        Event::Rejected {
            id,
            request,
            reason,
        } => dict!(py, {
            "type": "rejected", "id": id, "request": request_str(request),
            "reason": reject_str(reason),
        }),
        Event::Trade {
            maker_id,
            maker_owner,
            taker_id,
            taker_owner,
            taker_side,
            price,
            qty,
            maker_remaining,
        } => dict!(py, {
            "type": "trade", "maker_id": maker_id, "maker_owner": maker_owner,
            "taker_id": taker_id, "taker_owner": taker_owner,
            "taker_side": side_str(taker_side), "price": price, "qty": qty,
            "maker_remaining": maker_remaining,
        }),
        Event::Cancelled {
            id,
            owner,
            side,
            price,
            qty,
            reason,
        } => dict!(py, {
            "type": "cancelled", "id": id, "owner": owner, "side": side_str(side),
            "price": price, "qty": qty, "reason": cancel_str(reason),
        }),
        Event::Amended {
            id,
            side,
            old_price,
            old_qty,
            new_price,
            new_qty,
            kept_priority,
        } => dict!(py, {
            "type": "amended", "id": id, "side": side_str(side), "old_price": old_price,
            "old_qty": old_qty, "new_price": new_price, "new_qty": new_qty,
            "kept_priority": kept_priority,
        }),
        Event::BookUpdate {
            side,
            price,
            qty,
            orders,
        } => dict!(py, {
            "type": "book_update", "side": side_str(side), "price": price, "qty": qty,
            "orders": orders,
        }),
    })
}

fn events_list<'py>(py: Python<'py>, events: &[Event]) -> PyResult<Bound<'py, PyList>> {
    let list = PyList::empty(py);
    for e in events {
        list.append(event_dict(py, e)?)?;
    }
    Ok(list)
}

fn resting_dict<'py>(py: Python<'py>, o: &RestingOrder) -> PyResult<Bound<'py, PyDict>> {
    Ok(dict!(py, {
        "id": o.id, "owner": o.owner, "side": side_str(o.side), "price": o.price,
        "qty": o.qty, "seq": o.seq, "post_only": o.post_only,
    }))
}

fn levels_list(levels: impl Iterator<Item = LevelInfo>) -> Vec<(i64, u64, u32)> {
    levels.map(|l| (l.price, l.qty, l.orders)).collect()
}

fn l2_dict<'py, B: DepthView>(
    py: Python<'py>,
    book: &B,
    depth: usize,
) -> PyResult<Bound<'py, PyDict>> {
    Ok(dict!(py, {
        "bids": levels_list(book.depth(Side::Bid).take(depth)),
        "asks": levels_list(book.depth(Side::Ask).take(depth)),
    }))
}

fn level_tuple(l: Option<LevelInfo>) -> Option<(i64, u64)> {
    l.map(|l| (l.price, l.qty))
}

// ------------------------------------------------------------------------------------
// OrderBook
// ------------------------------------------------------------------------------------

/// A price-time priority limit order book. Prices are integer ticks and
/// quantities integer lots. Every method that changes the book returns the list
/// of events it produced, as dicts with a ``"type"`` key.
#[pyclass(module = "lob", name = "OrderBook")]
struct PyOrderBook {
    book: OrderBook,
    events: Vec<Event>,
}

impl PyOrderBook {
    fn run<'py>(&mut self, py: Python<'py>, cmd: Command) -> PyResult<Bound<'py, PyList>> {
        self.events.clear();
        self.book.process(&cmd, &mut self.events);
        events_list(py, &self.events)
    }
}

#[pymethods]
impl PyOrderBook {
    #[new]
    #[pyo3(signature = (stp = "cancel_resting", emit_book_updates = true))]
    fn new(stp: &str, emit_book_updates: bool) -> PyResult<Self> {
        Ok(Self {
            book: OrderBook::new(BookConfig {
                stp: parse_stp(stp)?,
                emit_book_updates,
            }),
            events: Vec::new(),
        })
    }

    /// Submits a new order. ``order_type`` is one of limit, market, ioc, fok,
    /// post_only; ``price`` is ignored for market orders.
    #[pyo3(signature = (id, side, price, qty, owner = 0, order_type = "limit"))]
    #[allow(clippy::too_many_arguments)] // mirrors the Python signature
    fn submit<'py>(
        &mut self,
        py: Python<'py>,
        id: u64,
        side: &str,
        price: i64,
        qty: u64,
        owner: u32,
        order_type: &str,
    ) -> PyResult<Bound<'py, PyList>> {
        let o = NewOrder::new(
            id,
            owner,
            parse_side(side)?,
            parse_order_type(order_type)?,
            price,
            qty,
        );
        self.run(py, Command::New(o))
    }

    #[pyo3(signature = (id, side, price, qty, owner = 0))]
    fn limit<'py>(
        &mut self,
        py: Python<'py>,
        id: u64,
        side: &str,
        price: i64,
        qty: u64,
        owner: u32,
    ) -> PyResult<Bound<'py, PyList>> {
        self.submit(py, id, side, price, qty, owner, "limit")
    }

    #[pyo3(signature = (id, side, qty, owner = 0))]
    fn market<'py>(
        &mut self,
        py: Python<'py>,
        id: u64,
        side: &str,
        qty: u64,
        owner: u32,
    ) -> PyResult<Bound<'py, PyList>> {
        self.submit(py, id, side, 0, qty, owner, "market")
    }

    #[pyo3(signature = (id, side, price, qty, owner = 0))]
    fn ioc<'py>(
        &mut self,
        py: Python<'py>,
        id: u64,
        side: &str,
        price: i64,
        qty: u64,
        owner: u32,
    ) -> PyResult<Bound<'py, PyList>> {
        self.submit(py, id, side, price, qty, owner, "ioc")
    }

    #[pyo3(signature = (id, side, price, qty, owner = 0))]
    fn fok<'py>(
        &mut self,
        py: Python<'py>,
        id: u64,
        side: &str,
        price: i64,
        qty: u64,
        owner: u32,
    ) -> PyResult<Bound<'py, PyList>> {
        self.submit(py, id, side, price, qty, owner, "fok")
    }

    #[pyo3(signature = (id, side, price, qty, owner = 0))]
    fn post_only<'py>(
        &mut self,
        py: Python<'py>,
        id: u64,
        side: &str,
        price: i64,
        qty: u64,
        owner: u32,
    ) -> PyResult<Bound<'py, PyList>> {
        self.submit(py, id, side, price, qty, owner, "post_only")
    }

    fn cancel<'py>(&mut self, py: Python<'py>, id: u64) -> PyResult<Bound<'py, PyList>> {
        self.run(py, Command::Cancel { id })
    }

    /// Cancel-replace. ``qty`` is the new open quantity. Size down at the same
    /// price keeps priority; anything else goes to the back of the queue.
    fn amend<'py>(
        &mut self,
        py: Python<'py>,
        id: u64,
        price: i64,
        qty: u64,
    ) -> PyResult<Bound<'py, PyList>> {
        self.run(py, Command::Amend { id, price, qty })
    }

    /// ``(price, qty)`` of the best bid, or None.
    fn best_bid(&self) -> Option<(i64, u64)> {
        level_tuple(self.book.best_bid())
    }

    /// ``(price, qty)`` of the best ask, or None.
    fn best_ask(&self) -> Option<(i64, u64)> {
        level_tuple(self.book.best_ask())
    }

    fn spread(&self) -> Option<i64> {
        self.book.top_of_book().spread()
    }

    fn mid(&self) -> Option<f64> {
        self.book.mid()
    }

    fn microprice(&self) -> Option<f64> {
        self.book.microprice()
    }

    #[pyo3(signature = (levels = 5))]
    fn depth_weighted_mid(&self, levels: usize) -> Option<f64> {
        self.book.depth_weighted_mid(levels)
    }

    /// Aggregated depth: ``{"bids": [(price, qty, orders), ...], "asks": [...]}``,
    /// best level first.
    #[pyo3(signature = (depth = 10))]
    fn l2<'py>(&self, py: Python<'py>, depth: usize) -> PyResult<Bound<'py, PyDict>> {
        l2_dict(py, &self.book, depth)
    }

    /// Every resting order in priority order: ``{"bids": [dict, ...], "asks": [...]}``.
    fn l3<'py>(&self, py: Python<'py>) -> PyResult<Bound<'py, PyDict>> {
        let side = |s| -> PyResult<Bound<'py, PyList>> {
            let list = PyList::empty(py);
            for o in self.book.orders(s) {
                list.append(resting_dict(py, &o)?)?;
            }
            Ok(list)
        };
        Ok(dict!(py, { "bids": side(Side::Bid)?, "asks": side(Side::Ask)? }))
    }

    /// The resting order with this id, or None.
    fn order<'py>(&self, py: Python<'py>, id: u64) -> PyResult<Option<Bound<'py, PyDict>>> {
        self.book
            .order(id)
            .map(|o| resting_dict(py, &o))
            .transpose()
    }

    fn __len__(&self) -> usize {
        self.book.len()
    }

    fn __repr__(&self) -> String {
        let fmt =
            |l: Option<LevelInfo>| l.map_or("-".to_string(), |l| format!("{}x{}", l.qty, l.price));
        format!(
            "OrderBook(bid={}, ask={}, orders={})",
            fmt(self.book.best_bid()),
            fmt(self.book.best_ask()),
            self.book.len()
        )
    }
}

// ------------------------------------------------------------------------------------
// Strategy callbacks
// ------------------------------------------------------------------------------------

/// The strategy's handle on the simulation, passed to every callback. Only valid
/// during the callback it was passed to.
#[pyclass(module = "lob", name = "Ctx")]
struct PyCtx {
    inner: Option<Ctx>,
}

impl PyCtx {
    fn ctx(&self) -> PyResult<&Ctx> {
        self.inner.as_ref().ok_or_else(|| {
            PyRuntimeError::new_err("ctx used outside the callback it was passed to")
        })
    }

    fn ctx_mut(&mut self) -> PyResult<&mut Ctx> {
        self.inner.as_mut().ok_or_else(|| {
            PyRuntimeError::new_err("ctx used outside the callback it was passed to")
        })
    }
}

#[pymethods]
impl PyCtx {
    /// Simulation time in nanoseconds.
    #[getter]
    fn now(&self) -> PyResult<u64> {
        Ok(self.ctx()?.now())
    }

    /// Position from the fills the strategy has been told about.
    #[getter]
    fn position(&self) -> PyResult<i64> {
        Ok(self.ctx()?.position())
    }

    fn best_bid(&self) -> PyResult<Option<(i64, u64)>> {
        Ok(level_tuple(self.ctx()?.book().top_of_book().bid))
    }

    fn best_ask(&self) -> PyResult<Option<(i64, u64)>> {
        Ok(level_tuple(self.ctx()?.book().top_of_book().ask))
    }

    fn mid(&self) -> PyResult<Option<f64>> {
        Ok(self.ctx()?.book().mid())
    }

    fn microprice(&self) -> PyResult<Option<f64>> {
        Ok(self.ctx()?.book().microprice())
    }

    fn spread(&self) -> PyResult<Option<i64>> {
        Ok(self.ctx()?.book().top_of_book().spread())
    }

    /// The strategy's (delayed) view of aggregated depth.
    #[pyo3(signature = (depth = 5))]
    fn l2<'py>(&self, py: Python<'py>, depth: usize) -> PyResult<Bound<'py, PyDict>> {
        l2_dict(py, self.ctx()?.book(), depth)
    }

    /// Orders the strategy believes are open or in flight.
    fn open_orders<'py>(&self, py: Python<'py>) -> PyResult<Bound<'py, PyList>> {
        let list = PyList::empty(py);
        for o in self.ctx()?.open_orders() {
            list.append(open_order_dict(py, o)?)?;
        }
        Ok(list)
    }

    /// Sends an order; returns its id.
    #[pyo3(signature = (side, price, qty, order_type = "limit"))]
    fn submit(&mut self, side: &str, price: i64, qty: u64, order_type: &str) -> PyResult<u64> {
        let (side, ty) = (parse_side(side)?, parse_order_type(order_type)?);
        Ok(self.ctx_mut()?.submit(side, ty, price, qty))
    }

    fn limit(&mut self, side: &str, price: i64, qty: u64) -> PyResult<u64> {
        self.submit(side, price, qty, "limit")
    }

    fn post_only(&mut self, side: &str, price: i64, qty: u64) -> PyResult<u64> {
        self.submit(side, price, qty, "post_only")
    }

    fn ioc(&mut self, side: &str, price: i64, qty: u64) -> PyResult<u64> {
        self.submit(side, price, qty, "ioc")
    }

    fn market(&mut self, side: &str, qty: u64) -> PyResult<u64> {
        self.submit(side, 0, qty, "market")
    }

    /// Requests a cancel. False if the order is unknown or already being cancelled.
    fn cancel(&mut self, id: u64) -> PyResult<bool> {
        Ok(self.ctx_mut()?.cancel(id))
    }

    /// Requests a cancel-replace. False if the order is unknown or being cancelled.
    fn amend(&mut self, id: u64, price: i64, qty: u64) -> PyResult<bool> {
        Ok(self.ctx_mut()?.amend(id, price, qty))
    }

    fn cancel_all(&mut self) -> PyResult<()> {
        self.ctx_mut()?.cancel_all();
        Ok(())
    }

    /// Ends the backtest after this callback.
    fn stop(&mut self) -> PyResult<()> {
        self.ctx_mut()?.stop();
        Ok(())
    }
}

fn open_order_dict<'py>(py: Python<'py>, o: &OpenOrder) -> PyResult<Bound<'py, PyDict>> {
    Ok(dict!(py, {
        "id": o.id, "side": side_str(o.side), "order_type": order_type_str(o.order_type),
        "price": o.price, "qty": o.qty, "pending": pending_str(o.pending),
    }))
}

fn fill_dict<'py>(py: Python<'py>, f: &Fill) -> PyResult<Bound<'py, PyDict>> {
    let liquidity = match f.liquidity {
        backtest::Liquidity::Maker => "maker",
        backtest::Liquidity::Taker => "taker",
    };
    Ok(dict!(py, {
        "order_id": f.order_id, "side": side_str(f.side), "price": f.price, "qty": f.qty,
        "leaves": f.leaves, "liquidity": liquidity, "exchange_ts": f.exchange_ts,
    }))
}

fn trade_dict<'py>(py: Python<'py>, t: &PublicTrade) -> PyResult<Bound<'py, PyDict>> {
    Ok(dict!(py, {
        "exchange_ts": t.exchange_ts, "price": t.price, "qty": t.qty,
        "aggressor": side_str(t.aggressor),
    }))
}

fn update_dict<'py>(py: Python<'py>, u: &OrderUpdate) -> PyResult<Bound<'py, PyDict>> {
    Ok(match *u {
        OrderUpdate::Accepted { id } => dict!(py, { "type": "accepted", "id": id }),
        OrderUpdate::Rejected {
            id,
            request,
            reason,
        } => dict!(py, {
            "type": "rejected", "id": id, "request": request_str(request),
            "reason": reject_str(reason),
        }),
        OrderUpdate::Cancelled { id, qty, reason } => dict!(py, {
            "type": "cancelled", "id": id, "qty": qty, "reason": cancel_str(reason),
        }),
        OrderUpdate::Amended {
            id,
            price,
            qty,
            kept_priority,
        } => dict!(py, {
            "type": "amended", "id": id, "price": price, "qty": qty,
            "kept_priority": kept_priority,
        }),
    })
}

/// Adapts a Python object to the `Strategy` trait. Callback methods are optional;
/// missing ones are skipped. The first exception stops the run and is re-raised
/// once the simulator returns.
struct PyStrategy {
    obj: Py<PyAny>,
    has: [bool; 6],
    error: Option<PyErr>,
}

const METHODS: [&str; 6] = [
    "on_start",
    "on_book_update",
    "on_fill",
    "on_timer",
    "on_trade",
    "on_order_update",
];

impl PyStrategy {
    fn new(obj: &Bound<'_, PyAny>) -> PyResult<Self> {
        let mut has = [false; 6];
        for (h, name) in has.iter_mut().zip(METHODS) {
            *h = obj.hasattr(name)? && obj.getattr(name)?.is_callable();
        }
        Ok(Self {
            obj: obj.clone().unbind(),
            has,
            error: None,
        })
    }

    /// Lends the real `Ctx` to Python for the duration of one call. The context
    /// is moved into the Python object and moved back afterwards, so a strategy
    /// that keeps a reference to it gets an error instead of stale state.
    fn call(
        &mut self,
        ctx: &mut Ctx,
        which: usize,
        arg: impl FnOnce(Python<'_>) -> PyResult<Option<Bound<'_, PyDict>>>,
    ) {
        if !self.has[which] || self.error.is_some() {
            return;
        }
        Python::with_gil(|py| {
            let result = (|| -> PyResult<()> {
                let pyctx = Bound::new(
                    py,
                    PyCtx {
                        inner: Some(std::mem::take(ctx)),
                    },
                )?;
                let arg = arg(py);
                let out = match arg {
                    Ok(Some(a)) => self.obj.bind(py).call_method1(METHODS[which], (&pyctx, a)),
                    Ok(None) => self.obj.bind(py).call_method1(METHODS[which], (&pyctx,)),
                    Err(e) => Err(e),
                };
                *ctx = pyctx
                    .borrow_mut()
                    .inner
                    .take()
                    .expect("ctx is only taken here");
                out.map(|_| ())
            })();
            if let Err(e) = result {
                self.error = Some(e);
                ctx.stop();
            }
        });
    }
}

impl Strategy for PyStrategy {
    fn on_start(&mut self, ctx: &mut Ctx) {
        self.call(ctx, 0, |_| Ok(None));
    }

    fn on_book_update(&mut self, ctx: &mut Ctx) {
        self.call(ctx, 1, |_| Ok(None));
    }

    fn on_fill(&mut self, ctx: &mut Ctx, fill: &Fill) {
        self.call(ctx, 2, |py| fill_dict(py, fill).map(Some));
    }

    fn on_timer(&mut self, ctx: &mut Ctx) {
        self.call(ctx, 3, |_| Ok(None));
    }

    fn on_trade(&mut self, ctx: &mut Ctx, trade: &PublicTrade) {
        self.call(ctx, 4, |py| trade_dict(py, trade).map(Some));
    }

    fn on_order_update(&mut self, ctx: &mut Ctx, update: &OrderUpdate) {
        self.call(ctx, 5, |py| update_dict(py, update).map(Some));
    }
}

// ------------------------------------------------------------------------------------
// Module functions
// ------------------------------------------------------------------------------------

fn load_config(
    config_json: Option<&str>,
    config_path: Option<PathBuf>,
) -> PyResult<BacktestConfig> {
    match (config_json, config_path) {
        (Some(_), Some(_)) => Err(PyValueError::new_err(
            "pass a config dict or a path, not both",
        )),
        (Some(json), None) => serde_json::from_str(json)
            .map_err(|e| PyValueError::new_err(format!("invalid backtest config: {e}"))),
        (None, Some(path)) => {
            BacktestConfig::load(&path).map_err(|e| PyValueError::new_err(e.to_string()))
        }
        (None, None) => Ok(BacktestConfig::default()),
    }
}

/// Runs a backtest and returns the result as a JSON string. Use
/// ``lob.run_backtest``, which wraps this.
#[pyfunction]
#[pyo3(signature = (config_json = None, config_path = None, strategy = None))]
fn _run_backtest(
    py: Python<'_>,
    config_json: Option<&str>,
    config_path: Option<PathBuf>,
    strategy: Option<&Bound<'_, PyAny>>,
) -> PyResult<String> {
    let cfg = load_config(config_json, config_path)?;
    let result = match strategy {
        None => py
            .allow_threads(|| cfg.run())
            .map_err(|e| PyValueError::new_err(e.to_string()))?,
        Some(obj) => {
            let mut adapter = PyStrategy::new(obj)?;
            let result = cfg
                .run_with(&mut adapter)
                .map_err(|e| PyValueError::new_err(e.to_string()))?;
            if let Some(err) = adapter.error.take() {
                return Err(err);
            }
            result
        }
    };
    serde_json::to_string(&result).map_err(|e| PyRuntimeError::new_err(e.to_string()))
}

fn feed_dict<'py>(py: Python<'py>, e: &FeedEvent) -> PyResult<Bound<'py, PyDict>> {
    Ok(match e.cmd {
        Command::New(o) => dict!(py, {
            "ts": e.ts, "action": "new", "id": o.id, "owner": o.owner,
            "side": side_str(o.side), "order_type": order_type_str(o.order_type),
            "price": o.price, "qty": o.qty,
        }),
        Command::Cancel { id } => dict!(py, { "ts": e.ts, "action": "cancel", "id": id }),
        Command::Amend { id, price, qty } => dict!(py, {
            "ts": e.ts, "action": "amend", "id": id, "price": price, "qty": qty,
        }),
    })
}

/// Generates a synthetic order-by-order feed. Takes the ``[market]`` settings as
/// a JSON string; use ``lob.generate_feed``, which wraps this.
#[pyfunction]
#[pyo3(signature = (market_json = None))]
fn _generate_feed<'py>(py: Python<'py>, market_json: Option<&str>) -> PyResult<Bound<'py, PyList>> {
    let cfg: SyntheticConfig = match market_json {
        Some(j) => serde_json::from_str(j)
            .map_err(|e| PyValueError::new_err(format!("invalid market config: {e}")))?,
        None => SyntheticConfig::default(),
    };
    let feed: Vec<FeedEvent> = py.allow_threads(|| SyntheticMarket::new(cfg).collect());
    let list = PyList::empty(py);
    for e in &feed {
        list.append(feed_dict(py, e)?)?;
    }
    Ok(list)
}

#[pymodule]
fn _lob(m: &Bound<'_, PyModule>) -> PyResult<()> {
    m.add("__version__", env!("CARGO_PKG_VERSION"))?;
    m.add_class::<PyOrderBook>()?;
    m.add_class::<PyCtx>()?;
    m.add_function(wrap_pyfunction!(_run_backtest, m)?)?;
    m.add_function(wrap_pyfunction!(_generate_feed, m)?)?;
    Ok(())
}
