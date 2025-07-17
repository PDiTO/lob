//! Example strategies. They are here to exercise the simulator, not to make money.

mod market_maker;
mod momentum;

pub use market_maker::{MarketMaker, MarketMakerConfig, ReferencePrice};
pub use momentum::{Momentum, MomentumConfig};
