//! Health factor monitor for rate arb positions.
//! Health factor computation — provider stored for live Aave V3 queries.

use anyhow::Result;
use dashmap::DashMap;
use ethers::prelude::*;
use std::sync::Arc;
use crate::strategies::rate_arb::RateArbPosition;

/// Aave V3 Pool address on Base — resolved from PoolAddressesProvider at startup
// Aave pool address resolved dynamically at startup — see main.rs HealthFactorMonitor::new()

pub struct HealthFactorMonitor {
    rate_arb_positions: Arc<DashMap<Address, RateArbPosition>>,
    provider:           Arc<Provider<Ipc>>,
    aave_pool:          Address,
}

impl Clone for HealthFactorMonitor {
    fn clone(&self) -> Self {
        Self {
            rate_arb_positions: self.rate_arb_positions.clone(),
            provider:           self.provider.clone(),
            aave_pool:          self.aave_pool,
        }
    }
}

impl HealthFactorMonitor {
    pub fn new(provider: Arc<Provider<Ipc>>, aave_pool: Address) -> Self {
        Self {
            rate_arb_positions: Arc::new(DashMap::new()),
            provider,
            aave_pool,
        }
    }

    pub fn register_position(&self, pos: RateArbPosition) {
        self.rate_arb_positions.insert(pos.asset, pos);
    }

    pub fn remove_position(&self, asset: Address) {
        self.rate_arb_positions.remove(&asset);
    }

    pub fn get_rate_arb_positions(&self) -> Vec<RateArbPosition> {
        // LAT-09: iterate without collect where possible; this collect is acceptable
        // for the expected small number of S5 positions (typically 1-3).
        self.rate_arb_positions.iter().map(|e| e.value().clone()).collect()
    }

    /// Update in-memory health factor for a specific asset position.
    pub fn update_hf(&self, asset: Address, live_hf: f64) {
        if let Some(mut pos) = self.rate_arb_positions.get_mut(&asset) {
            pos.health_factor = live_hf;
        }
    }

    /// Query live health factor from Aave V3 for the FlashExecutor.
    ///
    /// Calls `IAaveV3Pool.getUserAccountData(flashExecutor)` and reads the
    /// healthFactor field (6th return value, Ray-scaled = 1e18 = HF 1.0).
    /// Updates all in-memory positions with the fresh on-chain value.
    pub async fn refresh_hf_from_chain(&self, flash_executor: Address) -> Result<f64> {
        // ABI encode: getUserAccountData(address)
        let sel  = &ethers::utils::keccak256(b"getUserAccountData(address)")[..4];
        let data = [sel, &ethers::abi::encode(&[ethers::abi::Token::Address(flash_executor)])].concat();

        let res = self.provider.call(
            &TransactionRequest {
                to:   Some(self.aave_pool.into()),
                data: Some(data.into()),
                ..Default::default()
            }.into(),
            None,
        ).await?;

        // getUserAccountData returns 6 uint256 values:
        // [0] totalCollateralBase  [1] totalDebtBase  [2] availableBorrowsBase
        // [3] currentLiquidationThreshold  [4] ltv  [5] healthFactor (Ray: 1e18 = 1.0)
        if res.len() < 192 {
            anyhow::bail!("getUserAccountData: unexpected response length {}", res.len());
        }
        let hf_ray = U256::from_big_endian(&res[160..192]);
        let hf     = hf_ray.as_u128() as f64 / 1e18;

        // Update all tracked positions with the fresh HF
        for mut pos in self.rate_arb_positions.iter_mut() {
            pos.health_factor = hf;
        }

        tracing::debug!("HF refreshed from chain: {:.4} (executor={:?})", hf, flash_executor);
        Ok(hf)
    }
}
