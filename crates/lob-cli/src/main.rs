//! `lob`: benchmark the matching engine, run backtests, generate feeds.

use std::fs::File;
use std::io::{BufWriter, Write};
use std::path::PathBuf;
use std::time::Instant;

use anyhow::{Context, Result, bail};
use clap::{Parser, Subcommand, ValueEnum};
use lob::backtest::BacktestResult;
use lob::config::BacktestConfig;
use lob::workload::{self, WorkloadConfig};
use lob::{BookConfig, Event, OrderBook, Placement};

#[derive(Parser)]
#[command(
    name = "lob",
    version,
    about = "Limit order book matching engine and backtester"
)]
struct Cli {
    #[command(subcommand)]
    command: Cmd,
}

#[derive(Subcommand)]
enum Cmd {
    /// Measure matching engine throughput and per-message latency on a mixed
    /// add/cancel/amend/trade workload.
    Bench(BenchArgs),
    /// Run a backtest from a TOML config.
    Backtest(BacktestArgs),
    /// Write a synthetic order-by-order feed to CSV.
    Generate(GenerateArgs),
}

#[derive(clap::Args)]
struct BenchArgs {
    /// Messages in the measured stream.
    #[arg(long, default_value_t = 2_000_000)]
    ops: usize,
    /// Resting orders the workload hovers around.
    #[arg(long, default_value_t = 5_000)]
    depth: usize,
    #[arg(long, default_value_t = 1)]
    seed: u64,
    /// Timed passes over the stream; the best throughput is reported.
    #[arg(long, default_value_t = 5)]
    runs: usize,
    /// Skip generating L2 book update events.
    #[arg(long)]
    no_book_updates: bool,
}

#[derive(Clone, Copy, ValueEnum)]
enum Queue {
    Back,
    Front,
}

#[derive(clap::Args)]
struct BacktestArgs {
    /// TOML config file. Without one, defaults are used (idle strategy).
    #[arg(long, short)]
    config: Option<PathBuf>,
    /// Override the synthetic market seed.
    #[arg(long)]
    seed: Option<u64>,
    /// Override the synthetic market duration in seconds.
    #[arg(long)]
    duration: Option<f64>,
    /// Override both latencies, in microseconds.
    #[arg(long)]
    latency_us: Option<f64>,
    /// Override order entry latency, in microseconds.
    #[arg(long)]
    order_latency_us: Option<f64>,
    /// Override market data latency, in microseconds.
    #[arg(long)]
    md_latency_us: Option<f64>,
    /// Override where the strategy's orders join the queue.
    #[arg(long, value_enum)]
    queue: Option<Queue>,
    /// Print the summary as JSON instead of text.
    #[arg(long)]
    json: bool,
    /// Write sampled PnL to this CSV file.
    #[arg(long)]
    pnl_csv: Option<PathBuf>,
    /// Write the strategy's fills to this CSV file.
    #[arg(long)]
    fills_csv: Option<PathBuf>,
}

#[derive(clap::Args)]
struct GenerateArgs {
    /// Take the [market] section from this config.
    #[arg(long, short)]
    config: Option<PathBuf>,
    #[arg(long)]
    seed: Option<u64>,
    #[arg(long)]
    duration: Option<f64>,
    /// Output CSV path.
    #[arg(long, short)]
    out: PathBuf,
}

fn main() -> Result<()> {
    match Cli::parse().command {
        Cmd::Bench(a) => bench(a),
        Cmd::Backtest(a) => backtest(a),
        Cmd::Generate(a) => generate(a),
    }
}

fn percentile(sorted: &[u64], p: f64) -> u64 {
    let idx = ((sorted.len() as f64 - 1.0) * p).round() as usize;
    sorted[idx]
}

