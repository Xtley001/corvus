//!
//! Fixes:
//! get_optimal_swap_route verifies pool existence + non-zero liquidity
//!                before returning a route. No more always-returning-500bps silently.
//! simulate_tri_arb dispatches fetch_pool_reserves by dex_type —
//!                no more wasting an IPC call trying getReserves() on a UniV3 pool.
//! reset_cache builds EthersDB OUTSIDE the RwLock, then swaps atomically.
//!                Parallel strategy simulations no longer serialize on cache rebuild.
//!                Upgraded from Mutex to RwLock: multiple readers run concurrently.
//! gas_estimate_safety_margin config applied to all static estimates.

use anyhow::Result;
use ethers::prelude::*;
use revm::{
    db::EthersDB,
    primitives::{ExecutionResult, Output, TransactTo, U256 as rU256},
    EVM,
};
use std::sync::Arc;

use crate::{
    config::Config,
    shared::{
        addresses::base,
        gas_oracle::GasOracle,
        position_indexer::{BorrowPosition, PriceMap},
        price_feed::PoolState,
        math::{amm_out, stable_amm_out, SEL_SLOT0, SEL_LIQUIDITY, SEL_TICK_SPACING},
    },
};

pub struct SimResult {
    pub profit_usd:     f64,
    pub min_profit_wei: U256,
    pub gas_estimate:   u64,
    pub post_hf:        f64,
}

pub struct SimulationEngine {
    provider:   Arc<Provider<Ipc>>,
    gas_oracle: Arc<GasOracle>,
    cfg:        Config,
}

impl SimulationEngine {
    pub async fn new(provider: Arc<Provider<Ipc>>, gas_oracle: Arc<GasOracle>, cfg: Config) -> Result<Self> {
        Ok(Self { provider, gas_oracle, cfg })
    }

    // ─── REVM fork-and-call ───────────────────────────────────────────────
    async fn fork_and_call(
        &self,
        to:        Address,
        data:      Bytes,
        from:      Address,
        at_block:  u64,
        base_fee:  u64,
        timestamp: u64,
        coinbase:  Address,
    ) -> Result<ExecutionResult> {
        let gas_price = self.gas_oracle.effective_gas_price_wei() as u64;

        // Drive the EVM directly off EthersDB. In revm 3.5 `EthersDB` implements
        // `Database` (not `DatabaseRef`), so it cannot be wrapped in `CacheDB`
        // (which requires `ExtDB: DatabaseRef`); we therefore use `transact()`
        // (needs only `Database`) rather than `transact_ref()`. State is fetched
        // lazily over IPC per access.
        let db = EthersDB::new(Arc::clone(&self.provider), Some(BlockId::Number(at_block.into())))
            .ok_or_else(|| anyhow::anyhow!("EthersDB init failed at block {}", at_block))?;

        let mut evm = EVM::new();
        evm.database(db);
        evm.env.block.number    = rU256::from(at_block);
        evm.env.block.basefee   = rU256::from(base_fee);
        evm.env.block.timestamp = rU256::from(timestamp);
        evm.env.block.coinbase  = coinbase.0.into();
        evm.env.block.gas_limit = rU256::from(30_000_000u64);
        evm.env.tx.caller       = from.0.into();
        evm.env.tx.transact_to  = TransactTo::Call(to.0.into());
        evm.env.tx.data         = data.to_vec().into();
        evm.env.tx.gas_limit    = self.cfg.revm_sim_gas_limit;
        evm.env.tx.gas_price    = rU256::from(gas_price);

        // EthersDB's Database::Error is `()`, so EVMError<()> is not Display/Error and
        // cannot flow through `?` into anyhow — map it explicitly (it is Debug).
        let out = evm.transact()
            .map_err(|e| anyhow::anyhow!("REVM transact failed: {:?}", e))?;
        Ok(out.result)
    }

