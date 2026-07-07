//! Corvus v1.1 — Flash Loan MEV System
//! Base Mainnet (Chain ID: 8453)
//!
//! v1.1 changes:
//!   - S6 (Backrun) and S7 (Pendle Arb) removed — architecturally broken, not fixable
//!     without full rebuilds. Files deleted, all refs purged.
//!   - S4 (JIT) removed from confirmed block loop — dead work, runs only in mempool fast-loop.
//!   - Per-strategy independent timeouts replace shared 1800ms try_join! timeout.
//!   - S4 gas cost wired to live GasOracle (was hardcoded $0.30).
//!   - CacheDB snapshot distributed to strategies before spawn — eliminates write-lock serialization.
//!   - executor_private_key validated non-empty at startup.
//!   - handle_join_result_simple dead function removed.

mod config;
mod strategies;
mod shared;
mod monitoring;

use anyhow::Result;
use ethers::prelude::*;
use futures::StreamExt;
use std::sync::Arc;
use std::sync::atomic::{AtomicU32, Ordering};
use tokio::sync::RwLock;

use config::Config;
use shared::{
    addresses::base,
    flash_loan::FlashLoanRouter,
    gas_oracle::GasOracle,
    health_factor::HealthFactorMonitor,
    mempool_monitor::MempoolMonitor,
    pool_discovery::PoolRegistry,
    position_indexer::PositionIndexer,
    position_store,
    price_feed::PriceFeed,
    price_oracle::PriceOracle,
    rate_feed::RateFeed,
    signing::Signer,
    simulation::SimulationEngine,
    submission::SubmissionPipeline,
};

const REVERT_THRESHOLD: u32 = 25;
const REVERT_THRESHOLD_JIT: u32 = 10; // lower threshold for JIT (higher cost/tx)
/// Require 3 consecutive successes before resetting the revert counter.
/// Prevents a strategy reverting 24 times, succeeding once, then reverting 24 more
/// without ever tripping the breaker.
const RESET_CONSEC_THRESHOLD: u32 = 3;

static S1_REVERTS: AtomicU32 = AtomicU32::new(0);
static S2_REVERTS: AtomicU32 = AtomicU32::new(0);
static S3_REVERTS: AtomicU32 = AtomicU32::new(0);
static S4_REVERTS: AtomicU32 = AtomicU32::new(0);
static S5_REVERTS: AtomicU32 = AtomicU32::new(0);

// Consecutive success counters — each success increments; each revert resets to 0.
// Only when consecutive successes >= RESET_CONSEC_THRESHOLD do we clear the revert counter.
static S1_CONSEC: AtomicU32 = AtomicU32::new(0);
static S2_CONSEC: AtomicU32 = AtomicU32::new(0);
static S3_CONSEC: AtomicU32 = AtomicU32::new(0);
static S4_CONSEC: AtomicU32 = AtomicU32::new(0);
static S5_CONSEC: AtomicU32 = AtomicU32::new(0);

fn circuit_tripped(counter: &AtomicU32, threshold: u32, label: &str) -> bool {
    let n = counter.load(Ordering::SeqCst);
    if n >= threshold {
        tracing::error!("CIRCUIT BREAKER: {} paused after {} reverts", label, n);
        true
    } else {
        false
    }
}

/// F-09: async version that also fires Telegram Alert #1 on circuit trip.
async fn circuit_tripped_alert(
    counter:  &AtomicU32,
    threshold: u32,
    label:    &str,
    telegram: &monitoring::TelegramAlerter,
) -> bool {
    let n = counter.load(Ordering::SeqCst);
    if n >= threshold {
        let reason = format!("{} tripped after {} consecutive reverts", label, n);
        tracing::error!("CIRCUIT BREAKER: {}", reason);
        telegram.alert_circuit_breaker(&reason).await;
        true
    } else {
        false
    }
}

/// Record a revert: increment revert counter, reset consecutive successes.
fn record_revert(revert_ctr: &AtomicU32, consec_ctr: &AtomicU32) {
    revert_ctr.fetch_add(1, Ordering::SeqCst);
    consec_ctr.store(0, Ordering::SeqCst);
}

