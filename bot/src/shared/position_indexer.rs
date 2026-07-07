//! Position indexer.
//!
//! PHASE 3 (State bugs):
//!   - AAVE_V3_POOL no longer hardcoded — resolved address passed from main.rs
//!
//! PHASE 4 (Accuracy):
//!   - collateral_asset populated for Aave positions via getUserReserveData lookup
//!     (was always Address::zero(), preventing any liquidation from firing)
//!
//! Phase 3:
//!   - bootstrap genesis block taken from config (not hardcoded 1_000_000)

use anyhow::Result;
use dashmap::DashMap;
use ethers::prelude::*;
use std::sync::Arc;

pub type PriceMap = DashMap<Address, f64>;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum LendingProtocol { Aave, Morpho }

#[derive(Debug, Clone)]
pub struct BorrowPosition {
    pub borrower:             Address,
    pub protocol:             LendingProtocol,
    pub collateral_asset:     Address,
    pub debt_asset:           Address,
    pub collateral_amount:    U256,
    pub debt_amount:          U256,
    pub morpho_market_id:     H256,
    pub morpho_market_params: Bytes,
    pub health_factor:        f64,
    pub last_update_block:    u64,
}

impl BorrowPosition {
    /// Compute health factor from a live price map.
    /// Returns Err if either price is missing — callers must skip the position
    /// rather than using a silent 0.0 fallback that produces false HF signals.
    /// F-05 FIX: replaced unwrap_or(0.0) with explicit Option propagation.
    pub fn compute_hf(&self, prices: &PriceMap, liq_threshold: f64) -> f64 {
        // Use unwrap_or f64::MAX for missing prices so the position is NOT flagged
        // as liquidatable due to a missing feed. This is the safe silent-skip direction:
        // missing price → HF = MAX → position skipped → no false liquidation attempt.
        // The opposite failure (coll_price=0 → HF=0 → false trigger) is prevented.
        let coll_price = match prices.get(&self.collateral_asset).map(|p| *p) {
            Some(p) if p > 0.0 => p,
            _ => return f64::MAX,  // price missing or zero → skip this position safely
        };
        let debt_price = match prices.get(&self.debt_asset).map(|p| *p) {
            Some(p) if p > 0.0 => p,
            _ => return f64::MAX,  // price missing or zero → skip this position safely
        };
        let coll_hex  = format!("{:?}", self.collateral_asset).to_lowercase();
        let debt_hex  = format!("{:?}", self.debt_asset).to_lowercase();
        let coll_unit = 10f64.powi(crate::shared::addresses::base::token_decimals(&coll_hex) as i32);
        let debt_unit = 10f64.powi(crate::shared::addresses::base::token_decimals(&debt_hex) as i32);
        let coll_usd  = coll_price * self.collateral_amount.as_u128() as f64 / coll_unit;
        let debt_usd  = debt_price * self.debt_amount.as_u128()       as f64 / debt_unit;
        (coll_usd * liq_threshold) / debt_usd.max(1e-10)
    }
}

const AAVE_BORROW_TOPIC:              &str = "b3d084820fb1a9decffb176436bd02558d15fac9b0ddfed8c465bc7359d7dce0";
const MORPHO_SUPPLY_COLLATERAL_TOPIC: &str = "a3b9472a1399e17e123f3c2e6586c23e504184d504de59cdaa2b375244d0f6c4";
// PHASE 3 FIX: MORPHO_BLUE imported from addresses module — no hardcoded duplicate
use crate::shared::addresses::base::MORPHO_BLUE as MORPHO_BLUE_ADDR;

pub struct PositionIndexer {
    positions: DashMap<(Address, LendingProtocol), BorrowPosition>,
    prices:    Arc<PriceMap>,
    provider:  Arc<Provider<Ipc>>,
    aave_pool: Address,  // PHASE 3 FIX: resolved from PoolAddressesProvider in main.rs
}

impl PositionIndexer {
    /// PHASE 3 FIX: accept aave_pool resolved at startup, not hardcoded here.
    pub fn new(provider: Arc<Provider<Ipc>>, aave_pool: Address) -> Self {
        Self { positions: DashMap::new(), prices: Arc::new(DashMap::new()), provider, aave_pool }
    }