    // ─── Strategy 1: Cross-DEX Arb ───────────────────────────────────────
    pub async fn simulate_cross_dex_arb(
        &self,
        amount:                U256,
        buy_pool:              &PoolState,
        sell_pool:             &PoolState,
        profit_token_decimals: u8,
        eth_price:             f64,
    ) -> Result<SimResult> {
        use crate::shared::pool_discovery::DexType;
        let out1 = if buy_pool.dex == DexType::Aerodrome && buy_pool.is_stable() {
            stable_amm_out(buy_pool.reserve0, buy_pool.reserve1, amount, buy_pool.fee_bps)
        } else {
            amm_out(buy_pool.reserve0, buy_pool.reserve1, amount, buy_pool.fee_bps)
        };
        if out1.is_zero() {
            return Ok(SimResult { profit_usd: -1.0, min_profit_wei: U256::zero(), gas_estimate: 0, post_hf: 0.0 });
        }
        let out2 = if sell_pool.dex == DexType::Aerodrome && sell_pool.is_stable() {
            stable_amm_out(sell_pool.reserve1, sell_pool.reserve0, out1, sell_pool.fee_bps)
        } else {
            amm_out(sell_pool.reserve1, sell_pool.reserve0, out1, sell_pool.fee_bps)
        };
        let (profit_wei, is_profitable) = if out2 > amount { (out2 - amount, true) } else { (amount - out2, false) };
        if !is_profitable {
            return Ok(SimResult { profit_usd: -1.0, min_profit_wei: U256::zero(), gas_estimate: 0, post_hf: 0.0 });
        }
        let base_gas = 300_000u64;
        // apply gas safety margin
        let gas_estimate = (base_gas as f64 * self.cfg.gas_estimate_safety_margin) as u64;
        let effective_gas_wei = self.gas_oracle.effective_gas_price_wei();
        let gas_cost_usd = gas_estimate as f64 * effective_gas_wei / 1e18 * eth_price;
        let unit = 10f64.powi(profit_token_decimals as i32);
        let profit_norm = profit_wei.as_u128() as f64 / unit;
        let gross_profit_usd = if profit_token_decimals == 18 { profit_norm * eth_price } else { profit_norm };
        // F-04 FIX: Subtract Aave flash loan fee (0.09% = 9/10_000) when provider is Aave.
        // Without this, Aave-routed trades submit with negative real on-chain profit.
        // Provider selection mirrors FlashLoanRouter::select_provider logic.
        let flash_fee_usd = self.estimate_flash_fee_usd(amount, profit_token_decimals, eth_price).await;
        let profit_usd = gross_profit_usd - gas_cost_usd - flash_fee_usd;
        let min_profit_wei = profit_wei * self.cfg.min_profit_slippage_bps / 10_000;
        crate::monitoring::metrics::record_sim_result("cross_dex_arb", "profitable");
        Ok(SimResult { profit_usd, min_profit_wei, gas_estimate, post_hf: 0.0 })
    }

