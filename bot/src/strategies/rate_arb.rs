//! Rate-arb strategy (S5).
//!
//! v1.1 changes:
//!   - Minimum hold period enforced before unwind (rate_arb_min_hold_blocks).
//!     Emergency unwind (HF critical) bypasses the hold floor.
//!   - Live debt fetched via variableDebtToken.balanceOf before building close calldata.
//!     0.5% buffer added to cover accrued interest in flight.
//!
//! Earlier fixes:
//!
//! COMPILE: `run()` and `scan_for_new_positions()` function signatures were
//!          corrupted — the opening-brace body comment was accidentally inlined
//!          into the parameter list, breaking the parse. Fixed.
//!
//! COMPILE: `RateArbPosition { ... }` struct literal was missing its closing `};`
//!          and the `hf_monitor.register_position` call. Fixed.
//!
//! BUG-6 (CRITICAL): cbBTC notional was computed using eth_price (~$3K) as a BTC
//!          price proxy. BTC is ~$100K — a 33× underestimate. cbBTC was always
//!          below the $50K min-notional threshold. Now fetches btc_price from the
//!          Chainlink cbBTC/USD feed via mempool_monitor.current_oracle_prices().
//!
//! BUG-8 (HIGH): Circuit breaker reset consecutive_successes to 0 on any single
//!          success, so a flaky strategy (24 reverts, 1 success, 24 reverts) never
//!          tripped. Now uses a 3-consecutive-success threshold before resetting.
//!
//! BUG-5 (CRITICAL): build_rate_arb_open now receives distinct supply_proto and
//!          borrow_proto from RateFeed instead of both being aave_pool.

use anyhow::Result;
use ethers::prelude::*;
use serde::{Deserialize, Serialize};
use std::sync::Arc;
use crate::{
    config::Config,
    shared::{
        simulation::SimulationEngine,
        flash_loan::FlashLoanRouter,
        submission::SubmissionPipeline,
        health_factor::HealthFactorMonitor,
        rate_feed::RateFeed,
        pool_discovery::{USDC, WETH, cbETH, wstETH, cbBTC},
        addresses::base,
        position_store,
    },
};

/// Serde-able so positions survive restarts.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RateArbPosition {
    pub asset:           Address,
    pub supply_proto:    Address,
    pub borrow_proto:    Address,
    pub notional_usd:    f64,
    pub leverage:        f64,
    pub net_apy:         f64,
    pub health_factor:   f64,
    pub opened_block:    u64,
    pub opened_ts:       u64,
    pub pnl_usd:         f64,
    /// Actual debt in wei — used by build_rate_arb_close flash loan amount.
    pub debt_amount_wei: U256,
}

/// eth_price and btc_price are live parameters passed from main.rs.
// COMPILE FIX: function signature was split across a comment that accidentally
// consumed the opening brace, making the body invisible to the compiler.
pub async fn run(
    rate_feed:     Arc<RateFeed>,
    hf_monitor:    Arc<HealthFactorMonitor>,
    sim:           Arc<SimulationEngine>,
    flash:         Arc<FlashLoanRouter>,
    sub:           Arc<SubmissionPipeline>,
    cfg:           Config,
    current_block: u64,
    eth_price:     f64,
    btc_price:     f64,   // FIX (BUG-6): added; was missing, forcing cbBTC to use eth_price
) -> Result<()> {
    // All strategies run unconditionally — no phase check needed.

    let executor: Address = cfg.flash_executor_address.parse()?;
    if let Err(e) = hf_monitor.refresh_hf_from_chain(executor).await {
        tracing::warn!("HF refresh failed: {} — using stale HF values", e);
    }

    check_and_unwind_positions(&hf_monitor, &flash, &sub, &rate_feed, &cfg, current_block, &cfg.position_store_path).await?;
    scan_for_new_positions(&rate_feed, &sim, &flash, &sub, &hf_monitor, &cfg, current_block, eth_price, btc_price).await?;
    Ok(())
}

