"""Runs a backtest config across seeds, latencies and queue positions.

Prints a markdown table of averages across seeds. Needs a release build of the
CLI (cargo build --release).

    python3 scripts/sweep.py configs/market_maker.toml --seeds 5
"""

from __future__ import annotations

import argparse
import json
import statistics
import subprocess
from pathlib import Path

ROOT = Path(__file__).resolve().parents[1]
LOB = ROOT / "target" / "release" / "lob"


def run(config: str, seed: int, latency_us: float, queue: str) -> dict:
    out = subprocess.run(
        [
            str(LOB),
            "backtest",
            "--json",
            "--config",
            config,
            "--seed",
            str(seed),
            "--latency-us",
            str(latency_us),
            "--queue",
            queue,
        ],
        check=True,
        capture_output=True,
        text=True,
    ).stdout
    return json.loads(out)


def main() -> None:
    p = argparse.ArgumentParser(description=__doc__)
    p.add_argument("config")
    p.add_argument("--seeds", type=int, default=5)
    p.add_argument("--latencies-us", default="0,100,1000,10000,100000")
    p.add_argument("--queues", default="back,front")
    args = p.parse_args()

    latencies = [float(x) for x in args.latencies_us.split(",")]
    print(
        "| queue | latency | net PnL (mean ± sd) | lots filled | PnL per lot "
        "| 1s mid move per lot | Sharpe per 1s |"
    )
    print("|---|---|---|---|---|---|---|")
    for queue in args.queues.split(","):
        for lat in latencies:
            runs = [run(args.config, s, lat, queue) for s in range(1, args.seeds + 1)]
            pnl = [r["net_pnl"] for r in runs]
            lots = [r["filled_qty"] for r in runs]
            per_lot = sum(pnl) / max(sum(lots), 1)
            mid_move = statistics.mean(r["markouts"][1]["mid_move"] for r in runs)
            sharpe = statistics.mean(r["sharpe_per_interval"] for r in runs)
            sd = statistics.stdev(pnl) if len(pnl) > 1 else 0.0
            label = f"{lat / 1000:g} ms" if lat >= 1000 else f"{lat:g} µs"
            print(
                f"| {queue} | {label} | {statistics.mean(pnl):,.0f} ± {sd:,.0f} "
                f"| {statistics.mean(lots):,.0f} | {per_lot:+.3f} | {mid_move:+.3f} "
                f"| {sharpe:+.3f} |"
            )


if __name__ == "__main__":
    main()
