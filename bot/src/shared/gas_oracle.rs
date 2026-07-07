//! Gas oracle — dynamic base-fee caching.
//!
//! LAT-IPC-2 fix: add set_base_fee() so main.rs can push the base fee from
//! the block header directly (eliminates one fee_history IPC round-trip per block).
//!
//! Config-driven priority/max-fee multipliers (was hardcoded 10% / 2×).

use ethers::prelude::*;
use std::sync::{Arc, atomic::{AtomicU64, Ordering}};
use crate::config::Config;

/// Default floor (0.001 gwei) used only if the chain query fails entirely.
const BASE_FEE_FLOOR_WEI: u64 = 1_000_000;

pub struct GasOracle {
    cached_base_fee:      Arc<AtomicU64>,
    provider:             Arc<Provider<Ipc>>,
    priority_fee_mult:    f64,   // config: priority_fee_multiplier
    max_fee_base_mult:    f64,   // config: max_fee_base_multiplier
}

impl GasOracle {
    pub fn new(provider: Arc<Provider<Ipc>>, cfg: &Config) -> Self {
        Self {
            cached_base_fee:   Arc::new(AtomicU64::new(BASE_FEE_FLOOR_WEI)),
            provider,
            priority_fee_mult: cfg.priority_fee_multiplier,
            max_fee_base_mult: cfg.max_fee_base_multiplier,
        }
    }

    /// LAT-IPC-2: push base fee from block header — no extra IPC round-trip.
    /// Call this in main.rs immediately after receiving each new block.
    pub fn set_base_fee(&self, fee_wei: u64) {
        let fee = fee_wei.max(BASE_FEE_FLOOR_WEI);
        self.cached_base_fee.store(fee, Ordering::SeqCst);
        tracing::debug!("GasOracle: base_fee={:.4} gwei (from block header)", fee as f64 / 1e9);
    }

    /// Fallback: fetch base fee from fee_history if block header not available.
    /// Use set_base_fee() instead in the main block loop.
    pub async fn refresh(&self) {
        match self.provider.fee_history(1u64, BlockNumber::Latest, &[]).await {
            Ok(history) => {
                if let Some(fee) = history.base_fee_per_gas.last() {
                    self.set_base_fee(fee.as_u64());
                }
            }
            Err(e) => tracing::warn!("GasOracle refresh failed: {} — using stale value", e),
        }
    }

    pub fn base_fee_wei(&self) -> u64 {
        self.cached_base_fee.load(Ordering::SeqCst)
    }

    /// Returns (priority_fee_wei, max_fee_per_gas_wei) for a given tip multiplier.
    /// max_fee = base × max_fee_base_mult + priority_fee
    pub fn max_fee_wei(&self, tip_mult: f64) -> (u64, u64) {
        let base     = self.base_fee_wei();
        let priority = (base as f64 * self.priority_fee_mult * tip_mult) as u64;
        let max_fee  = (base as f64 * self.max_fee_base_mult) as u64 + priority;
        (priority, max_fee)
    }

    /// Effective gas price for cost calculations = base + priority_tip.
    pub fn effective_gas_price_wei(&self) -> f64 {
        let base = self.base_fee_wei() as f64;
        base * (1.0 + self.priority_fee_mult)
    }
}