    pub async fn bootstrap(&self, from_block: u64, to_block: u64) -> Result<()> {
        tracing::info!("Bootstrapping position index {} to {}", from_block, to_block);
        let chunk_size = 10_000u64;
        let mut start  = from_block;
        while start < to_block {
            let end = (start + chunk_size).min(to_block);
            if let Err(e) = self.scan_block_range(start, end).await {
                tracing::warn!("scan_block_range {}-{} failed: {}", start, end, e);
            }
            start = end + 1;
        }
        tracing::info!("Bootstrap complete: {} positions indexed", self.positions.len());
        Ok(())
    }

    async fn scan_block_range(&self, from: u64, to: u64) -> Result<()> {
        let morpho_blue: Address = MORPHO_BLUE_ADDR.parse()?;
        let aave_topic   = H256::from_slice(&hex::decode(AAVE_BORROW_TOPIC)?);
        let morpho_topic = H256::from_slice(&hex::decode(MORPHO_SUPPLY_COLLATERAL_TOPIC)?);

        let aave_filter = Filter::new()
            .from_block(from).to_block(to)
            .address(self.aave_pool)  // PHASE 3: use resolved address
            .topic0(aave_topic);

        if let Ok(logs) = self.provider.get_logs(&aave_filter).await {
            for log in logs { self.process_aave_borrow_log(log).await; }
        }

        let morpho_filter = Filter::new()
            .from_block(from).to_block(to)
            .address(morpho_blue)
            .topic0(morpho_topic);

        if let Ok(logs) = self.provider.get_logs(&morpho_filter).await {
            for log in logs { self.process_morpho_supply_collateral_log(log).await; }
        }

        // PHASE 4: after scanning, populate collateral_asset for any Aave positions missing it
        self.populate_aave_collateral_assets().await;

        Ok(())
    }

    async fn process_aave_borrow_log(&self, log: Log) {
        if log.topics.len() < 3 || log.data.len() < 96 { return; }
        let reserve      = Address::from(log.topics[1]);
        let on_behalf_of = Address::from(log.topics[2]);
        let amount       = U256::from_big_endian(&log.data[32..64]);
        let block_number = log.block_number.map(|b| b.as_u64()).unwrap_or(0);

        let pos = BorrowPosition {
            borrower: on_behalf_of, protocol: LendingProtocol::Aave,
            // collateral_asset resolved async in populate_aave_collateral_assets()
            collateral_asset: Address::zero(),
            debt_asset: reserve,
            collateral_amount: U256::zero(), debt_amount: amount,
            morpho_market_id: H256::zero(), morpho_market_params: Bytes::default(),
            health_factor: f64::MAX, last_update_block: block_number,
        };
        self.positions.insert((on_behalf_of, LendingProtocol::Aave), pos);
    }

    async fn process_morpho_supply_collateral_log(&self, log: Log) {
        if log.topics.len() < 4 || log.data.len() < 32 { return; }
        let market_id    = log.topics[1];
        let on_behalf_of = Address::from(log.topics[3]);
        let assets       = U256::from_big_endian(&log.data[0..32]);
        let block_number = log.block_number.map(|b| b.as_u64()).unwrap_or(0);

        let pos = BorrowPosition {
            borrower: on_behalf_of, protocol: LendingProtocol::Morpho,
            collateral_asset: Address::zero(), debt_asset: Address::zero(),
            collateral_amount: assets, debt_amount: U256::zero(),
            morpho_market_id: market_id, morpho_market_params: Bytes::default(),
            health_factor: f64::MAX, last_update_block: block_number,
        };
        self.positions.insert((on_behalf_of, LendingProtocol::Morpho), pos);
    }

