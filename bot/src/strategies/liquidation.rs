//! Strategy 3 — Protocol Liquidation.
//!
//! v1.1 changes:
//!   - current_oracle_prices() now returns Arc<DashMap<Address, f64>> (zero-copy).
//!     run() and run_presign signatures updated accordingly.
//!
//! Earlier fixes:
//!   - simulate_liquidation now receives eth_price (was missing arg, compile error)
//!   - LIQUIDATION_HF_THRESHOLD moved to config (liquidation_hf_threshold)

use anyhow::Result;
use dashmap::DashMap;
use ethers::types::{Address, U256};
use std::sync::Arc;
use crate::{
    config::Config,
    shared::{
        addresses::base,
        position_indexer::{PositionIndexer, PriceMap},
        mempool_monitor::MempoolMonitor,
        simulation::SimulationEngine,
        flash_loan::FlashLoanRouter,
        submission::SubmissionPipeline,
    },
};

pub async fn run(
    indexer:   Arc<PositionIndexer>,
    mm:        Arc<MempoolMonitor>,
    sim:       Arc<SimulationEngine>,
    flash:     Arc<FlashLoanRouter>,
    sub:       Arc<SubmissionPipeline>,
    cfg:       Config,
    eth_price: f64,
) -> Result<()> {
    let prices = mm.current_oracle_prices();
    execute_liquidations(&indexer, &*prices, &sim, &flash, &sub, &cfg, eth_price).await
}

/// Mempool-triggered pre-signing using pending oracle prices.
pub async fn run_presign(
    indexer:        Arc<PositionIndexer>,
    current_prices: Arc<DashMap<Address, f64>>,
    pending_prices: PriceMap,
    sim:            Arc<SimulationEngine>,
    flash:          Arc<FlashLoanRouter>,
    sub:            Arc<SubmissionPipeline>,
    cfg:            Config,
    eth_price:      f64,
) -> Result<()> {
    let hf_thresh = cfg.liquidation_hf_threshold;
    let at_risk   = indexer.positions_below_pending_hf(&pending_prices, hf_thresh);
    if at_risk.is_empty() { return Ok(()); }

    tracing::info!("Pre-signing {} liquidations on pending oracle update", at_risk.len());
    // current_prices is Arc<DashMap> — deref to &DashMap which coerces to &PriceMap
    execute_liquidations_for_positions(&at_risk, &*current_prices, &sim, &flash, &sub, &cfg, eth_price).await
}

async fn execute_liquidations(
    indexer:   &Arc<PositionIndexer>,
    prices:    &PriceMap,
    sim:       &Arc<SimulationEngine>,
    flash:     &Arc<FlashLoanRouter>,
    sub:       &Arc<SubmissionPipeline>,
    cfg:       &Config,
    eth_price: f64,
) -> Result<()> {
    let hf_thresh = cfg.liquidation_hf_threshold;
    // AUDIT 2.3: refresh stored HF from live prices FIRST — otherwise every position
    // still carries its f64::MAX index-time default and nothing is ever below threshold.
    indexer.refresh_health_factors(prices);
    let positions = indexer.positions_below_hf(hf_thresh);
    execute_liquidations_for_positions(&positions, prices, sim, flash, sub, cfg, eth_price).await
}

async fn execute_liquidations_for_positions(
    positions: &[crate::shared::position_indexer::BorrowPosition],
    prices:    &PriceMap,
    sim:       &Arc<SimulationEngine>,
    flash:     &Arc<FlashLoanRouter>,
    sub:       &Arc<SubmissionPipeline>,
    cfg:       &Config,
    eth_price: f64,
) -> Result<()> {
    for pos in positions {
        // PHASE 1 fix: pass eth_price to simulate_liquidation
        let sim_r = sim.simulate_liquidation(pos, prices, eth_price).await?;
        if sim_r.profit_usd < cfg.min_liquidation_profit_usd { continue; }

        tracing::info!(
            "Liquidation: borrower={:?} protocol={:?} profit=${:.2}",
            pos.borrower, pos.protocol, sim_r.profit_usd
        );

        // PHASE 1 fix: get_optimal_swap_route is now implemented
        let swap_route = sim.get_optimal_swap_route(pos.collateral_asset, pos.debt_asset).await?;
        let provider   = flash.select_provider(pos.debt_asset, pos.debt_amount).await?;

        // FIX (BUG-7): compute min_profit_wei from the config USD floor so the contract
        // enforces a real profit gate. Previously U256::zero() was passed, letting
        // gas-negative transactions pass the require(profit >= minProfit) check.
        let min_profit_wei = {
            let min_usd = cfg.min_liquidation_profit_usd.max(0.0);
            // Convert USD to wei: min_usd / eth_price * 1e18, clamped to u128
            let wei_f = (min_usd / eth_price.max(1.0)) * 1e18;
            U256::from(wei_f.min(u128::MAX as f64) as u128)
        };
        let calldata = flash.build_liquidation(pos, provider, swap_route, min_profit_wei).await?;

        sub.submit_priority(calldata, sim_r.gas_estimate).await?;
        crate::monitoring::metrics::record_trade("liquidation", sim_r.profit_usd);
    }
    Ok(())
}
