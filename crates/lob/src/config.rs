//! Backtest configuration files (TOML).
//!
//! ```toml
//! [market]            # synthetic market; ignored if feed_csv is set
//! seed = 7
//! duration_s = 600
//!
//! [sim]
//! order_latency_ns = 50_000
//! market_data_latency_ns = 50_000
//! queue_position = "back"
//!
//! [strategy]
//! type = "market_maker"
//! half_spread = 1.0
//! ```

use std::fs::File;
use std::io::BufReader;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};
use thiserror::Error;

use crate::backtest::{self, BacktestResult, Idle, SimConfig, Strategy};
use crate::feed::{self, FeedError, FeedEvent};
use crate::strategies::{MarketMaker, MarketMakerConfig, Momentum, MomentumConfig};
use crate::synthetic::{SyntheticConfig, SyntheticMarket};

#[derive(Debug, Error)]
pub enum ConfigError {
    #[error("reading {path}: {source}")]
    Io {
        path: PathBuf,
        source: std::io::Error,
    },
    #[error("parsing config: {0}")]
    Toml(#[from] toml::de::Error),
    #[error("reading feed {path}: {source}")]
    Feed { path: PathBuf, source: FeedError },
}

/// Which built-in strategy to run.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum StrategyConfig {
    /// Sends nothing. The PnL is zero; useful for checking the feed.
    #[default]
    None,
    MarketMaker(MarketMakerConfig),
    Momentum(MomentumConfig),
}

impl StrategyConfig {
    pub fn build(&self) -> Box<dyn Strategy> {
        match self {
            StrategyConfig::None => Box::new(Idle),
            StrategyConfig::MarketMaker(c) => Box::new(MarketMaker::new(c.clone())),
            StrategyConfig::Momentum(c) => Box::new(Momentum::new(c.clone())),
        }
    }
}

#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct BacktestConfig {
    pub market: SyntheticConfig,
    /// Replay this CSV feed instead of generating one. Relative paths are resolved
    /// against the config file's directory.
    pub feed_csv: Option<PathBuf>,
    pub sim: SimConfig,
    pub strategy: StrategyConfig,
}

impl BacktestConfig {
    pub fn from_toml(s: &str) -> Result<Self, ConfigError> {
        Ok(toml::from_str(s)?)
    }

    pub fn load(path: &Path) -> Result<Self, ConfigError> {
        let text = std::fs::read_to_string(path).map_err(|source| ConfigError::Io {
            path: path.to_owned(),
            source,
        })?;
        let mut cfg = Self::from_toml(&text)?;
        if let Some(feed) = &cfg.feed_csv
            && feed.is_relative()
            && let Some(dir) = path.parent()
        {
            cfg.feed_csv = Some(dir.join(feed));
        }
        Ok(cfg)
    }

    /// The feed this config describes: the CSV if given, else the synthetic market.
    pub fn feed(&self) -> Result<Box<dyn Iterator<Item = FeedEvent>>, ConfigError> {
        match &self.feed_csv {
            Some(path) => {
                let err = |source| ConfigError::Feed {
                    path: path.clone(),
                    source,
                };
                let file = File::open(path).map_err(|e| err(FeedError::Io(e)))?;
                let events = feed::read_csv(BufReader::new(file)).map_err(err)?;
                Ok(Box::new(events.into_iter()))
            }
            None => Ok(Box::new(SyntheticMarket::new(self.market.clone()))),
        }
    }

    /// Runs the configured built-in strategy.
    pub fn run(&self) -> Result<BacktestResult, ConfigError> {
        let mut strategy = self.strategy.build();
        self.run_with(strategy.as_mut())
    }

    /// Runs the configured feed and sim settings with a caller-supplied strategy.
    pub fn run_with(&self, strategy: &mut dyn Strategy) -> Result<BacktestResult, ConfigError> {
        Ok(backtest::run(self.feed()?, strategy, &self.sim))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::book::Placement;

    #[test]
    fn parses_a_full_config() {
        let cfg = BacktestConfig::from_toml(
            r#"
            [market]
            seed = 3
            duration_s = 10
            fair_value = "mean_reverting"

            [sim]
            order_latency_ns = 1_000
            queue_position = "front"
            markout_horizons_ns = [5]

            [strategy]
            type = "market_maker"
            half_spread = 2.0
            reference = "mid"
            "#,
        )
        .unwrap();
        assert_eq!(cfg.market.seed, 3);
        assert_eq!(cfg.sim.order_latency_ns, 1_000);
        assert_eq!(cfg.sim.queue_position, Placement::Front);
        match cfg.strategy {
            StrategyConfig::MarketMaker(m) => assert_eq!(m.half_spread, 2.0),
            other => panic!("wrong strategy {other:?}"),
        }
    }

    #[test]
    fn unknown_keys_are_errors() {
        assert!(BacktestConfig::from_toml("[sim]\nlatency = 5\n").is_err());
        assert!(BacktestConfig::from_toml("[strategy]\ntype = \"yolo\"\n").is_err());
    }

    #[test]
    fn empty_config_runs_the_idle_strategy() {
        let mut cfg = BacktestConfig::from_toml("").unwrap();
        cfg.market.duration_s = 5.0;
        let r = cfg.run().unwrap();
        assert_eq!(r.summary.fills, 0);
        assert_eq!(r.summary.net_pnl, 0.0);
        assert!(r.summary.feed_events > 100);
    }
}