async fn check_and_unwind_positions(
    hf_monitor:    &Arc<HealthFactorMonitor>,
    flash:         &Arc<FlashLoanRouter>,
    sub:           &Arc<SubmissionPipeline>,
    rate_feed:     &Arc<RateFeed>,
    cfg:           &Config,
    current_block: u64,
    store_path:    &str,
) -> Result<()> {
    let provider = flash.provider();

    let close_spread_bps = cfg.rate_arb_close_spread_bps;
    let close_hf         = cfg.rate_arb_close_hf;
    let emergency_hf     = cfg.rate_arb_emergency_hf;
    let max_hold_days    = cfg.rate_arb_max_hold_days;
    let min_hold_blocks  = cfg.rate_arb_min_hold_blocks;

    let mut did_close = false;
    for pos in hf_monitor.get_rate_arb_positions() {
        let spread_bps  = rate_feed.net_spread_bps(pos.asset);
        let hold_blocks = current_block.saturating_sub(pos.opened_block);
        let hold_days   = hold_blocks as f64 * 2.0 / 86_400.0;

        // Emergency unwind ignores minimum hold (HF critical — act immediately).
        let force_unwind = pos.health_factor < emergency_hf;
        let past_minimum = hold_blocks >= min_hold_blocks;

        let should_unwind = force_unwind || (past_minimum && (
            pos.health_factor < close_hf
            || spread_bps      < close_spread_bps
            || hold_days       > max_hold_days
        ));

        if should_unwind {
            let reason = if force_unwind                       { "EMERGENCY: HF critical" }
                         else if pos.health_factor < close_hf  { "HF below threshold" }
                         else if spread_bps < close_spread_bps { "Spread compressed" }
                         else                                   { "Max hold exceeded" };
            tracing::warn!(
                "RateArb unwind: asset={:?} reason={} HF={:.3} spread={:.1}bps hold_blocks={} pnl=${:.2}",
                pos.asset, reason, pos.health_factor, spread_bps, hold_blocks, pos.pnl_usd
            );

            // Fetch live debt from Aave variableDebtToken to avoid stale amount revert.
            // Aave pool + flash executor come from config/addresses — single source of truth.
            let aave_pool: Address = base::AAVE_V3_POOL_PROXY.parse().unwrap_or_default();
            let flash_executor: Address = cfg.flash_executor_address.parse().unwrap_or_default();
            let live_debt = fetch_aave_variable_debt(pos.asset, aave_pool, flash_executor, &provider)
                .await
                .unwrap_or(pos.debt_amount_wei); // fallback to stored if RPC fails
            // 0.5% buffer for interest accrued between fetch and tx inclusion
            let buffered_debt = live_debt * U256::from(1005u64) / U256::from(1000u64);

            let mut pos_close = pos.clone();
            pos_close.debt_amount_wei = buffered_debt;

            let calldata = flash.build_rate_arb_close(&pos_close).await?;
            if force_unwind {
                sub.submit_priority(calldata, 700_000).await?;
            } else {
                sub.submit(calldata, 700_000).await?;
            }
            crate::monitoring::metrics::record_trade("rate_arb_close", pos.pnl_usd);
            hf_monitor.remove_position(pos.asset);
            crate::monitoring::metrics::record_position_close("rate_arb");
            did_close = true;
        }
    }

    if did_close {
        let positions = hf_monitor.get_rate_arb_positions();
        if let Err(e) = position_store::save(store_path, &positions) {
            tracing::warn!("Failed to persist positions after close: {}", e);
        }
    }

    let min_hf = hf_monitor.get_rate_arb_positions()
        .iter().map(|p| p.health_factor).fold(f64::MAX, f64::min);
    crate::monitoring::metrics::alert_hf(min_hf);
    Ok(())
}

