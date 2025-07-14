//! Timestamped order flow and a plain CSV format for storing it.
//!
//! A feed is an order-by-order (L3) message log: every new order, cancel and
//! amend from the rest of the market, in time order. The backtester replays it
//! into a live matching engine alongside the strategy's own orders.
//!
//! CSV columns: `ts_ns,action,id,owner,side,type,price,qty`
//!
//! ```text
//! 0,new,1,17,bid,limit,9998,4
//! 1500,amend,1,,,,9999,3
//! 2200,cancel,1,,,,,
//! ```

use std::io::{self, BufRead, Write};

use thiserror::Error;

use crate::types::{Command, NewOrder, OrderType, Side};

/// One message from the market, stamped with its exchange arrival time.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct FeedEvent {
    pub ts: u64,
    pub cmd: Command,
}

#[derive(Debug, Error)]
pub enum FeedError {
    #[error(transparent)]
    Io(#[from] io::Error),
    #[error("line {line}: {msg}")]
    Parse { line: usize, msg: String },
}

pub const CSV_HEADER: &str = "ts_ns,action,id,owner,side,type,price,qty";

fn side_str(s: Side) -> &'static str {
    match s {
        Side::Bid => "bid",
        Side::Ask => "ask",
    }
}

fn type_str(t: OrderType) -> &'static str {
    match t {
        OrderType::Limit => "limit",
        OrderType::Market => "market",
        OrderType::Ioc => "ioc",
        OrderType::Fok => "fok",
        OrderType::PostOnly => "post_only",
    }
}

/// Writes a feed as CSV, header included.
pub fn write_csv<W: Write>(
    mut w: W,
    events: impl IntoIterator<Item = FeedEvent>,
) -> io::Result<()> {
    writeln!(w, "{CSV_HEADER}")?;
    for e in events {
        match e.cmd {
            Command::New(o) => writeln!(
                w,
                "{},new,{},{},{},{},{},{}",
                e.ts,
                o.id,
                o.owner,
                side_str(o.side),
                type_str(o.order_type),
                o.price,
                o.qty
            )?,
            Command::Cancel { id } => writeln!(w, "{},cancel,{id},,,,,", e.ts)?,
            Command::Amend { id, price, qty } => {
                writeln!(w, "{},amend,{id},,,,{price},{qty}", e.ts)?
            }
        }
    }
    w.flush()
}

/// Reads a CSV feed. Timestamps must not go backwards.
pub fn read_csv<R: BufRead>(r: R) -> Result<Vec<FeedEvent>, FeedError> {
    let mut out = Vec::new();
    let mut last_ts = 0;
    for (i, line) in r.lines().enumerate() {
        let line = line?;
        let lineno = i + 1;
        let line = line.trim();
        if line.is_empty() || line.starts_with('#') || (i == 0 && line.starts_with("ts_ns")) {
            continue;
        }
        let err = |msg: String| FeedError::Parse { line: lineno, msg };
        let f: Vec<&str> = line.split(',').map(str::trim).collect();
        if f.len() != 8 {
            return Err(err(format!("expected 8 fields, found {}", f.len())));
        }
        fn num<T: std::str::FromStr>(s: &str, what: &str) -> Result<T, String> {
            s.parse().map_err(|_| format!("bad {what}: {s:?}"))
        }
        let ts: u64 = num(f[0], "timestamp").map_err(err)?;
        if ts < last_ts {
            return Err(err(format!("timestamp {ts} is before {last_ts}")));
        }
        last_ts = ts;
        let id = num(f[2], "id").map_err(err)?;
        let cmd = match f[1] {
            "new" => {
                let side = match f[4] {
                    "bid" | "buy" | "b" => Side::Bid,
                    "ask" | "sell" | "s" | "a" => Side::Ask,
                    other => return Err(err(format!("bad side: {other:?}"))),
                };
                let order_type = match f[5] {
                    "limit" => OrderType::Limit,
                    "market" => OrderType::Market,
                    "ioc" => OrderType::Ioc,
                    "fok" => OrderType::Fok,
                    "post_only" => OrderType::PostOnly,
                    other => return Err(err(format!("bad order type: {other:?}"))),
                };
                let price = if order_type == OrderType::Market && f[6].is_empty() {
                    0
                } else {
                    num(f[6], "price").map_err(err)?
                };
                Command::New(NewOrder::new(
                    id,
                    num(f[3], "owner").map_err(err)?,
                    side,
                    order_type,
                    price,
                    num(f[7], "qty").map_err(err)?,
                ))
            }
            "cancel" => Command::Cancel { id },
            "amend" => Command::Amend {
                id,
                price: num(f[6], "price").map_err(err)?,
                qty: num(f[7], "qty").map_err(err)?,
            },
            other => return Err(err(format!("bad action: {other:?}"))),
        };
        out.push(FeedEvent { ts, cmd });
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn csv_round_trip() {
        let events = vec![
            FeedEvent {
                ts: 0,
                cmd: Command::New(NewOrder::limit(1, 3, Side::Bid, -5, 2)),
            },
            FeedEvent {
                ts: 10,
                cmd: Command::New(NewOrder::market(2, 4, Side::Ask, 1)),
            },
            FeedEvent {
                ts: 10,
                cmd: Command::Amend {
                    id: 1,
                    price: -4,
                    qty: 1,
                },
            },
            FeedEvent {
                ts: 20,
                cmd: Command::Cancel { id: 1 },
            },
        ];
        let mut buf = Vec::new();
        write_csv(&mut buf, events.clone()).unwrap();
        assert_eq!(read_csv(buf.as_slice()).unwrap(), events);
    }

    #[test]
    fn rejects_out_of_order_and_malformed_lines() {
        let bad = "ts_ns,action,id,owner,side,type,price,qty\n5,cancel,1,,,,,\n4,cancel,2,,,,,\n";
        assert!(matches!(
            read_csv(bad.as_bytes()),
            Err(FeedError::Parse { line: 3, .. })
        ));
        assert!(read_csv("1,new,1,1,up,limit,5,5\n".as_bytes()).is_err());
        assert!(read_csv("1,new,1\n".as_bytes()).is_err());
    }
}
