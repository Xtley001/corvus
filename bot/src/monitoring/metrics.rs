//! Metrics server — Corvus v1.0
//! Binds to 127.0.0.1 only (strategy data must not be externally accessible).
//! Registers all Prometheus counters/gauges via once_cell guard (no double-register panic).

use lazy_static::lazy_static;
use once_cell::sync::OnceCell;
use prometheus::{Counter, CounterVec, Gauge, IntCounter, Opts, Registry};
use axum::{routing::get, Router};

lazy_static! {
    pub static ref TRADE_COUNT:      IntCounter = IntCounter::new("corvus_trades_total", "Trades executed").unwrap();
    pub static ref PROFIT_TOTAL:     Counter    = Counter::new("corvus_profit_usd_total", "Total profit USD").unwrap();
    pub static ref STRATEGY_PROFIT:  CounterVec = CounterVec::new(Opts::new("corvus_strategy_profit_usd", "Profit by strategy"), &["strategy"]).unwrap();
    pub static ref INCLUSION_RATE:   Gauge      = Gauge::new("corvus_inclusion_rate", "Bundle inclusion rate 0-1").unwrap();
    pub static ref GAS_RESERVE_ETH:  Gauge      = Gauge::new("corvus_gas_reserve_eth", "ETH gas reserve").unwrap();
    pub static ref RATE_ARB_MIN_HF:  Gauge      = Gauge::new("corvus_rate_arb_min_hf", "Minimum HF across rate arb positions").unwrap();
    pub static ref SUBMISSION_COUNT: IntCounter = IntCounter::new("corvus_submissions_total", "Tx submissions").unwrap();
    pub static ref SIM_REVERTS:      CounterVec = CounterVec::new(Opts::new("corvus_simulations_total", "Simulations by strategy and result"), &["strategy", "result"]).unwrap();
    // IPC latency — fed from main block loop on every block
    pub static ref NODE_IPC_LATENCY: Gauge      = Gauge::new("corvus_node_ipc_latency_ms", "Node IPC round-trip latency ms").unwrap();
    pub static ref SUB_LATENCY_MS:   Gauge      = Gauge::new("corvus_submission_latency_ms", "Submission pipeline latency p99 ms").unwrap();
    pub static ref ACTIVE_POSITIONS: Gauge      = Gauge::new("corvus_active_positions", "Active S5 rate arb positions").unwrap();
    // tx submission vs on-chain inclusion tracking
    pub static ref SUBMITTED_TOTAL:  IntCounter = IntCounter::new("corvus_submitted_total", "Total txs broadcast").unwrap();
    pub static ref INCLUDED_TOTAL:   IntCounter = IntCounter::new("corvus_included_total", "Total txs confirmed on-chain").unwrap();
}

static METRICS_INIT: OnceCell<()> = OnceCell::new();

pub fn init() {
    METRICS_INIT.get_or_init(|| {
        let r = prometheus::default_registry();
        let _ = r.register(Box::new(TRADE_COUNT.clone()));
        let _ = r.register(Box::new(PROFIT_TOTAL.clone()));
        let _ = r.register(Box::new(STRATEGY_PROFIT.clone()));
        let _ = r.register(Box::new(INCLUSION_RATE.clone()));
        let _ = r.register(Box::new(GAS_RESERVE_ETH.clone()));
        let _ = r.register(Box::new(RATE_ARB_MIN_HF.clone()));
        let _ = r.register(Box::new(SUBMISSION_COUNT.clone()));
        let _ = r.register(Box::new(SIM_REVERTS.clone()));
        let _ = r.register(Box::new(NODE_IPC_LATENCY.clone()));
        let _ = r.register(Box::new(SUB_LATENCY_MS.clone()));
        let _ = r.register(Box::new(ACTIVE_POSITIONS.clone()));
        let _ = r.register(Box::new(SUBMITTED_TOTAL.clone()));
        let _ = r.register(Box::new(INCLUDED_TOTAL.clone()));
        RATE_ARB_MIN_HF.set(99.0);
        INCLUSION_RATE.set(0.0);
        GAS_RESERVE_ETH.set(0.0);
    });
}

pub fn record_trade(strategy: &str, profit_usd: f64) {
    TRADE_COUNT.inc();
    PROFIT_TOTAL.inc_by(profit_usd);
    STRATEGY_PROFIT.with_label_values(&[strategy]).inc_by(profit_usd);
    tracing::info!("Trade: strategy={} profit=${:.4}", strategy, profit_usd);
}

pub fn record_submission() {
    SUBMISSION_COUNT.inc();
    SUBMITTED_TOTAL.inc();
}

pub fn record_inclusion() {
    INCLUDED_TOTAL.inc();
    // Recompute inclusion rate
    let sub = SUBMITTED_TOTAL.get() as f64;
    let inc = INCLUDED_TOTAL.get() as f64;
    if sub > 0.0 { INCLUSION_RATE.set(inc / sub); }
}

pub fn record_sim_result(strategy: &str, result: &str) {
    SIM_REVERTS.with_label_values(&[strategy, result]).inc();
}

pub fn alert_hf(hf: f64) {
    RATE_ARB_MIN_HF.set(hf);
    if hf < 1.10      { tracing::error!("SEVERITY 1: Rate arb HF={:.3} — EMERGENCY UNWIND", hf); }
    else if hf < 1.20 { tracing::warn!("SEVERITY 2: Rate arb HF={:.3} approaching threshold", hf); }
}

pub fn record_position_open(strategy: &str)  { ACTIVE_POSITIONS.inc(); tracing::info!("Position opened: {}", strategy); }
pub fn record_position_close(strategy: &str) { ACTIVE_POSITIONS.dec(); tracing::info!("Position closed: {}", strategy); }

pub fn update_gas_reserve(eth: f64) {
    GAS_RESERVE_ETH.set(eth);
    if eth < 1.0 { tracing::warn!("SEVERITY 2: Gas reserve low: {:.3} ETH", eth); }
}

/// called from main block loop to track node health.
pub fn update_ipc_latency(ms: f64)      { NODE_IPC_LATENCY.set(ms); }
/// driven by record_submission / record_inclusion above.
pub fn update_inclusion_rate(rate: f64) { INCLUSION_RATE.set(rate); }

pub async fn serve(port: u16) {
    let app = Router::new().route("/metrics", get(metrics_handler));
    // Bind to localhost only — metrics expose strategy intelligence.
    let listener = match tokio::net::TcpListener::bind(format!("127.0.0.1:{}", port)).await {
        Ok(l)  => l,
        Err(e) => {
            tracing::error!("Metrics server failed to bind on port {}: {} — continuing without metrics", port, e);
            return;
        }
    };
    tracing::info!("Metrics server on 127.0.0.1:{}", port);
    if let Err(e) = axum::serve(listener, app).await {
        tracing::error!("Metrics server exited: {}", e);
    }
}

async fn metrics_handler() -> String {
    use prometheus::Encoder;
    let enc = prometheus::TextEncoder::new();
    let mut buf = Vec::new();
    enc.encode(&prometheus::gather(), &mut buf).unwrap();
    String::from_utf8(buf).unwrap_or_default()
}