// COMPILE FIX: function signature was corrupted in the same way as `run()`.
async fn scan_for_new_positions(
    rate_feed:     &Arc<RateFeed>,
    sim:           &Arc<SimulationEngine>,
    flash:         &Arc<FlashLoanRouter>,
    sub:           &Arc<SubmissionPipeline>,
    hf_monitor:    &Arc<HealthFactorMonitor>,
    cfg:           &Config,
    current_block: u64,
    eth_price:     f64,
    btc_price:     f64,   // FIX (BUG-6): distinct BTC price
) -> Result<()> {
    let open_assets: std::collections::HashSet<Address> =
        hf_monitor.get_rate_arb_positions().iter().map(|p| p.asset).collect();

    // cbBTC gets its own entry with a min_notional_usd guard.
    let assets_to_check: &[(&str, u128, &str, Option<f64>)] = &[
        (USDC,   cfg.rate_arb_max_notional_usdc, "USDC",   None),
        (WETH,   cfg.rate_arb_max_notional_weth, "WETH",   None),
        (cbETH,  cfg.rate_arb_max_notional_weth, "cbETH",  None),
        (wstETH, cfg.rate_arb_max_notional_weth, "wstETH", None),
        (cbBTC,  cfg.rate_arb_max_notional_weth / 30, "cbBTC", Some(cfg.rate_arb_cbbtc_min_notional_usd)),
    ];

    for (asset_str, max_notional_raw, asset_name, min_notional_usd_opt) in assets_to_check {
        let asset: Address = asset_str.parse()?;
        if open_assets.contains(&asset) { continue; }

        let spread_bps = rate_feed.net_spread_bps(asset);
        if spread_bps < cfg.min_rate_spread_bps {
            tracing::debug!("RateArb {}: spread={:.1}bps < {:.1}bps, skipping", asset_name, spread_bps, cfg.min_rate_spread_bps);
            continue;
        }

        let (supply_apy, borrow_apy) = rate_feed.get_best_spread(asset);
        // FIX (BUG-5): fetch distinct supply_proto and borrow_proto from RateFeed.
        let (supply_proto, borrow_proto) = rate_feed.get_best_proto_addresses(asset);

        // AUDIT 2.5: the open/close calldata builders only emit Aave-shaped supply/
        // borrow/repay/withdraw calls. Routing those to Morpho Blue (different ABI)
        // reverts. Until a native Morpho carry path exists, skip any leg that resolves
        // to a non-Aave protocol rather than submitting a guaranteed-revert tx.
        let aave_pool: Address = base::AAVE_V3_POOL_PROXY.parse()?;
        if supply_proto != aave_pool || borrow_proto != aave_pool {
            tracing::debug!(
                "RateArb {}: best route uses a non-Aave protocol (supply={:?} borrow={:?}) — \
                 Morpho carry not yet implemented, skipping",
                asset_name, supply_proto, borrow_proto
            );
            continue;
        }
        if !cfg.rate_arb_open_enabled {
            tracing::debug!("RateArb {}: opening disabled (rate_arb_open_enabled=false)", asset_name);
            continue;
        }

        let available_depth = flash.get_provider_depth(asset).await.unwrap_or(U256::zero());
        let notional = (available_depth * U256::from(40u64) / U256::from(100u64)).min(U256::from(*max_notional_raw));
        if notional.is_zero() { continue; }

        if let Some(min_notional) = min_notional_usd_opt {
            let asset_hex = format!("{:?}", asset).to_lowercase();
            let dec = base::token_decimals(&asset_hex);
            // FIX (BUG-6): use btc_price for cbBTC, not eth_price.
            // Old code used eth_price (~$3K) as a proxy for BTC (~$100K) — a 33×
            // underestimate that made every cbBTC position appear below the $50K floor.
            let token_price = if asset_name == &"cbBTC" { btc_price } else { eth_price };
            let notional_usd = notional.as_u128() as f64 / 10f64.powi(dec as i32) * token_price;
            if notional_usd < *min_notional {
                tracing::debug!(
                    "RateArb {}: notional ${:.0} < min ${:.0}, skipping (not worth gas)",
                    asset_name, notional_usd, min_notional
                );
                continue;
            }
        }

        let asset_hex = format!("{:?}", asset).to_lowercase();
        let (_, liq_thresh) = crate::shared::addresses::base::aave_ltv_liq_threshold(&asset_hex);
        let target_hf = 1.30f64;
        let max_safe_lev = if liq_thresh > 1.0 / target_hf {
            liq_thresh / (liq_thresh - 1.0 / target_hf)
        } else {
            cfg.rate_arb_base_leverage
        };
        // AUDIT 2.10 FIX: apply the safety cap LAST so the 1.5 floor can never push
        // leverage back above max_safe_lev (which would violate the HF target).
        let cap = max_safe_lev.min(4.0).max(1.0);
        let dynamic_leverage = (cfg.rate_arb_base_leverage + spread_bps / 100.0)
            .max(1.5_f64.min(cap))
            .min(cap);

        let sim_r = sim.simulate_rate_arb_open(
            asset, notional, dynamic_leverage, supply_apy, borrow_apy, eth_price
        ).await?;

        if sim_r.post_hf < 1.30 {
            tracing::debug!("RateArb {}: post_hf={:.3} below 1.30, skipping", asset_name, sim_r.post_hf);
            continue;
        }
        if sim_r.profit_usd < 0.0 {
            tracing::debug!("RateArb {}: breakeven exceeds {} days, skipping", asset_name, cfg.rate_arb_max_breakeven_days);
            continue;
        }

        tracing::info!(
            "RateArb open: {} spread={:.1}bps leverage={:.2}x HF={:.3} daily=${:.2}",
            asset_name, spread_bps, dynamic_leverage, sim_r.post_hf, sim_r.profit_usd
        );

        // AUDIT 2.10 FIX: the open calldata supplies `notional` and borrows `notional`
        // in total (step_notional × loops each side), so the real Aave debt is `notional`,
        // not notional*(lev-1). Track the actual borrowed amount so close repays correctly.
        let debt_amount_wei = notional;

        // FIX (BUG-5): pass supply_proto and borrow_proto to build_rate_arb_open.
        let calldata = flash.build_rate_arb_open(asset, notional, dynamic_leverage, supply_proto, borrow_proto).await?;
        sub.submit(calldata, sim_r.gas_estimate).await?;

        // COMPILE FIX: struct literal was missing its closing `};` and the
        // register_position call was dangling outside the struct.
        let pos = RateArbPosition {
            asset, supply_proto, borrow_proto,
            notional_usd:  notional.as_u128() as f64,
            leverage:      dynamic_leverage,
            net_apy:       supply_apy - borrow_apy,
            health_factor: sim_r.post_hf,
            opened_block:  current_block,
            opened_ts:     std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap_or_default().as_secs(),
            pnl_usd:         0.0,
            debt_amount_wei,
        };

        hf_monitor.register_position(pos.clone());
        crate::monitoring::metrics::record_position_open("rate_arb");
        crate::monitoring::metrics::record_trade("rate_arb_open", sim_r.profit_usd);

        let positions = hf_monitor.get_rate_arb_positions();
        if let Err(e) = position_store::save(&cfg.position_store_path, &positions) {
            tracing::warn!("Failed to persist positions after open: {}", e);
        }
    }
    Ok(())
}