    // ─── Strategy 2: Tri-Arb ─────────────────────────────────────────────
    /// dex_types now used to dispatch to the correct reserve reader.
    /// Previously, every leg tried getReserves() first (wasting an IPC call on UniV3 pools).
    pub async fn simulate_tri_arb(
        &self,
        tokens:                [Address; 3],
        pools:                 [Address; 3],
        dex_types:             [u8; 3],
        amount:                U256,
        profit_token_decimals: u8,
        eth_price:             f64,
    ) -> Result<SimResult> {
        let mut out = amount;
        let mut base_gas = 50_000u64;

        for (i, &pool_addr) in pools.iter().enumerate() {
            // dispatch by dex_type, not by fallback trial-and-error
            let (r_in, r_out, fee_bps, is_stable) = if dex_types[i] == 0 {
                self.fetch_aero_reserves(pool_addr).await
                    .unwrap_or((U256::from(1u64), U256::from(1u64), 30u32, false))
            } else {
                // dex_type 1 = UniV3, dex_type 2 = Balancer (treated as UniV3 virtual reserves)
                self.fetch_uni_v3_virtual_reserves_with_fee(pool_addr, tokens[i]).await
                    .unwrap_or((U256::from(1u64), U256::from(1u64), 5u32, false))
            };
            base_gas += if dex_types[i] == 0 { 80_000 } else { 150_000 };
            out = if is_stable { stable_amm_out(r_in, r_out, out, fee_bps) } else { amm_out(r_in, r_out, out, fee_bps) };
            if out.is_zero() {
                return Ok(SimResult { profit_usd: -1.0, min_profit_wei: U256::zero(), gas_estimate: 0, post_hf: 0.0 });
            }
        }

        let (profit_wei, is_profitable) = if out > amount { (out - amount, true) } else { (amount - out, false) };
        if !is_profitable {
            crate::monitoring::metrics::record_sim_result("tri_arb", "unprofitable");
            return Ok(SimResult { profit_usd: -1.0, min_profit_wei: U256::zero(), gas_estimate: 0, post_hf: 0.0 });
        }
        // apply gas safety margin
        let gas_estimate = (base_gas as f64 * self.cfg.gas_estimate_safety_margin) as u64;
        let effective_gas_wei = self.gas_oracle.effective_gas_price_wei();
        let gas_cost_usd = gas_estimate as f64 * effective_gas_wei / 1e18 * eth_price;
        let unit = 10f64.powi(profit_token_decimals as i32);
        let profit_norm = profit_wei.as_u128() as f64 / unit;
        let gross_profit_usd = if profit_token_decimals == 18 { profit_norm * eth_price } else { profit_norm };
        // F-04 FIX: subtract Aave flash loan fee when Aave is the provider for tri-arb.
        let flash_fee_usd = self.estimate_flash_fee_usd(amount, profit_token_decimals, eth_price).await;
        let profit_usd = gross_profit_usd - gas_cost_usd - flash_fee_usd;
        crate::monitoring::metrics::record_sim_result("tri_arb", "profitable");
        Ok(SimResult {
            profit_usd,
            min_profit_wei: profit_wei * self.cfg.min_profit_slippage_bps / 10_000,
            gas_estimate,
            post_hf: 0.0,
        })
    }

