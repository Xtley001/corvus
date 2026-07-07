//! Interest rate feed.
//!
//! PHASE 2 (Critical bugs):
//!   - CRITICAL: Morpho market() ABI decode fixed — struct packs two uint128 per 32-byte word.
//!     Old code read full 32-byte words as uint256, getting garbage rates.
//!   - CRITICAL: Aave getReserveData() byte offsets fixed — packed uint128 pairs.
//!     Old code assumed each field was a standalone uint256 word.
//!
//! PHASE 5 (Profit):
//!   - PROFIT-6: wstETH and cbBTC added to monitored assets

use anyhow::Result;
use dashmap::DashMap;
use ethers::prelude::*;
use std::sync::Arc;
use crate::shared::addresses::base;

#[derive(Debug, Clone)]
pub struct RateSnapshot {
    pub asset:        Address,
    pub supply_apy:   f64,
    pub borrow_apy:   f64,
    pub supply_proto: Address,
    pub borrow_proto: Address,
}

#[derive(Clone)]
pub struct RateFeed {
    rates:    DashMap<Address, RateSnapshot>,
    provider: Arc<Provider<Ipc>>,
    morpho:   Address,
}

impl RateFeed {
    pub fn new(provider: Arc<Provider<Ipc>>) -> Self {
        Self {
            rates:  DashMap::new(),
            provider,
            morpho: base::MORPHO_BLUE.parse().expect("invalid Morpho address"),
        }
    }

    /// Query live APY from Aave V3 and Morpho for each monitored asset.
    /// PROFIT-6: now includes wstETH and cbBTC in addition to USDC, WETH, cbETH.
    pub async fn refresh(&self, aave_pool: Address) -> Result<()> {
        let assets_to_monitor: Vec<(&str, Address)> = vec![
            ("USDC",   base::USDC.parse()?),
            ("WETH",   base::WETH.parse()?),
            ("cbETH",  base::CBETH.parse()?),
            ("wstETH", base::WSTETH.parse()?),  // PROFIT-6
            ("cbBTC",  base::CBBTC.parse()?),   // PROFIT-6
        ];

        for (name, asset) in assets_to_monitor {
            match self.query_rates(aave_pool, asset).await {
                Ok(snap) => {
                    tracing::debug!(
                        "RateFeed {}: supply={:.2}% borrow={:.2}% spread={:.1}bps",
                        name,
                        snap.supply_apy * 100.0,
                        snap.borrow_apy * 100.0,
                        (snap.supply_apy - snap.borrow_apy) * 10_000.0,
                    );
                    self.rates.insert(asset, snap);
                }
                Err(e) => tracing::warn!("RateFeed query failed for {}: {}", name, e),
            }
        }
        Ok(())
    }

    pub fn monitored_assets(&self) -> Vec<Address> {
        self.rates.iter().map(|e| *e.key()).collect()
    }

    pub fn get_best_spread(&self, asset: Address) -> (f64, f64) {
        match self.rates.get(&asset) {
            Some(r) => (r.supply_apy, r.borrow_apy),
            None    => (0.0, 0.0),
        }
    }

    pub fn net_spread_bps(&self, asset: Address) -> f64 {
        let (s, b) = self.get_best_spread(asset);
        (s - b) * 10_000.0
    }

    pub fn get_best_proto_addresses(&self, asset: Address) -> (Address, Address) {
        match self.rates.get(&asset) {
            Some(r) => (r.supply_proto, r.borrow_proto),
            None    => (Address::zero(), Address::zero()),
        }
    }

    // ─── Private: on-chain queries ────────────────────────────────────────

    async fn query_rates(&self, aave_pool: Address, asset: Address) -> Result<RateSnapshot> {
        let (aave_supply, aave_borrow) = self.query_aave_rates(aave_pool, asset).await
            .unwrap_or((0.0, f64::MAX));
        let (morpho_supply, morpho_borrow) = self.query_morpho_rates(asset).await
            .unwrap_or((0.0, f64::MAX));

        let (supply_apy, supply_proto) = if morpho_supply > aave_supply {
            (morpho_supply, self.morpho)
        } else {
            (aave_supply, aave_pool)
        };
        let (borrow_apy, borrow_proto) = if morpho_borrow < aave_borrow {
            (morpho_borrow, self.morpho)
        } else {
            (aave_borrow, aave_pool)
        };

        Ok(RateSnapshot { asset, supply_apy, borrow_apy, supply_proto, borrow_proto })
    }

