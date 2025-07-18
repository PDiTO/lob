//! Smoke tests for the `lob` binary.

use std::path::PathBuf;
use std::process::Command;

fn lob() -> Command {
    Command::new(env!("CARGO_BIN_EXE_lob"))
}

fn repo_root() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../..")
}

fn run_ok(cmd: &mut Command) -> String {
    let out = cmd.output().expect("failed to run lob");
    assert!(
        out.status.success(),
        "lob failed: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    String::from_utf8(out.stdout).unwrap()
}

#[test]
fn bench_reports_throughput_and_latency() {
    let out = run_ok(lob().args(["bench", "--ops", "20000", "--depth", "500", "--runs", "1"]));
    assert!(out.contains("M msgs/s"), "{out}");
    assert!(out.contains("p99.9"), "{out}");
}

#[test]
fn backtest_replays_the_sample_feed_as_json() {
    let config = repo_root().join("configs/replay.toml");
    let out = run_ok(lob().args(["backtest", "--json", "--config"]).arg(&config));
    let v: serde_json::Value = serde_json::from_str(&out).unwrap();
    assert_eq!(v["feed_events"], 2832);
    assert!(v["fills"].as_u64().unwrap() > 0);
}

#[test]
fn overrides_change_the_run() {
    let config = repo_root().join("configs/market_maker.toml");
    let run = |extra: &[&str]| {
        let out = run_ok(
            lob()
                .args(["backtest", "--json", "--duration", "30", "--config"])
                .arg(&config)
                .args(extra),
        );
        serde_json::from_str::<serde_json::Value>(&out).unwrap()
    };
    let back = run(&[]);
    let front = run(&["--queue", "front"]);
    assert!(front["filled_qty"].as_u64() > back["filled_qty"].as_u64());
}

#[test]
fn generate_writes_a_feed_that_backtest_can_read() {
    let dir = std::env::temp_dir().join(format!("lob-cli-test-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    let feed = dir.join("feed.csv");
    run_ok(
        lob()
            .args(["generate", "--seed", "3", "--duration", "5", "--out"])
            .arg(&feed),
    );
    let config = dir.join("cfg.toml");
    std::fs::write(
        &config,
        "feed_csv = \"feed.csv\"\n[strategy]\ntype = \"market_maker\"\n",
    )
    .unwrap();
    let out = run_ok(lob().args(["backtest", "--config"]).arg(&config));
    assert!(out.contains("net pnl"), "{out}");
    std::fs::remove_dir_all(&dir).unwrap();
}

#[test]
fn bad_config_is_a_clean_error() {
    let dir = std::env::temp_dir().join(format!("lob-cli-bad-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    let config = dir.join("bad.toml");
    std::fs::write(&config, "[sim]\nnot_a_field = 1\n").unwrap();
    let out = lob()
        .args(["backtest", "--config"])
        .arg(&config)
        .output()
        .unwrap();
    assert!(!out.status.success());
    assert!(String::from_utf8_lossy(&out.stderr).contains("not_a_field"));
    std::fs::remove_dir_all(&dir).unwrap();
}