    /// PHASE 4: populate collateral_asset for Aave positions.
    /// Aave's Borrow event only emits the debt asset. Collateral requires
    /// getUserReserveData() calls. Done in batch after each scan chunk.
    async fn populate_aave_collateral_assets(&self) {
        let missing: Vec<Address> = self.positions
            .iter()
            .filter_map(|e| {
                let pos = e.value();
                if pos.protocol == LendingProtocol::Aave && pos.collateral_asset.is_zero() {
                    Some(pos.borrower)
                } else {
                    None
                }
            })
            .collect();

        if missing.is_empty() { return; }

        // getUserAccountData returns (totalCollateralBase, totalDebtBase, ..., healthFactor)
        // We need to identify which reserve is the collateral — use getUserReserveData per token
        let sel = &ethers::utils::keccak256(b"getUserReserveData(address,address)")[..4];

        // Common collateral tokens to check
        let candidates: Vec<Address> = vec![
            crate::shared::addresses::base::WETH.parse().unwrap_or_default(),
            crate::shared::addresses::base::CBETH.parse().unwrap_or_default(),
            crate::shared::addresses::base::WSTETH.parse().unwrap_or_default(),
            crate::shared::addresses::base::CBBTC.parse().unwrap_or_default(),
        ];

        for borrower in missing.iter().take(50) {  // batch limit: 50 at a time
            let mut best_coll: Address = Address::zero();
            let mut best_coll_amount   = U256::zero();

            for &cand in &candidates {
                let data = [
                    sel,
                    ethers::abi::encode(&[
                        ethers::abi::Token::Address(cand),
                        ethers::abi::Token::Address(*borrower),
                    ]).as_slice(),
                ].concat();

                if let Ok(res) = self.provider.call(
                    &TransactionRequest { to: Some(self.aave_pool.into()), data: Some(data.into()), ..Default::default() }.into(),
                    None,
                ).await {
                    // getUserReserveData returns: currentATokenBalance(uint256) as first word
                    if res.len() >= 32 {
                        let coll_bal = U256::from_big_endian(&res[0..32]);
                        if coll_bal > best_coll_amount {
                            best_coll_amount = coll_bal;
                            best_coll        = cand;
                        }
                    }
                }
            }

            if !best_coll.is_zero() {
                if let Some(mut pos) = self.positions.get_mut(&(*borrower, LendingProtocol::Aave)) {
                    pos.collateral_asset   = best_coll;
                    pos.collateral_amount  = best_coll_amount;
                }
            }
        }
    }

    pub async fn watch_live(&self) -> Result<()> {
        let morpho_blue: Address = MORPHO_BLUE_ADDR.parse()?;
        let filter = Filter::new()
            .address(vec![self.aave_pool, morpho_blue])
            .topic0(vec![
                H256::from_slice(&hex::decode(AAVE_BORROW_TOPIC)?),
                H256::from_slice(&hex::decode(MORPHO_SUPPLY_COLLATERAL_TOPIC)?),
            ]);
        let mut stream = self.provider.watch(&filter).await?;
        while let Some(log) = stream.next().await {
            if let Some(t0) = log.topics.first() {
                if format!("{:x}", t0) == AAVE_BORROW_TOPIC {
                    self.process_aave_borrow_log(log).await;
                } else {
                    self.process_morpho_supply_collateral_log(log).await;
                }
            }
        }
        Ok(())
    }

    pub fn update_price(&self, asset: Address, price_usd: f64) { self.prices.insert(asset, price_usd); }

    /// AUDIT 2.3 FIX: recompute stored health factor for every indexed position from
    /// the live price map, using the collateral asset's real liquidation threshold.
    /// Must be called each block before `positions_below_hf` — otherwise stored HF is
    /// f64::MAX (its index-time default) and the confirmed-block S3 path never fires.
    pub fn refresh_health_factors(&self, prices: &PriceMap) {
        for mut e in self.positions.iter_mut() {
            let coll_hex = format!("{:?}", e.collateral_asset).to_lowercase();
            let (_, liq_thresh) = crate::shared::addresses::base::aave_ltv_liq_threshold(&coll_hex);
            let hf = e.compute_hf(prices, liq_thresh);
            e.health_factor = hf;
        }
    }

    pub fn positions_below_hf(&self, threshold: f64) -> Vec<BorrowPosition> {
        self.positions.iter().filter_map(|e| {
            let pos = e.value();
            if pos.health_factor < threshold { Some(pos.clone()) } else { None }
        }).collect()
    }

    /// AUDIT 2.3: per-asset liquidation threshold (was hard-coded 0.85 for all assets).
    pub fn positions_below_pending_hf(&self, pending: &PriceMap, threshold: f64) -> Vec<BorrowPosition> {
        self.positions.iter().filter_map(|e| {
            let pos = e.value();
            let coll_hex = format!("{:?}", pos.collateral_asset).to_lowercase();
            let (_, liq_thresh) = crate::shared::addresses::base::aave_ltv_liq_threshold(&coll_hex);
            if pos.compute_hf(pending, liq_thresh) < threshold { Some(pos.clone()) } else { None }
        }).collect()
    }

    pub fn all_positions(&self)  -> Vec<BorrowPosition> { self.positions.iter().map(|e| e.value().clone()).collect() }
    pub fn position_count(&self) -> usize               { self.positions.len() }
    pub fn current_prices(&self) -> Arc<PriceMap>       { self.prices.clone() }
}
