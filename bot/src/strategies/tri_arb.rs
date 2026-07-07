//! Strategy 2 — Tri-Arb (3-leg circular arbitrage).
//!
//! PHASE 4 (Accuracy):
//!   - MEDIUM: tri-arb optimal sizing now uses min of two 2-leg optima (was ignoring middle leg)
//!     Old code called optimal_arb_input_with_fees(p01, p20) — p12 entirely ignored.

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
        pool_discovery::{MONITORED_TRIANGLES, DexType},
        math::optimal_arb_input_with_fees,
        addresses::base,
    },
};

pub async fn run(
    pf:          PriceFeed,
    sim:         Arc<SimulationEngine>,
    flash:       Arc<FlashLoanRouter>,
    sub:         Arc<SubmissionPipeline>,
    cfg:         Config,
    mempool_mon: Arc<MempoolMonitor>,
) -> Result<()> {
    let eth_oracle: ethers::types::Address = base::CHAINLINK_ETH_USD.parse()?;
    let eth_price = mempool_mon.current_oracle_prices()
        .get(&eth_oracle)
        .map(|p| *p)
        .unwrap_or(cfg.eth_price_fallback_usd);

    for tri in MONITORED_TRIANGLES {
        let t = |i: usize| -> anyhow::Result<ethers::types::Address> { Ok(tri.tokens[i].parse()?) };
        let (t0, t1, t2) = (t(0)?, t(1)?, t(2)?);
        let dex = |i: usize| if tri.dex_types[i] == 0 { DexType::Aerodrome } else { DexType::UniswapV3 };

        let p01 = match pf.get_by_tokens(t0, t1, dex(0)) { Some(p) => p, None => continue };
        let p12 = match pf.get_by_tokens(t1, t2, dex(1)) { Some(p) => p, None => continue };
        let p20 = match pf.get_by_tokens(t2, t0, dex(2)) { Some(p) => p, None => continue };

        let ratio = p01.rate(t0) * p01.fee_factor()
            * p12.rate(t1) * p12.fee_factor()
            * p20.rate(t2) * p20.fee_factor();

        if ratio <= 1.0008 { continue; }

        // PHASE 4 FIX: 3-leg optimal sizing — use min of two 2-leg optima.
        // Old code: optimal_arb_input_with_fees(p01.reserve0, p20.reserve0, p01.fee_bps, p20.fee_bps, 20)
        //           → ignores middle leg p12 entirely, wrong when p12 is the binding constraint.
        //
        // Fix: compute the optimum for each adjacent pair of legs and take the minimum.
        // This is a conservative bound that ensures we don't exceed any single leg's depth.
        let opt_01_12 = optimal_arb_input_with_fees(p01.reserve0, p12.reserve0, p01.fee_bps, p12.fee_bps, 20);
        let opt_12_20 = optimal_arb_input_with_fees(p12.reserve0, p20.reserve0, p12.fee_bps, p20.fee_bps, 20);
        let amount    = opt_01_12.min(opt_12_20);

        if amount.is_zero() { continue; }

        let profit_token_hex = format!("{:?}", t0).to_lowercase();
        let profit_dec       = base::token_decimals(&profit_token_hex) as u8;

        let sim_r = sim.simulate_tri_arb(
            [t0, t1, t2],
            [p01.address, p12.address, p20.address],
            tri.dex_types,
            amount,
            profit_dec,
            eth_price,
        ).await?;
        if sim_r.profit_usd < cfg.min_profit_usd { continue; }

        tracing::info!(
            "TriArb {}: ratio={:.5} profit=${:.4}",
            tri.name, ratio, sim_r.profit_usd
        );

        // AUDIT 2.1/2.7: pass per-leg fee tier and stable flag from live pool state.
        // The contract builds swap calldata on-chain (no MAX amountIn, no wrong recipient).
        let fees_bps = [p01.fee_bps, p12.fee_bps, p20.fee_bps];
        let stables  = [
            p01.dex == DexType::Aerodrome && p01.fee_bps < 10,
            p12.dex == DexType::Aerodrome && p12.fee_bps < 10,
            p20.dex == DexType::Aerodrome && p20.fee_bps < 10,
        ];

        let calldata = flash.build_tri_arb(
            [t0, t1, t2],
            tri.dex_types,
            fees_bps,
            stables,
            amount,
            sim_r.min_profit_wei,
        ).await?;
        sub.submit(calldata, sim_r.gas_estimate).await?;
        crate::monitoring::metrics::record_trade("tri_arb", sim_r.profit_usd);
    }
    Ok(())
}