    /// Query Aave V3 getReserveData() — PHASE 2 CRITICAL FIX.
    ///
    /// Aave V3 ReserveData packs TWO uint128 values per 32-byte ABI word.
    /// Old code assumed each field was a standalone uint256 word — WRONG.
    ///
    /// Correct packed layout:
    ///   Word 0 [0..32]:   configuration bitmap (full 256-bit)
    ///   Word 1 [32..64]:  [0..16] liquidityIndex (u128) | [16..32] currentLiquidityRate (u128)
    ///   Word 2 [64..96]:  [32..48] variableBorrowIndex (u128) | [48..64] currentVariableBorrowRate (u128)
    ///
    /// So:
    ///   currentLiquidityRate    = bytes [48..64]  (upper half of word 1)
    ///   currentVariableBorrowRate = bytes [80..96] (upper half of word 2)
    async fn query_aave_rates(&self, aave_pool: Address, asset: Address) -> Result<(f64, f64)> {
        let sel  = &ethers::utils::keccak256(b"getReserveData(address)")[..4];
        let data = [sel, &ethers::abi::encode(&[ethers::abi::Token::Address(asset)])].concat();

        let res = self.provider.call(
            &TransactionRequest { to: Some(aave_pool.into()), data: Some(data.into()), ..Default::default() }.into(),
            None,
        ).await?;

        if res.len() < 96 {
            anyhow::bail!("getReserveData: response too short ({})", res.len());
        }

        // PHASE 2 FIX: correct byte offsets for packed uint128 pairs
        // Word 1 upper 128 bits = bytes [48..64] = currentLiquidityRate (Ray)
        let liquidity_rate_ray  = u128::from_be_bytes(res[48..64].try_into()?);
        // Word 2 upper 128 bits = bytes [80..96] = currentVariableBorrowRate (Ray)
        let variable_borrow_ray = if res.len() >= 96 {
            u128::from_be_bytes(res[80..96].try_into()?)
        } else { 0u128 };

        const RAY: f64 = 1e27;
        let supply_apr = liquidity_rate_ray as f64 / RAY;
        let borrow_apr = variable_borrow_ray as f64 / RAY;

        Ok((supply_apr, borrow_apr))
    }

    /// Query Morpho Blue market() — PHASE 2 CRITICAL FIX.
    ///
    /// Morpho's Market struct packs TWO uint128 values per 32-byte ABI word:
    ///   Word 0 [0..32]:   [0..16] totalSupplyAssets  | [16..32] totalSupplyShares
    ///   Word 1 [32..64]:  [32..48] totalBorrowAssets | [48..64] totalBorrowShares
    ///   Word 2 [64..96]:  [64..80] lastUpdate        | [80..96] fee
    ///
    /// Old code read res[0..32] and res[64..96] as full uint256 words — WRONG.
    async fn query_morpho_rates(&self, asset: Address) -> Result<(f64, f64)> {
        let market_id = self.derive_market_id(asset)
            .ok_or_else(|| anyhow::anyhow!("no verified Morpho market for asset {:?}", asset))?;

        let sel  = &ethers::utils::keccak256(b"market(bytes32)")[..4];
        let data = [sel, market_id.as_bytes()].concat();

        let res = self.provider.call(
            &TransactionRequest { to: Some(self.morpho.into()), data: Some(data.into()), ..Default::default() }.into(),
            None,
        ).await?;

        if res.len() < 96 {
            anyhow::bail!("Morpho market(): response too short ({})", res.len());
        }

        // PHASE 2 FIX: read packed 128-bit pairs from correct byte offsets
        let total_supply_assets = u128::from_be_bytes(res[0..16].try_into()?);
        let total_borrow_assets = u128::from_be_bytes(res[32..48].try_into()?);
        let fee_raw             = if res.len() >= 96 { u128::from_be_bytes(res[80..96].try_into()?) } else { 0u128 };
        let fee = fee_raw as f64 / 1e18;

        if total_supply_assets == 0 {
            anyhow::bail!("Morpho market: zero supply assets");
        }

        let utilization = (total_borrow_assets as f64 / total_supply_assets as f64).min(1.0);

        // AdaptiveCurveIrm: linear to 80% util, then spike
        let borrow_apr = if utilization < 0.8 {
            utilization * 0.10
        } else {
            0.08 + (utilization - 0.8) * 1.5
        };
        let supply_apr = borrow_apr * utilization * (1.0 - fee);

        Ok((supply_apr, borrow_apr))
    }

    /// Map a monitored asset to its Morpho Blue market ID.
    /// Returns None when the asset is unmapped OR the mapped ID is still the
    /// all-zero placeholder (unverified market) — callers must skip Morpho for
    /// that asset rather than querying market 0x0.
    fn derive_market_id(&self, asset: Address) -> Option<H256> {
        let asset_hex = format!("{:?}", asset).to_lowercase();
        let id_str = if asset_hex.contains("833589") {
            base::MORPHO_MARKET_USDC_WETH
        } else if asset_hex.contains("420000") {
            base::MORPHO_MARKET_WETH_USDC
        } else if asset_hex.contains("2ae3f1") {
            base::MORPHO_MARKET_CBETH_USDC
        } else if asset_hex.contains("cbb7c0") {
            base::MORPHO_MARKET_CBBTC_USDC
        } else if asset_hex.contains("c1cba3") {
            base::MORPHO_MARKET_WSTETH_USDC
        } else {
            tracing::debug!("derive_market_id: unmapped asset {:?}", asset);
            return None;
        };
        // Reject the all-zero placeholder — market unverified / not yet set.
        if id_str.chars().all(|c| c == '0') {
            tracing::debug!("derive_market_id: {:?} market ID is unset placeholder — skipping Morpho", asset);
            return None;
        }
        let bytes = hex::decode(id_str).ok()?;
        if bytes.len() != 32 { return None; }
        Some(H256::from_slice(&bytes))
    }
}