// ─── Live debt fetch ──────────────────────────────────────────────────────────
//
// Aave V3: getReserveData(asset) returns a ReserveData struct. Field 10 (zero-indexed)
// is the variableDebtTokenAddress. We then call balanceOf(user) on that token —
// this includes all accrued interest since position open.
async fn fetch_aave_variable_debt(
    asset:          Address,
    pool:           Address,
    flash_executor: Address,
    provider:       &Arc<Provider<Ipc>>,
) -> Result<U256> {
    let sel = &ethers::utils::keccak256(b"getReserveData(address)")[..4];
    let call_data = [
        sel,
        ethers::abi::encode(&[ethers::abi::Token::Address(asset)]).as_slice(),
    ].concat();
    let res = provider.call(
        &TransactionRequest {
            to:   Some(pool.into()),
            data: Some(call_data.into()),
            ..Default::default()
        }.into(),
        None,
    ).await?;
    // ReserveData field layout (each 32 bytes): configuration(0), liquidityIndex(1),
    // currentLiquidityRate(2), variableBorrowIndex(3), currentVariableBorrowRate(4),
    // currentStableBorrowRate(5), lastUpdateTimestamp(6), id(7), aTokenAddress(8),
    // stableDebtTokenAddress(9), variableDebtTokenAddress(10)
    let offset = 10 * 32;
    if res.len() < offset + 32 {
        anyhow::bail!("getReserveData response too short for variableDebtToken");
    }
    let vdt_address = Address::from_slice(&res[offset + 12..offset + 32]);

    // balanceOf(flashExecutor) on variableDebtToken returns debt including accrued interest
    let bal_sel  = &ethers::utils::keccak256(b"balanceOf(address)")[..4];
    let bal_data = [
        bal_sel,
        ethers::abi::encode(&[ethers::abi::Token::Address(flash_executor)]).as_slice(),
    ].concat();
    let bal_res = provider.call(
        &TransactionRequest {
            to:   Some(vdt_address.into()),
            data: Some(bal_data.into()),
            ..Default::default()
        }.into(),
        None,
    ).await?;
    if bal_res.len() < 32 {
        anyhow::bail!("balanceOf response too short");
    }
    Ok(U256::from_big_endian(&bal_res[0..32]))
}
