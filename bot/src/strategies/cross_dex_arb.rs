//! Strategy 1 — Cross-DEX Arbitrage.
//! MED-01/PROFIT-01: uses fee-aware optimal_arb_input_with_fees; handles stable pools.

use anyhow::Result;
use std::sync::Arc;
use crate::{
    config::Config,
    shared::{
        price_feed::PriceFeed,
        simulation::SimulationEngine,
        flash_loan::FlashLoanRouter,
        submission::SubmissionPipeline,
        mempool_monitor::MempoolMonitor,
        pool_discovery::{MONITORED_PAIRS, DexType},
        math::optimal_arb_input_with_fees,
        addresses::base,
    },
};

// ETH price comes from live Chainlink oracle via MempoolMonitor.
// Hardcoded $3,000 caused the profit gate to become inaccurate when ETH price moved.

pub async fn run(
    pf:         PriceFeed,
    sim:        Arc<SimulationEngine>,
    flash:      Arc<FlashLoanRouter>,
    sub:        Arc<SubmissionPipeline>,
    cfg:        Config,
    mempool_mon: Arc<MempoolMonitor>,
) -> Result<()> {
    // live ETH price from Chainlink oracle via MempoolMonitor
    let eth_oracle: ethers::prelude::Address = base::CHAINLINK_ETH_USD.parse()?;
    let eth_price = mempool_mon.current_oracle_prices()
        .get(&eth_oracle)
        .map(|p| *p)
        .unwrap_or(cfg.eth_price_fallback_usd);

    for pair in MONITORED_PAIRS {
        let t0 = pair.token0.parse()?;
        let t1 = pair.token1.parse()?;

        let aero = match pf.get_by_tokens(t0, t1, DexType::Aerodrome) { Some(s) => s, None => continue };
        let uni  = match pf.get_by_tokens(t0, t1, DexType::UniswapV3) { Some(s) => s, None => continue };

        let ap = aero.price_t1_per_t0();
        let up = uni.price_t1_per_t0();
        if ap == 0.0 || up == 0.0 { continue; }

        let spread = ((ap - up).abs() / ap.min(up)) * 10_000.0;
        if spread < cfg.min_arb_spread_bps { continue; }

        let (buy, sell) = if ap < up { (&aero, &uni) } else { (&uni, &aero) };

        // MED-01/PROFIT-01: fee-aware sizing — accounts for actual pool fees on both legs.
        // Both args must be the reserve of the SAME token (the flash-borrowed token, token0)
        // in their respective pools.  The formula computes sqrt(r_buy_t0 × r_sell_t0) − r_buy_t0,
        // the amount that maximises profit at the given spread accounting for both pool fees.
        // Using sell.reserve1 (token1, a different asset) gave sqrt(USDC × WETH) — dimensionally
        // wrong, producing a borrow size that could be 10× too large or 5× too small.
        let amount = optimal_arb_input_with_fees(
            buy.reserve0,
            sell.reserve0,  // FIX: same token (token0) in the sell pool, not reserve1
            buy.fee_bps,    // PROFIT-02: live fee from pool discovery
            sell.fee_bps,
            30,             // max 30% of reserves
        );
        if amount.is_zero() { continue; }

        // Determine profit token decimals for NEW-CRIT-04
        let profit_token_decimals: u8 = {
            let token_hex = format!("{:?}", buy.token0).to_lowercase();
            base::token_decimals(&token_hex) as u8
        };

        let sim_r = sim.simulate_cross_dex_arb(amount, buy, sell, profit_token_decimals, eth_price).await?;
        if sim_r.profit_usd < cfg.min_profit_usd { continue; }

        tracing::info!(
            "CrossDexArb {}: spread={:.1}bps profit=${:.4} gas_est={}",
            pair.name, spread, sim_r.profit_usd, sim_r.gas_estimate
        );

        crate::monitoring::metrics::record_sim_result("cross_dex_arb", "submitted");
        let calldata = flash.build_cross_dex_arb(amount, buy, sell, sim_r.min_profit_wei).await?;
        sub.submit(calldata, sim_r.gas_estimate).await?;
        crate::monitoring::metrics::record_trade("cross_dex_arb", sim_r.profit_usd);
    }
    Ok(())
}