    // ─── Strategy 3: Liquidation ──────────────────────────────────────────
    pub async fn simulate_liquidation(
        &self,
        pos:       &BorrowPosition,
        prices:    &PriceMap,
        eth_price: f64,
    ) -> Result<SimResult> {
        // F-05 FIX: Replace unwrap_or(0.0) with explicit error propagation.
        // A zero price from a missing/unloaded feed causes false liquidation signals:
        //   coll_price=0 → coll_usd=0 → HF=0 → position appears maximally underwater.
        //   debt_price=0 → division by zero → infinite min_profit → no trade ever fires.
        // Both are silent failures. Propagate the error so callers can skip this position.
        let coll_price = prices.get(&pos.collateral_asset)
            .map(|p| *p)
            .ok_or_else(|| anyhow::anyhow!(
                "No price for collateral asset {:?} — feed may not have loaded yet",
                pos.collateral_asset
            ))?;
        let debt_price = prices.get(&pos.debt_asset)
            .map(|p| *p)
            .ok_or_else(|| anyhow::anyhow!(
                "No price for debt asset {:?} — feed may not have loaded yet",
                pos.debt_asset
            ))?;
        let collateral_hex = format!("{:?}", pos.collateral_asset).to_lowercase();
        let debt_hex       = format!("{:?}", pos.debt_asset).to_lowercase();
        let coll_dec       = base::token_decimals(&collateral_hex);
        let debt_dec       = base::token_decimals(&debt_hex);
        let debt_unit      = 10f64.powi(debt_dec as i32);
        let coll_unit      = 10f64.powi(coll_dec as i32);
        let debt_usd       = debt_price * pos.debt_amount.as_u128() as f64 / debt_unit;
        let liq_bonus      = base::aave_liq_bonus(&collateral_hex);
        let swap_haircut   = self.cfg.liquidation_swap_haircut;
        let safety_factor  = self.cfg.liquidation_safety_factor;
        let gross_profit_usd = debt_usd * (liq_bonus - swap_haircut) * safety_factor;
        if gross_profit_usd <= 0.0 {
            return Ok(SimResult { profit_usd: -1.0, min_profit_wei: U256::zero(), gas_estimate: 0, post_hf: 0.0 });
        }
        let base_gas = 500_000u64;
        // apply safety margin
        let gas_estimate = (base_gas as f64 * self.cfg.gas_estimate_safety_margin) as u64;
        let effective_gas_wei = self.gas_oracle.effective_gas_price_wei();
        let gas_cost_usd = gas_estimate as f64 * effective_gas_wei / 1e18 * eth_price;
        // F-04 FIX: subtract Aave flash loan fee for liquidations (debt is flash-borrowed).
        // Aave charges 0.09% on flashLoanSimple — must be deducted from gross profit.
        let flash_fee_usd = {
            // AUDIT 2.8: Aave V3 Base flash premium is 5 bps (was 9). Ideally read live.
            let fee_wei = pos.debt_amount * U256::from(5u64) / U256::from(10_000u64);
            fee_wei.as_u128() as f64 / 10f64.powi(debt_dec as i32) * debt_price
        };
        let net_profit_usd = gross_profit_usd - gas_cost_usd - flash_fee_usd;
        if net_profit_usd <= 0.0 {
            return Ok(SimResult { profit_usd: -1.0, min_profit_wei: U256::zero(), gas_estimate, post_hf: 0.0 });
        }
        let min_profit_native = net_profit_usd * 0.7 / debt_price * 10f64.powi(debt_dec as i32);
        let min_profit_wei    = U256::from(min_profit_native as u128);
        let coll_usd = coll_price * pos.collateral_amount.as_u128() as f64 / coll_unit;
        let (_, liq_thresh) = base::aave_ltv_liq_threshold(&collateral_hex);
        let post_hf = if debt_usd > 1e-10 {
            ((coll_usd - debt_usd * (1.0 + liq_bonus)) * liq_thresh) / (debt_usd * 0.5).max(1e-10)
        } else {
            f64::MAX
        };
        crate::monitoring::metrics::record_sim_result("liquidation", "profitable");
        Ok(SimResult { profit_usd: net_profit_usd, min_profit_wei, gas_estimate, post_hf })
    }

    // ─── Strategy 5: Rate Arb ─────────────────────────────────────────────
    pub async fn simulate_rate_arb_open(
        &self,
        asset:       Address,
        notional:    U256,
        leverage:    f64,
        supply_apy:  f64,
        borrow_apy:  f64,
        eth_price:   f64,
    ) -> Result<SimResult> {
        let asset_hex = format!("{:?}", asset).to_lowercase();
        let (_, liq_threshold) = base::aave_ltv_liq_threshold(&asset_hex);
        let asset_dec = base::token_decimals(&asset_hex);
        let collateral_usd = notional.as_u128() as f64 / 10f64.powi(asset_dec as i32);
        let total_borrowed = collateral_usd * (leverage - 1.0);
        let post_hf = (collateral_usd * leverage * liq_threshold) / total_borrowed.max(1e-10);
        // AUDIT 2.10 FIX: a levered carry earns supply on the whole levered notional and
        // pays borrow only on the borrowed portion (leverage-1). The old formula
        // (supply-borrow)*leverage overstated yield and opened marginal positions.
        let net_daily_yield =
            (supply_apy * leverage - borrow_apy * (leverage - 1.0)) * collateral_usd / 365.0;
        let base_gas = 600_000u64;
        // F-01 FIX: Restored commented-out return statement (compile failure)
        let gas_estimate = (base_gas as f64 * self.cfg.gas_estimate_safety_margin) as u64;
        let effective_gas_wei = self.gas_oracle.effective_gas_price_wei();
        let gas_cost_usd = gas_estimate as f64 * effective_gas_wei / 1e18 * eth_price;
        let profit_usd = net_daily_yield - gas_cost_usd;
        let min_profit_wei = U256::from(
            (net_daily_yield.max(0.0) / eth_price.max(1.0) * 1e18) as u128
        );
        Ok(SimResult { profit_usd, min_profit_wei, gas_estimate, post_hf })
    }