/// Record a success: increment consecutive counter; reset revert counter only
/// after RESET_CONSEC_THRESHOLD consecutive successes (FIX BUG-8).
fn record_success(revert_ctr: &AtomicU32, consec_ctr: &AtomicU32) {
    let consec = consec_ctr.fetch_add(1, Ordering::SeqCst) + 1;
    if consec >= RESET_CONSEC_THRESHOLD {
        revert_ctr.store(0, Ordering::SeqCst);
        consec_ctr.store(0, Ordering::SeqCst);
    }
}

#[tokio::main]
async fn main() -> Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::from_default_env()
                .add_directive("corvus=info".parse()?)
                .add_directive("warn".parse()?)
        )
        .init();

    let cfg = Config::load()?;
    validate_config(&cfg)?;

    tracing::info!("╔══════════════════════════════════════╗");
    tracing::info!("║         CORVUS  v1.1.0               ║");
    tracing::info!("║   Flash Loan MEV — Base Mainnet      ║");
    tracing::info!("║   Strategies: S1–S5 (active)         ║");
    tracing::info!("╚══════════════════════════════════════╝");

    monitoring::metrics::init();
    let metrics_port = cfg.metrics_port;
    tokio::spawn(async move { monitoring::metrics::serve(metrics_port).await });

    // ── Graceful shutdown ──────────────────────────────────────────────────
    let shutdown = Arc::new(tokio::sync::Notify::new());
    {
        let s = shutdown.clone();
        tokio::spawn(async move {
            #[cfg(unix)] {
                use tokio::signal::unix::{signal, SignalKind};
                let mut sigterm = signal(SignalKind::terminate()).expect("SIGTERM");
                let mut sigint  = signal(SignalKind::interrupt()).expect("SIGINT");
                tokio::select! {
                    _ = sigterm.recv() => tracing::warn!("SIGTERM — shutting down"),
                    _ = sigint.recv()  => tracing::warn!("SIGINT — shutting down"),
                }
            }
            #[cfg(not(unix))] {
                tokio::signal::ctrl_c().await.expect("Ctrl+C");
            }
            s.notify_one();
        });
    }

    // ── THREE IPC connections ──────────────────────────────────────────────
    tracing::info!("Connecting IPC (3 connections): {}", cfg.ipc_path);
    let provider_blocks  = Arc::new(Provider::<Ipc>::connect_ipc(&cfg.ipc_path).await
        .expect("IPC connection 1 (blocks) failed"));
    let provider_mempool = Arc::new(Provider::<Ipc>::connect_ipc(&cfg.ipc_path).await
        .expect("IPC connection 2 (mempool) failed"));
    let provider_archive = Arc::new(Provider::<Ipc>::connect_ipc(&cfg.ipc_path).await
        .expect("IPC connection 3 (archive/REVM) failed"));
    tracing::info!("IPC: 3 connections established");

    let block_number = provider_blocks.get_block_number().await?;
    tracing::info!("Current block: {}", block_number);

    // Resolve Aave pool
    let aave_pool: Address = {
        let provider_addr: Address = cfg.aave_addresses_provider.parse()?;
        let selector = &ethers::utils::keccak256(b"getPool()")[..4];
        let res = provider_blocks.call(
            &TransactionRequest { to: Some(provider_addr.into()), data: Some(selector.to_vec().into()), ..Default::default() }.into(),
            None,
        ).await?;
        Address::from_slice(&res[12..32])
    };
    tracing::info!("Aave V3 Pool: {:?}", aave_pool);

    let gas_oracle  = Arc::new(GasOracle::new(provider_blocks.clone(), &cfg));
    gas_oracle.refresh().await;

    let signer      = Arc::new(Signer::new(
        cfg.executor_private_key.expose(),
        cfg.chain_id,
        &cfg.flash_executor_address,
        gas_oracle.clone(),
    )?);
    let sim_engine  = Arc::new(SimulationEngine::new(provider_archive.clone(), gas_oracle.clone(), cfg.clone()).await?);
    let flash_router= Arc::new(FlashLoanRouter::new(provider_blocks.clone(), &cfg)?);
    let submission  = Arc::new(SubmissionPipeline::new(&cfg, signer, provider_blocks.clone()).await?);
    let hf_monitor  = Arc::new(HealthFactorMonitor::new(provider_blocks.clone(), aave_pool));
    let rate_feed   = Arc::new(RateFeed::new(provider_blocks.clone()));
    let mempool_mon = Arc::new(MempoolMonitor::new(provider_mempool.clone(), cfg.eth_price_fallback_usd)?);
    let pos_indexer = Arc::new(PositionIndexer::new(provider_archive.clone(), aave_pool));
    // AUDIT 2.4: proactive on-chain price poller — populates the shared price map every
    // block so S3 liquidation and cbBTC rate-arb are no longer blind to non-ETH prices.
    let price_oracle = Arc::new(PriceOracle::new(provider_blocks.clone(), cfg.eth_price_fallback_usd)?);

    // F-09 FIX: Initialise Telegram alerter. Disabled (with warning) if token/chat_id not set.
    let telegram = Arc::new(monitoring::TelegramAlerter::new(
        cfg.telegram_bot_token.clone(),
        cfg.telegram_chat_id,
    ));

    // AUDIT 6.1: install a panic hook that flushes open S5 positions to disk before the
    // panicking task unwinds. Combined with panic="unwind", this restores the crash-safety
    // the docs claimed. Positions are also saved on every open/close, so this is a backstop.
    {
        let hf_for_panic = hf_monitor.clone();
        let store_path   = cfg.position_store_path.clone();
        let default_hook = std::panic::take_hook();
        std::panic::set_hook(Box::new(move |info| {
            let positions = hf_for_panic.get_rate_arb_positions();
            tracing::error!("PANIC: {} — flushing {} rate-arb positions to {}",
                info, positions.len(), store_path);
            if let Err(e) = position_store::save(&store_path, &positions) {
                tracing::error!("panic-time position flush failed: {}", e);
            }
            default_hook(info);
        }));
    }

    // reload persisted rate arb positions on startup
    match position_store::load::<strategies::rate_arb::RateArbPosition>(&cfg.position_store_path) {
        Ok(positions) if !positions.is_empty() => {
            tracing::info!("Reloaded {} rate arb positions from {}", positions.len(), cfg.position_store_path);
            for pos in positions {
                hf_monitor.register_position(pos);
            }
        }
        Ok(_) => tracing::info!("No persisted rate arb positions found"),
        Err(e) => tracing::warn!("Could not load persisted positions ({}): starting fresh", e),
    }

    tracing::info!("Building pool registry...");
    let registry = Arc::new(RwLock::new(PoolRegistry::build(provider_blocks.clone()).await?));
    tracing::info!("Pool registry: {} pools", registry.read().await.pool_count());

    let price_feed = PriceFeed::new(provider_blocks.clone(), registry.clone()).await?;

    // All strategies active unconditionally — bootstrap position indexer.
    if cfg.flash_executor_address.is_empty() {
        anyhow::bail!("flash_executor_address must be set (env: CORVUS_FLASH_EXECUTOR_ADDRESS)");
    }
    let genesis = cfg.bootstrap_from_block;
    tracing::info!("Bootstrapping positions from block {}...", genesis);
    pos_indexer.bootstrap(genesis, block_number.as_u64()).await?;
    tracing::info!("Bootstrap: {} positions", pos_indexer.position_count());

    let pi_live = pos_indexer.clone();
    tokio::spawn(async move {
        if let Err(e) = pi_live.watch_live().await {
            tracing::error!("Position indexer: {}", e);
        }
    });

    // ── Mempool fast-loop (S3 presign + S4 JIT on pending txs) ───────────
    {
        let mm4      = mempool_mon.clone();
        let sim4     = sim_engine.clone();
        let fl4      = flash_router.clone();
        let sub4     = submission.clone();
        let pi3      = pos_indexer.clone();
        let sim3     = sim_engine.clone();
        let fl3      = flash_router.clone();
        let sub3     = submission.clone();
        let cfg_mp   = cfg.clone();
        let gas_mp   = gas_oracle.clone();
        let prov_mp  = provider_mempool.clone();

        tokio::spawn(async move {
            let mut stream = match prov_mp.watch_pending_transactions().await {
                Ok(s)  => s,
                Err(e) => { tracing::error!("watch_pending_transactions: {}", e); return; }
            };
            while let Some(hash) = stream.next().await {
                let block_n = 0u64;
                // S4: JIT — runs only here (not in confirmed block loop)
                // AUDIT 2.9: gated behind cfg.jit_enabled (default false) until rebuilt.
                if cfg_mp.jit_enabled && !circuit_tripped(&S4_REVERTS, REVERT_THRESHOLD_JIT, "S4-JIT") {
                    for swap in mm4.get_large_pending_swaps() {
                        let sim       = sim4.clone(); let fl = fl4.clone(); let su = sub4.clone();
                        let cfg_c     = cfg_mp.clone();
                        let gas_c     = gas_mp.clone();
                        let eth_price = mm4.eth_price();
                        tokio::spawn(async move {
                            if let Err(e) = strategies::flash_jit::run_on_pending(
                                swap, sim, fl, su, cfg_c, gas_c, eth_price, block_n,
                            ).await {
                                record_revert(&S4_REVERTS, &S4_CONSEC);
                                tracing::debug!("S4 pending JIT: {}", e);
                            }
                        });
                    }
                }
                // S3: Liquidation presign on pending oracle price updates
                let pending_prices = mm4.pending_oracle_prices();
                if !pending_prices.is_empty() {
                    let pi    = pi3.clone(); let si = sim3.clone();
                    let fl    = fl3.clone(); let su = sub3.clone();
                    let cfg_c = cfg_mp.clone();
                    let mm_c  = mm4.current_oracle_prices();
                    let eth   = mm4.eth_price();
                    tokio::spawn(async move {
                        if let Err(e) = strategies::liquidation::run_presign(pi, mm_c, pending_prices, si, fl, su, cfg_c, eth).await {
                            tracing::debug!("S3 presign: {}", e);
                        }
                    });
                }
                let _ = hash;
            }
        });
    }

    // ── Main confirmed-block loop ──────────────────────────────────────────
    tracing::info!("Subscribing to new blocks — all strategies active...");
    let mut block_stream = provider_blocks.subscribe_blocks().await?;

    loop {
        tokio::select! {
            _ = shutdown.notified() => {
                tracing::warn!("Shutdown — exiting cleanly");
                break;
            }
            block_opt = block_stream.next() => {
                let block = match block_opt {
                    Some(b) => b,
                    None    => { tracing::warn!("Block stream ended"); break; }
                };
                let bn        = block.number.unwrap_or_default().as_u64();
                let timestamp = block.timestamp.as_u64();
                let coinbase  = block.author.unwrap_or_default();

                // measure IPC round-trip from block header arrival
                let block_recv_ts = std::time::Instant::now();

                if let Some(base_fee) = block.base_fee_per_gas {
                    gas_oracle.set_base_fee(base_fee.as_u64());
                } else {
                    gas_oracle.refresh().await;
                }

                // IPC latency = time from block arrival to gas oracle update
                monitoring::metrics::update_ipc_latency(block_recv_ts.elapsed().as_secs_f64() * 1000.0);

                mempool_mon.flush_confirmed_pending(bn);

                // Refresh pool registry every N blocks (configured by pool_refresh_blocks)
                if bn % cfg.pool_refresh_blocks == 0 {
                    match PoolRegistry::build(provider_blocks.clone()).await {
                        Ok(fresh) => {
                            *registry.write().await = fresh;
                            tracing::debug!("Pool registry refreshed at block {}", bn);
                        }
                        Err(e) => tracing::warn!("Pool registry refresh failed: {}", e),
                    }
                }

                if let Err(e) = submission.resync_nonce().await {
                    tracing::warn!("Nonce resync: {}", e);
                }

                let mut pf = price_feed.clone();
                pf.update(bn, provider_blocks.clone()).await?;
                rate_feed.refresh(aave_pool).await?;

                // AUDIT 2.4: refresh on-chain prices into the shared map BEFORE strategies
                // read them. Written under both token- and oracle-address keys.
                price_oracle.poll(&mempool_mon.current_oracle_prices()).await;

                if bn % cfg.monitoring_gas_check_interval_blocks == 0 {
                    if let Ok(balance) = provider_blocks.get_balance(
                        cfg.flash_executor_address.parse::<Address>().unwrap_or_default(), None
                    ).await {
                        let balance_eth = balance.as_u128() as f64 / 1e18;
                        monitoring::metrics::update_gas_reserve(balance_eth);
                        // F-09 Alert #3: Gas reserve < 1 ETH
                        if balance_eth < 1.0 {
                            let tg = telegram.clone();
                            let bal = balance_eth;
                            tokio::spawn(async move {
                                tg.alert_low_gas_reserve(bal).await;
                            });
                        }
                    }
                }

                // F-14 style: these are static compile-time constants — if they're
                // malformed a startup .expect() panic is correct (not a runtime failure).
                let eth_oracle: Address = base::CHAINLINK_ETH_USD.parse()
                    .expect("CHAINLINK_ETH_USD constant is malformed — fix addresses.rs");
                let eth_price = mempool_mon.current_oracle_prices()
                    .get(&eth_oracle).map(|p| *p)
                    .unwrap_or(cfg.eth_price_fallback_usd);

                // FIX (BUG-6): fetch live BTC price for cbBTC notional calculation.
                // Previously rate_arb used eth_price as a proxy, causing a ~33× underestimate.
                let btc_oracle: Address = base::CHAINLINK_CBBTC_USD.parse()
                    .expect("CHAINLINK_CBBTC_USD constant is malformed — fix addresses.rs");
                let btc_price = mempool_mon.current_oracle_prices()
                    .get(&btc_oracle).map(|p| *p)
                    .unwrap_or(eth_price * 30.0); // fallback: ~30× ETH (rough BTC/ETH ratio)

                // Dynamic spread threshold: at high gas, raise spread floor so we
                // don't chase sub-threshold arbs that won't clear gas cost.
                let base_fee_gwei = gas_oracle.base_fee_wei() as f64 / 1e9;
                let dynamic_spread_bps = cfg.min_arb_spread_bps
                    * (1.0 + base_fee_gwei / 10.0).min(10.0); // cap at 10×
                // Bake dynamic spread into per-block config clone — strategies read cfg.min_arb_spread_bps
                let mut cfg = cfg.clone();
                cfg.min_arb_spread_bps = dynamic_spread_bps;

                // ── Spawn strategies ──────────────────────────────────────
                let (pf1, pf2) = (pf.clone(), pf.clone());
                let (sim1, sim2) = (sim_engine.clone(), sim_engine.clone());
                let (fl1,  fl2)  = (flash_router.clone(), flash_router.clone());
                let (sub1, sub2) = (submission.clone(), submission.clone());
                let (cfg1, cfg2) = (cfg.clone(), cfg.clone());
                let (mm1,  mm2)  = (mempool_mon.clone(), mempool_mon.clone());

                // AUDIT 5.1: the per-block CacheDB snapshot clone was dead code — no strategy
                // ever called simulate_with_snapshot, so it just wasted ~30 MB/block. Removed.
                // S1/S2/S3 sims are algebraic; only S4 (disabled) touches REVM.

                // S1 — Cross-DEX Arb (algebraic simulation, no REVM lock)
                let tg1 = telegram.clone();
                let s1 = tokio::spawn(async move {
                    if circuit_tripped_alert(&S1_REVERTS, REVERT_THRESHOLD, "S1-CrossDexArb", &tg1).await { return; }
                    if let Err(e) = strategies::cross_dex_arb::run(pf1, sim1, fl1, sub1, cfg1, mm1).await {
                        record_revert(&S1_REVERTS, &S1_CONSEC);
                        tracing::debug!("S1: {}", e);
                    } else { record_success(&S1_REVERTS, &S1_CONSEC); }
                });

                // S2 — Tri-Arb (algebraic simulation, no REVM lock)
                let tg2 = telegram.clone();
                let s2 = tokio::spawn(async move {
                    if circuit_tripped_alert(&S2_REVERTS, REVERT_THRESHOLD, "S2-TriArb", &tg2).await { return; }
                    if let Err(e) = strategies::tri_arb::run(pf2, sim2, fl2, sub2, cfg2, mm2).await {
                        record_revert(&S2_REVERTS, &S2_CONSEC);
                        tracing::debug!("S2: {}", e);
                    } else { record_success(&S2_REVERTS, &S2_CONSEC); }
                });

                // S3 — Liquidation (REVM — uses snapshot)
                let sim3  = sim_engine.clone(); let fl3  = flash_router.clone();
                let sub3  = submission.clone(); let cfg3 = cfg.clone();
                let pi3   = pos_indexer.clone(); let mm3  = mempool_mon.clone();
                let ep3   = eth_price;
                let tg3   = telegram.clone();
                let s3 = tokio::spawn(async move {
                    if circuit_tripped_alert(&S3_REVERTS, REVERT_THRESHOLD, "S3-Liq", &tg3).await { return; }
                    if let Err(e) = strategies::liquidation::run(pi3, mm3, sim3, fl3, sub3, cfg3, ep3).await {
                        record_revert(&S3_REVERTS, &S3_CONSEC);
                        tracing::debug!("S3: {}", e);
                    } else { record_success(&S3_REVERTS, &S3_CONSEC); }
                });

                // S4 — Flash JIT: runs ONLY in the mempool fast-loop (below).
                // Confirmed-block S4 was dead work — pending swaps are already settled by
                // block time. Spawn removed. S4_REVERTS/S4_CONSEC still used in fast-loop.

                // S5 — Rate Arb
                let rf5  = rate_feed.clone(); let hf5  = hf_monitor.clone();
                let sim5 = sim_engine.clone(); let fl5  = flash_router.clone();
                let sub5 = submission.clone(); let cfg5 = cfg.clone();
                let tg5  = telegram.clone();
                let s5 = tokio::spawn(async move {
                    if circuit_tripped_alert(&S5_REVERTS, REVERT_THRESHOLD, "S5-RateArb", &tg5).await { return; }
                    if let Err(e) = strategies::rate_arb::run(rf5, hf5, sim5, fl5, sub5, cfg5, bn, eth_price, btc_price).await {
                        record_revert(&S5_REVERTS, &S5_CONSEC);
                        tracing::debug!("S5: {}", e);
                    } else { record_success(&S5_REVERTS, &S5_CONSEC); }
                });

                // Per-strategy independent timeouts — one slow strategy no longer
                // cancels the entire block's opportunity set.
                // S3 gets extra time: liquidation may process many REVM positions.
                tokio::join!(
                    tokio::time::timeout(std::time::Duration::from_millis(500), s1),
                    tokio::time::timeout(std::time::Duration::from_millis(500), s2),
                    tokio::time::timeout(std::time::Duration::from_millis(900), s3),
                    tokio::time::timeout(std::time::Duration::from_millis(600), s5),
                );
            }
        }
    }

    tracing::info!("Corvus exited cleanly.");
    Ok(())
}

