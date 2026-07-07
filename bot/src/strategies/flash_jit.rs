//!
//! v1.1 changes:
//!   - Gas cost now computed from live GasOracle instead of hardcoded $0.30.
//!   - `run_on_pending` and `run` accept gas_oracle + eth_price parameters.
//!   - `run()` (block-loop entry) removed — S4 runs only in the mempool fast-loop.
//!
//! JIT profit estimate now accounts for in-range liquidity dilution.
//!             Old code assumed 100% fee capture. Correct formula:
//!             fee_capture = our_liq / (existing_in_range_liq + our_liq)
//! LP amount split computed via sqrtPriceX96 math in build_flash_jit.
//! Circuit breaker wired in (counter updated in main.rs).

use anyhow::Result;
use std::sync::Arc;
use crate::{
    config::Config,
    shared::{
        mempool_monitor::{MempoolMonitor, PendingSwap},
        simulation::SimulationEngine,
        flash_loan::FlashLoanRouter,
        submission::SubmissionPipeline,
        gas_oracle::GasOracle,
        math::{floor_tick, ceil_tick},
    },
};

pub async fn run_on_pending(
    swap:        PendingSwap,
    sim:         Arc<SimulationEngine>,
    flash:       Arc<FlashLoanRouter>,
    sub:         Arc<SubmissionPipeline>,
    cfg:         Config,
    gas_oracle:  Arc<GasOracle>,
    eth_price:   f64,
    at_block:    u64,
) -> Result<()> {
    if swap.amount_usd < cfg.jit_min_swap_usd * 10.0 { return Ok(()); }
    if swap.pool.is_zero() {
        tracing::debug!("JIT: pool address zero for {:?} — skipping", swap.tx_hash);
        return Ok(());
    }

    let ending_tick = sim.simulate_swap_ending_tick(swap.pool, swap.amount_in, swap.zero_for_one, at_block).await?;
    let (current_tick, tick_spacing) = sim.get_pool_tick_info(swap.pool).await?;

    let (tick_lower, tick_upper) = if swap.zero_for_one {
        (floor_tick(ending_tick, tick_spacing), ceil_tick(current_tick, tick_spacing))
    } else {
        (floor_tick(current_tick, tick_spacing), ceil_tick(ending_tick, tick_spacing))
    };

    if tick_lower >= tick_upper {
        anyhow::bail!("JIT: degenerate tick range [{}, {}]", tick_lower, tick_upper);
    }

    // fetch in-range liquidity for dilution-adjusted fee estimate
    let in_range_liq = sim.get_tick_range_liquidity(swap.pool, tick_lower, tick_upper).await
        .unwrap_or_default();

    let fee_tier_pct = swap.fee as f64 / 1_000_000.0;

    // Approximate our liquidity contribution from the flash loan amount
    // L_our ≈ amount_usd / (2 * fee_tier * price) — simplified heuristic
    // Use 1% of swap amount as our liquidity proxy
    let our_liq_proxy = swap.amount_usd * 0.01;
    let existing_liq = in_range_liq.as_u128() as f64;

    // diluted fee share = our_liq / (existing + our_liq)
    let fee_share = if existing_liq + our_liq_proxy > 0.0 {
        our_liq_proxy / (existing_liq + our_liq_proxy)
    } else {
        1.0 // empty pool — capture everything (rare)
    };

    let estimated_fees    = swap.amount_usd * fee_tier_pct * fee_share;
    let flash_loan_cost   = 0.0; // Balancer V2: free
    // Live gas cost: JIT executes ~580K gas (mint + swap + decreaseLiquidity + collect + burn)
    let gas_estimate      = (580_000f64 * cfg.gas_estimate_safety_margin) as u64;
    let gas_cost_usd      = gas_estimate as f64
        * gas_oracle.effective_gas_price_wei() as f64 / 1e18
        * eth_price;
    let estimated_profit  = estimated_fees - flash_loan_cost - gas_cost_usd;

    tracing::debug!(
        "JIT: pool={:?} swap=${:.0} fee_tier={:.3}% fee_share={:.2}% est_profit=${:.4}",
        swap.pool, swap.amount_usd, fee_tier_pct * 100.0, fee_share * 100.0, estimated_profit
    );

    if estimated_profit < cfg.min_jit_profit_usd {
        tracing::debug!("JIT: profit ${:.4} below threshold ${}", estimated_profit, cfg.min_jit_profit_usd);
        return Ok(());
    }

    tracing::info!(
        "JIT opportunity: pool={:?} swap=${:.0} fee_share={:.1}% est_profit=${:.2} ticks=[{},{}]",
        swap.pool, swap.amount_usd, fee_share * 100.0, estimated_profit, tick_lower, tick_upper
    );

    // pass sim so build_flash_jit can compute correct LP amounts
    let calldata = flash.build_flash_jit(
        &sim,
        swap.pool, swap.token_in, swap.token_out, swap.fee,
        tick_lower, tick_upper,
        swap.amount_in, estimated_profit as u64,
        swap.zero_for_one,
    ).await?;

    sub.submit_jit_bundle(calldata, swap.raw_tx, swap.block_number + 1).await?;
    crate::monitoring::metrics::record_trade("flash_jit", estimated_profit);
    Ok(())
}