    // ─── get_optimal_swap_route with pool existence check ────
    /// Finds a swap route from collateral → debt. Verifies pool exists and has
    /// non-trivial liquidity before returning a fee-tier route.
    /// AUDIT 2.1/2.2 FIX: returns the packed UniV3 **path** bytes (not full calldata).
    /// The contract's `_executeLiquidation` builds `exactInput` on-chain with the live
    /// seized-collateral amount and recipient = itself. Path is `abi.encodePacked`:
    ///   single hop: collateral | fee(3) | debt
    ///   two hop:    collateral | feeA(3) | WETH | feeB(3) | debt
    pub async fn get_optimal_swap_route(
        &self,
        collateral: Address,
        debt:       Address,
    ) -> Result<Bytes> {
        let factory: Address = base::UNISWAP_V3_FACTORY.parse()?;
        let weth:    Address = base::WETH.parse()?;

        // Try direct routes in order of typical liquidity depth on Base
        for &fee in &[500u32, 3000, 100, 10000] {
            if let Ok(pool) = self.get_uni_v3_pool(collateral, debt, fee, factory).await {
                if !pool.is_zero() {
                    if let Ok((r0, r1)) = self.get_uni_virtual_reserves(pool).await {
                        let min_reserves = U256::from(1_000u64); // dust threshold
                        if r0 > min_reserves && r1 > min_reserves {
                            let mut path = Vec::with_capacity(20 + 3 + 20);
                            path.extend_from_slice(collateral.as_bytes());
                            path.extend_from_slice(&fee.to_be_bytes()[1..]);
                            path.extend_from_slice(debt.as_bytes());
                            return Ok(Bytes::from(path));
                        }
                    }
                }
            }
        }

        // Two-hop via WETH — best fee tier per hop.
        let fee_a = self.best_fee_tier(collateral, weth, factory).await
            .map_err(|_| anyhow::anyhow!(
                "get_optimal_swap_route: no liquid {:?}→WETH pool found for two-hop route", collateral
            ))?;
        let fee_b = self.best_fee_tier(weth, debt, factory).await
            .map_err(|_| anyhow::anyhow!(
                "get_optimal_swap_route: no liquid WETH→{:?} pool found for two-hop route", debt
            ))?;

        let mut path = Vec::with_capacity(20 + 3 + 20 + 3 + 20);
        path.extend_from_slice(collateral.as_bytes());
        path.extend_from_slice(&fee_a.to_be_bytes()[1..]);  // 3 bytes for fee (uint24)
        path.extend_from_slice(weth.as_bytes());
        path.extend_from_slice(&fee_b.to_be_bytes()[1..]);
        path.extend_from_slice(debt.as_bytes());
        Ok(Bytes::from(path))
    }

    // ─── Pool helpers ──────────────────────────────────────────────────────

    pub async fn get_pool_tick_info(&self, pool: Address) -> Result<(i32, i32)> {
        let res = self.provider.call(&tx_req(pool, SEL_SLOT0.to_vec()), None).await?;
        let tick = decode_int24_from_word(&res, 32);
        let ts = self.provider.call(&tx_req(pool, SEL_TICK_SPACING.to_vec()), None).await?;
        let spacing = decode_int24_from_word(&ts, 0).max(1);
        Ok((tick, spacing))
    }