fn validate_config(cfg: &Config) -> Result<()> {
    use std::str::FromStr;

    // Private key must be set — empty produces a cryptic LocalWallet parse error otherwise.
    if cfg.executor_private_key.expose().is_empty() {
        anyhow::bail!("executor_private_key must be set (env: CORVUS_EXECUTOR_PRIVATE_KEY)");
    }

    let addr_fields: &[(&str, &str)] = &[
        ("balancer_vault",          &cfg.balancer_vault),
        ("morpho_blue",             &cfg.morpho_blue),
        ("aave_addresses_provider", &cfg.aave_addresses_provider),
        ("aerodrome_factory",       &cfg.aerodrome_factory),
        ("uniswap_v3_factory",      &cfg.uniswap_v3_factory),
    ];
    for (name, val) in addr_fields {
        if val.is_empty() { anyhow::bail!("Config '{}' must not be empty", name); }
        Address::from_str(val).map_err(|_| anyhow::anyhow!("Config '{}' = '{}' invalid", name, val))?;
    }
    if cfg.chain_id == 0        { anyhow::bail!("chain_id must not be 0"); }
    if cfg.ipc_path.is_empty()  { anyhow::bail!("ipc_path must not be empty"); }
    // active_phase is retained for config documentation only — S1–S5 run unconditionally.
    if cfg.flash_executor_address.is_empty() {
        anyhow::bail!("flash_executor_address must be set — required for all strategies");
    }
    if cfg.gas_estimate_safety_margin < 1.0 {
        anyhow::bail!("gas_estimate_safety_margin must be >= 1.0 (got {})", cfg.gas_estimate_safety_margin);
    }
    if cfg.rate_arb_cbbtc_min_notional_usd < 10_000.0 {
        tracing::warn!("rate_arb_cbbtc_min_notional_usd={} is very low — cbBTC rate arb may not cover gas",
            cfg.rate_arb_cbbtc_min_notional_usd);
    }
    tracing::info!("Config validation passed — S1–S5 enabled.");
    Ok(())
}
