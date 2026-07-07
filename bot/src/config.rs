//! SecretKey uses zeroize to wipe private key from heap on drop.
//!   v1.1: removed S6 (backrun) and S7 (pendle) config fields.
//!   Added: rate_arb_min_hold_blocks.

use serde::Deserialize;
use anyhow::Result;
use zeroize::Zeroize;

/// Wrapper that (a) prevents the key from appearing in logs/debug output,
/// (b) zeroes the backing String memory when dropped.
#[derive(Clone)]
pub struct SecretKey(String);

impl SecretKey {
    pub fn expose(&self) -> &str { &self.0 }
}

impl std::fmt::Debug for SecretKey {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "[REDACTED]")
    }
}

impl Drop for SecretKey {
    fn drop(&mut self) {
        // Zero the heap allocation before the allocator reclaims it.
        self.0.zeroize();
    }
}

impl<'de> Deserialize<'de> for SecretKey {
    fn deserialize<D: serde::Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
        Ok(SecretKey(String::deserialize(d)?))
    }
}

#[derive(Debug, Clone, Deserialize)]
pub struct Config {
    pub ipc_path:               String,
    pub chain_id:               u64,
    pub executor_private_key:   SecretKey,
    pub cold_wallet:            String,
    pub flash_executor_address: String,
    pub balancer_vault:         String,
    pub morpho_blue:            String,
    pub aave_addresses_provider:String,
    pub aerodrome_factory:      String,
    pub uniswap_v3_factory:     String,

    pub min_profit_usd:              f64,
    pub min_arb_spread_bps:          f64,
    pub min_liquidation_profit_usd:  f64,
    pub min_jit_profit_usd:          f64,
    pub min_rate_spread_bps:         f64,
    pub max_gas_price_gwei:          f64,
    pub pool_min_liquidity_usd:      f64,
    pub pool_refresh_blocks:         u64,
    pub active_phase:                u8,
    pub eth_price_fallback_usd:      f64,
    pub monitoring_gas_check_interval_blocks: u64,

    /// Block from which to start scanning for borrow positions.
    pub bootstrap_from_block: u64,

    // ── REVM ──────────────────────────────────────────────────────────────
    pub revm_sim_gas_limit: u64,

    // ── JIT ───────────────────────────────────────────────────────────────
    pub jit_min_swap_usd: f64,
    /// AUDIT 2.9: master switch for S4 (Flash JIT). Default false — the JIT profit
    /// math is heuristic, the Aerodrome path mints a UniV3 position (pool-type mismatch),
    /// and Base's thin public mempool makes JIT signal sparse. Keep off until rebuilt +
    /// fork-tested. Highest cost-per-failed-attempt of any strategy.
    pub jit_enabled: bool,

    // ── Gas ───────────────────────────────────────────────────────────────
    pub priority_fee_multiplier:   f64,
    pub max_fee_base_multiplier:   f64,
    pub rpc_timeout_secs:          u64,
    /// multiply all static gas estimates by this factor before profit check.
    /// Accounts for simulation-vs-onchain variance. Default: 1.15 (15% safety margin).
    pub gas_estimate_safety_margin: f64,

    // ── Slippage & profit gates ───────────────────────────────────────────
    pub min_profit_slippage_bps:    u64,
    pub liquidation_swap_haircut:   f64,
    pub liquidation_safety_factor:  f64,
    pub liquidation_hf_threshold:   f64,

    // ── Rate Arb ─────────────────────────────────────────────────────────
    pub rate_arb_close_spread_bps:     f64,
    pub rate_arb_close_hf:             f64,
    pub rate_arb_emergency_hf:         f64,
    pub rate_arb_max_hold_days:        f64,
    pub rate_arb_base_leverage:        f64,
    pub rate_arb_max_notional_usdc:    u128,
    pub rate_arb_max_notional_weth:    u128,
    pub rate_arb_max_breakeven_days:   f64,
    /// AUDIT-cbBTC: minimum USD notional for cbBTC rate arb to be worth gas.
    /// Set to 50_000 — below this the daily yield barely covers open/close gas cost.
    pub rate_arb_cbbtc_min_notional_usd: f64,

    /// AUDIT 2.5: master switch for OPENING new rate-arb (S5) positions. Default false —
    /// the current open/close calldata only supports Aave, and Aave↔Aave same-asset carry
    /// captures ~no spread, so opening stays off until a native Morpho carry path exists.
    /// Unwinding existing positions is always allowed regardless of this flag.
    pub rate_arb_open_enabled: bool,

    /// Minimum number of blocks a rate-arb position must be held before unwind is
    /// considered. Prevents open+close in the same epoch (2× gas, zero spread capture).
    /// Emergency unwind (HF critical) ignores this floor. ~150 blocks ≈ 5 minutes.
    pub rate_arb_min_hold_blocks: u64,

    // ── Position persistence ───────────────────────────────────────────────
    /// Path for persisting open rate-arb positions across restarts.
    pub position_store_path:            String,

    pub base_sequencer_rpc:  String,
    pub builder_endpoints:   Vec<String>,
    pub metrics_port:        u16,

    // F-09 FIX: Telegram alerting — required for all 6 mandatory alert conditions.
    // Without this, circuit breaker trips, HF drops, panics, etc. go unnoticed.
    /// Telegram bot token from @BotFather — set via CORVUS_TELEGRAM_BOT_TOKEN env var.
    pub telegram_bot_token:  String,
    /// Telegram chat ID to send alerts to — set via CORVUS_TELEGRAM_CHAT_ID env var.
    /// Use a negative number for group chats (e.g. -100123456789).
    pub telegram_chat_id:    i64,
}

impl Config {
    pub fn load() -> Result<Self> {
        dotenv::dotenv().ok();
        Ok(config::Config::builder()
            .add_source(config::File::with_name("config/default"))
            .add_source(config::Environment::with_prefix("CORVUS"))
            .build()?
            .try_deserialize()?)
    }
}