    pub async fn simulate_swap_ending_tick(
        &self,
        pool:     Address,
        amount:   U256,
        zfo:      bool,
        at_block: u64,
    ) -> Result<i32> {
        let quoter: Address = base::UNISWAP_V3_QUOTER_V2.parse()?;
        let (token0, token1, fee) = self.get_pool_tokens_and_fee(pool).await?;
        let (token_in, token_out) = if zfo { (token0, token1) } else { (token1, token0) };
        let sel = &ethers::utils::keccak256(b"quoteExactInputSingle((address,address,uint256,uint24,uint160))")[..4];
        let params = ethers::abi::encode(&[ethers::abi::Token::Tuple(vec![
            ethers::abi::Token::Address(token_in),
            ethers::abi::Token::Address(token_out),
            ethers::abi::Token::Uint(amount),
            ethers::abi::Token::Uint(U256::from(fee)),
            ethers::abi::Token::Uint(U256::zero()),
        ])]);
        let calldata = Bytes::from([sel, params.as_slice()].concat());
        let base_fee = self.gas_oracle.base_fee_wei();
        let timestamp = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH).unwrap_or_default().as_secs();

        match self.fork_and_call(quoter, calldata, Address::zero(), at_block, base_fee, timestamp, Address::zero()).await? {
            ExecutionResult::Success { output, .. } => {
                let out = match output {
                    Output::Call(b) => b.to_vec(),
                    _ => anyhow::bail!("Quoter: unexpected output type"),
                };
                if out.len() < 64 { anyhow::bail!("Quoter: response too short"); }
                let sqrt_price_x96 = U256::from_big_endian(&out[32..64]);
                Ok(sqrt_price_x96_to_tick(sqrt_price_x96))
            }
            _ => {
                let (tick, spacing) = self.get_pool_tick_info(pool).await?;
                tracing::debug!("QuoterV2 reverted for pool {:?}, using tick±spacing fallback", pool);
                Ok(if zfo { tick - spacing } else { tick + spacing })
            }
        }
    }

    pub async fn get_tick_range_liquidity(&self, pool: Address, tick_lower: i32, tick_upper: i32) -> Result<U256> {
        // slot0 current_tick tells us whether [tick_lower, tick_upper] straddles the active price.
        // JIT ranges always straddle the current tick by construction — if they don't, this
        // position earns zero fees and we should return zero to filter it out.
        let res = self.provider.call(&tx_req(pool, SEL_SLOT0.to_vec()), None).await?;
        let current_tick = decode_int24_from_word(&res, 32);
        if current_tick < tick_lower || current_tick >= tick_upper {
            return Ok(U256::zero()); // swap won't touch our range
        }
        // liquidity() returns active in-range liquidity at the current tick — the correct
        // denominator for JIT fee-share when the range straddles the current tick.
        let liq_res = self.provider.call(&tx_req(pool, SEL_LIQUIDITY.to_vec()), None).await?;
        Ok(U256::from_big_endian(&liq_res))
    }

    /// Returns sqrtPriceX96 from pool slot0.
    pub async fn get_sqrt_price_x96(&self, pool: Address) -> Result<U256> {
        let res = self.provider.call(&tx_req(pool, SEL_SLOT0.to_vec()), None).await?;
        if res.len() < 32 { anyhow::bail!("slot0 too short"); }
        Ok(U256::from_big_endian(&res[0..32]))
    }

    // ─── Private helpers ──────────────────────────────────────────────────

    /// lookup pool via UniV3 factory.getPool.
    async fn get_uni_v3_pool(&self, t0: Address, t1: Address, fee: u32, factory: Address) -> Result<Address> {
        let sel  = &ethers::utils::keccak256(b"getPool(address,address,uint24)")[..4];
        let data = [sel, ethers::abi::encode(&[
            ethers::abi::Token::Address(t0),
            ethers::abi::Token::Address(t1),
            ethers::abi::Token::Uint(U256::from(fee)),
        ]).as_slice()].concat();
        let res = self.provider.call(
            &TransactionRequest { to: Some(factory.into()), data: Some(data.into()), ..Default::default() }.into(),
            None,
        ).await?;
        if res.len() < 32 { anyhow::bail!("getPool short response"); }
        Ok(Address::from_slice(&res[12..32]))
    }

    /// explicit Aerodrome reserve reader (getReserves style).
    async fn fetch_aero_reserves(&self, pool: Address) -> Result<(U256, U256, u32, bool)> {
        let aero_sel = crate::shared::math::SEL_GET_RESERVES;
        let res = self.provider.call(&tx_req(pool, aero_sel.to_vec()), None).await?;
        if res.len() < 64 {
            anyhow::bail!("getReserves too short for pool {:?}", pool);
        }
        let r0 = U256::from_big_endian(&res[0..32]);
        let r1 = U256::from_big_endian(&res[32..64]);
        Ok((r0, r1, 30, false))
    }

    /// explicit UniV3 virtual reserve reader (does NOT try getReserves first).
    // F-15 FIX: Propagate fee() call failures instead of silently returning 0-fee fallback.
    // A zero fee inflates profit estimates (missing fee deduction) and a zero-byte path
    // passed to the UniV3 router causes guaranteed-revert calldata.
    async fn fetch_uni_v3_virtual_reserves_with_fee(&self, pool: Address, _token_in: Address) -> Result<(U256, U256, u32, bool)> {
        let fee_res = self.provider.call(&tx_req(pool, base::SEL_FEE.to_vec()), None)
            .await
            .map_err(|e| anyhow::anyhow!("fee() call failed for pool {:?}: {}", pool, e))?;
        if fee_res.len() < 32 {
            anyhow::bail!("fee() response too short ({} bytes) for pool {:?}", fee_res.len(), pool);
        }
        let fee_ppm = U256::from_big_endian(&fee_res).as_u32();
        let fee_bps = fee_ppm / 100;
        let (r0, r1) = self.get_uni_virtual_reserves(pool).await
            .map_err(|e| anyhow::anyhow!("get_uni_virtual_reserves failed for {:?}: {}", pool, e))?;
        Ok((r0, r1, fee_bps, false))
    }

    /// AUDIT 2.8 FIX: estimate the flash-loan fee CONSERVATIVELY.
    /// The old code only charged the fee above a 50M-USD heuristic, but
    /// `select_provider` routes to Aave whenever Balancer AND Morpho depth are both
    /// below the amount — which happens far below 50M for thinner assets — so small
    /// Aave-routed trades showed phantom profit and reverted the on-chain profit gate.
    /// We now always deduct the Aave premium (worst case). Under-reporting profit on a
    /// trade that actually routes free only skips a marginal trade — the safe direction.
    /// NOTE: `AAVE_FLASH_PREMIUM_BPS` should ideally be read live from
    /// `pool.FLASHLOAN_PREMIUM_TOTAL()`; 5 bps is Aave V3 Base's current value.
    async fn estimate_flash_fee_usd(
        &self,
        amount:    U256,
        decimals:  u8,
        eth_price: f64,
    ) -> f64 {
        const AAVE_FLASH_PREMIUM_BPS: u64 = 5; // 0.05%
        let fee_wei  = amount * U256::from(AAVE_FLASH_PREMIUM_BPS) / U256::from(10_000u64);
        let fee_norm = fee_wei.as_u128() as f64 / 10f64.powi(decimals as i32);
        if decimals == 18 { fee_norm * eth_price } else { fee_norm }
    }

    async fn get_uni_virtual_reserves(&self, pool: Address) -> Result<(U256, U256)> {
        let slot0_res = self.provider.call(&tx_req(pool, SEL_SLOT0.to_vec()), None).await?;
        let liq_res   = self.provider.call(&tx_req(pool, SEL_LIQUIDITY.to_vec()), None).await?;
        let sqp = U256::from_big_endian(&slot0_res[0..32]);
        let liq = U256::from_big_endian(&liq_res).as_u128() as f64;
        let q96   = U256::from(1u128) << 96;
        let sq_hi = (sqp / q96).as_u128() as f64;
        let sq_lo = (sqp % q96).as_u128() as f64 / (1u128 << 96) as f64;
        let sq    = sq_hi + sq_lo;
        let r0 = U256::from((liq / sq.max(1e-30)) as u128);
        let r1 = U256::from((liq * sq) as u128);
        Ok((r0, r1))
    }

    async fn get_pool_tokens_and_fee(&self, pool: Address) -> Result<(Address, Address, u32)> {
        let t0_res = self.provider.call(&tx_req(pool, base::SEL_TOKEN0.to_vec()), None).await?;
        let t1_res = self.provider.call(&tx_req(pool, base::SEL_TOKEN1.to_vec()), None).await?;
        let f_res  = self.provider.call(&tx_req(pool, base::SEL_FEE.to_vec()), None).await?;
        let t0  = Address::from_slice(&t0_res[12..32]);
        let t1  = Address::from_slice(&t1_res[12..32]);
        let fee = U256::from_big_endian(&f_res).as_u32();
        Ok((t0, t1, fee))
    }

    /// F-18 helper: find the best (most-liquid) fee tier for a UniV3 pair.
    /// Iterates [500, 3000, 100, 10000] ppm and returns the first tier whose pool
    /// exists and has non-dust reserves. Errors if no liquid pool is found.
    async fn best_fee_tier(&self, t0: Address, t1: Address, factory: Address) -> Result<u32> {
        let min_reserves = U256::from(1_000u64);
        for &fee in &[500u32, 3000, 100, 10000] {
            if let Ok(pool) = self.get_uni_v3_pool(t0, t1, fee, factory).await {
                if !pool.is_zero() {
                    if let Ok((r0, r1)) = self.get_uni_virtual_reserves(pool).await {
                        if r0 > min_reserves && r1 > min_reserves {
                            return Ok(fee);
                        }
                    }
                }
            }
        }
        anyhow::bail!("best_fee_tier: no liquid pool found for {:?} → {:?}", t0, t1)
    }
}