fn bench(a: BenchArgs) -> Result<()> {
    if a.ops == 0 || a.runs == 0 {
        bail!("--ops and --runs must be positive");
    }
    let t = Instant::now();
    let w = workload::generate(&WorkloadConfig {
        ops: a.ops,
        seed: a.seed,
        target_depth: a.depth,
        ..WorkloadConfig::default()
    });
    let n = w.ops.len() as f64;
    println!(
        "workload: {} messages after {} warm-up adds, generated in {:.2?}",
        w.ops.len(),
        w.warmup.len(),
        t.elapsed()
    );
    println!(
        "mix: {:.1}% add, {:.1}% cancel, {:.1}% amend, {:.1}% marketable",
        100.0 * w.mix.adds as f64 / n,
        100.0 * w.mix.cancels as f64 / n,
        100.0 * w.mix.amends as f64 / n,
        100.0 * w.mix.aggressive as f64 / n,
    );

    let cfg = BookConfig {
        emit_book_updates: !a.no_book_updates,
        ..BookConfig::default()
    };
    let fresh = || {
        let mut book = OrderBook::new(cfg);
        let mut ev = Vec::new();
        for c in &w.warmup {
            ev.clear();
            book.process(c, &mut ev);
        }
        book
    };

    // Throughput: whole stream, no per-message timing.
    let mut best = f64::MAX;
    let mut stats = (0usize, 0usize, 0usize);
    for _ in 0..a.runs {
        let mut book = fresh();
        let mut ev = Vec::with_capacity(64);
        let (mut events, mut trades) = (0usize, 0usize);
        let t = Instant::now();
        for c in &w.ops {
            ev.clear();
            book.process(c, &mut ev);
            events += ev.len();
            trades += ev
                .iter()
                .filter(|e| matches!(e, Event::Trade { .. }))
                .count();
        }
        let secs = t.elapsed().as_secs_f64();
        best = best.min(secs);
        stats = (events, trades, book.len());
    }
    println!(
        "throughput: {:.2} M msgs/s (best of {}; {:.1} ns/msg), {} events, {} trades, {} resting at end",
        n / best / 1e6,
        a.runs,
        best * 1e9 / n,
        stats.0,
        stats.1,
        stats.2
    );

    // Latency: time every message individually.
    let mut book = fresh();
    let mut ev = Vec::with_capacity(64);
    let mut lat = Vec::with_capacity(w.ops.len());
    for c in &w.ops {
        ev.clear();
        let t = Instant::now();
        book.process(c, &mut ev);
        lat.push(t.elapsed().as_nanos() as u64);
    }
    // Rough cost of the timer itself, so the numbers can be read honestly.
    let t = Instant::now();
    let mut sink = 0u128;
    for _ in 0..100_000 {
        sink = sink.wrapping_add(Instant::now().elapsed().as_nanos());
    }
    let timer_ns = t.elapsed().as_nanos() as f64 / 100_000.0;
    std::hint::black_box(sink);
    lat.sort_unstable();
    println!(
        "latency per message (ns): p50 {}  p90 {}  p99 {}  p99.9 {}  p99.99 {}  max {}",
        percentile(&lat, 0.50),
        percentile(&lat, 0.90),
        percentile(&lat, 0.99),
        percentile(&lat, 0.999),
        percentile(&lat, 0.9999),
        lat[lat.len() - 1]
    );
    println!(
        "  (includes timer overhead of about {timer_ns:.0} ns per reading pair; clock resolution varies by platform)"
    );
    Ok(())
}

fn load_config(path: &Option<PathBuf>) -> Result<BacktestConfig> {
    match path {
        Some(p) => BacktestConfig::load(p).with_context(|| format!("loading {}", p.display())),
        None => Ok(BacktestConfig::default()),
    }
}

fn backtest(a: BacktestArgs) -> Result<()> {
    let mut cfg = load_config(&a.config)?;
    if let Some(seed) = a.seed {
        cfg.market.seed = seed;
    }
    if let Some(d) = a.duration {
        cfg.market.duration_s = d;
    }
    let us = |x: f64| (x * 1_000.0).round() as u64;
    if let Some(l) = a.latency_us {
        cfg.sim.order_latency_ns = us(l);
        cfg.sim.market_data_latency_ns = us(l);
    }
    if let Some(l) = a.order_latency_us {
        cfg.sim.order_latency_ns = us(l);
    }
    if let Some(l) = a.md_latency_us {
        cfg.sim.market_data_latency_ns = us(l);
    }
    if let Some(q) = a.queue {
        cfg.sim.queue_position = match q {
            Queue::Back => Placement::Back,
            Queue::Front => Placement::Front,
        };
    }

    let t = Instant::now();
    let result = cfg.run()?;
    let elapsed = t.elapsed();

    if a.json {
        println!("{}", serde_json::to_string_pretty(&result.summary)?);
    } else {
        println!("{}", result.summary);
        println!(
            "simulated {} feed messages in {:.2?}",
            result.summary.feed_events, elapsed
        );
    }
    if let Some(p) = &a.pnl_csv {
        write_pnl(p, &result)?;
    }
    if let Some(p) = &a.fills_csv {
        write_fills(p, &result)?;
    }
    Ok(())
}

fn create(path: &PathBuf) -> Result<BufWriter<File>> {
    Ok(BufWriter::new(
        File::create(path).with_context(|| format!("creating {}", path.display()))?,
    ))
}

fn write_pnl(path: &PathBuf, r: &BacktestResult) -> Result<()> {
    let mut w = create(path)?;
    writeln!(w, "ts_ns,pnl,position,mid")?;
    for s in &r.samples {
        let mid = s.mid.map(|m| m.to_string()).unwrap_or_default();
        writeln!(w, "{},{},{},{}", s.ts, s.pnl, s.position, mid)?;
    }
    Ok(w.flush()?)
}

fn write_fills(path: &PathBuf, r: &BacktestResult) -> Result<()> {
    let mut w = create(path)?;
    writeln!(w, "ts_ns,order_id,side,price,qty,liquidity,mid_before")?;
    for f in &r.fills {
        let mid = f.mid_before.map(|m| m.to_string()).unwrap_or_default();
        writeln!(
            w,
            "{},{},{:?},{},{},{:?},{}",
            f.ts, f.order_id, f.side, f.price, f.qty, f.liquidity, mid
        )?;
    }
    Ok(w.flush()?)
}

fn generate(a: GenerateArgs) -> Result<()> {
    let mut cfg = load_config(&a.config)?.market;
    if let Some(seed) = a.seed {
        cfg.seed = seed;
    }
    if let Some(d) = a.duration {
        cfg.duration_s = d;
    }
    let feed = lob::synthetic::generate(&cfg);
    let n = feed.len();
    lob::feed::write_csv(create(&a.out)?, feed)?;
    println!("wrote {n} messages to {}", a.out.display());
    Ok(())
}