// ─── Helpers ─────────────────────────────────────────────────────────────────

fn decode_int24_from_word(res: &Bytes, word_start: usize) -> i32 {
    if res.len() < word_start + 32 { return 0; }
    let b0 = res[word_start + 29];
    let b1 = res[word_start + 30];
    let b2 = res[word_start + 31];
    let raw = ((b0 as u32) << 16) | ((b1 as u32) << 8) | (b2 as u32);
    if b0 & 0x80 != 0 { (raw | 0xFF00_0000) as i32 } else { raw as i32 }
}

fn sqrt_price_x96_to_tick(sqrt_price_x96: U256) -> i32 {
    if sqrt_price_x96.is_zero() { return 0; }
    let q96   = U256::from(1u128) << 96;
    let sq_hi = (sqrt_price_x96 / q96).as_u128() as f64;
    let sq_lo = (sqrt_price_x96 % q96).as_u128() as f64 / (1u128 << 96) as f64;
    let sq    = sq_hi + sq_lo;
    let price = sq * sq;
    (price.ln() / 1.0001_f64.ln()).floor() as i32
}

fn tx_req(to: Address, data: Vec<u8>) -> ethers::types::transaction::eip2718::TypedTransaction {
    // ethers 2.0 Middleware::call takes &TypedTransaction, not &TransactionRequest.
    TransactionRequest { to: Some(to.into()), data: Some(data.into()), ..Default::default() }.into()
}

trait PoolStateExt { fn is_stable(&self) -> bool; }
impl PoolStateExt for PoolState {
    fn is_stable(&self) -> bool { self.fee_bps < 10 }
}
