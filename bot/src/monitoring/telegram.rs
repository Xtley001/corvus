//! Telegram alerting — Corvus v1.0
//!
//! F-09 FIX: Implements all 6 mandatory operator alert conditions.
//! Without real-time Telegram alerts, critical failures (HF drop, gas reserve
//! exhaustion, strategy panic, circuit breaker trip) go unnoticed until an
//! operator manually checks Grafana — by which time capital may be lost.
//!
//! Alert conditions (all 6 required by audit):
//!   1. Circuit breaker trip
//!   2. Health factor < 1.15 (rate arb position at risk)
//!   3. Gas reserve < 1 ETH
//!   4. Strategy panic / task death
//!   5. S5 emergency unwind triggered
//!   6. Simulation divergence > 0.1%

use anyhow::Result;
use reqwest::Client;
use serde_json::json;

const TELEGRAM_API_BASE: &str = "https://api.telegram.org/bot";

#[derive(Clone)]
pub struct TelegramAlerter {
    client:   Client,
    token:    String,
    chat_id:  i64,
    enabled:  bool,
}

impl TelegramAlerter {
    /// Create a new alerter. If token is empty or chat_id is 0, alerting is
    /// disabled (logged as a warning, not a hard failure).
    pub fn new(token: String, chat_id: i64) -> Self {
        let enabled = !token.is_empty() && chat_id != 0;
        if !enabled {
            tracing::warn!(
                "Telegram alerting DISABLED — set CORVUS_TELEGRAM_BOT_TOKEN and \
                 CORVUS_TELEGRAM_CHAT_ID to enable. You will NOT receive production alerts."
            );
        }
        Self { client: Client::new(), token, chat_id, enabled }
    }

    /// Send a raw message. Silently logs on failure so one alert failure never
    /// kills the main loop. All callers go through this.
    pub async fn send(&self, msg: &str) {
        if !self.enabled { return; }
        if let Err(e) = self.send_inner(msg).await {
            tracing::error!("Telegram send failed: {}", e);
        }
    }

    async fn send_inner(&self, msg: &str) -> Result<()> {
        let url = format!("{}{}/sendMessage", TELEGRAM_API_BASE, self.token);
        let body = json!({
            "chat_id":    self.chat_id,
            "text":       msg,
            "parse_mode": "Markdown",
        });
        let resp = self.client
            .post(&url)
            .json(&body)
            .timeout(std::time::Duration::from_secs(5))
            .send()
            .await?;
        if !resp.status().is_success() {
            let status = resp.status();
            let body   = resp.text().await.unwrap_or_default();
            anyhow::bail!("Telegram API error {}: {}", status, body);
        }
        Ok(())
    }

    // ── Alert #1: Circuit breaker trip ───────────────────────────────────────

    pub async fn alert_circuit_breaker(&self, reason: &str) {
        self.send(&format!(
            "🚨 *CORVUS CIRCUIT BREAKER TRIPPED*\n\
             Chain: Base Mainnet\n\
             Reason: `{}`\n\
             All strategies halted. Manual review required.",
            reason
        )).await;
    }

    // ── Alert #2: Health factor < 1.15 ───────────────────────────────────────

    pub async fn alert_low_health_factor(&self, hf: f64, threshold: f64) {
        self.send(&format!(
            "⚠️ *CORVUS LOW HEALTH FACTOR*\n\
             HF: `{:.4}` (threshold: `{:.2}`)\n\
             Rate arb position approaching liquidation zone.\n\
             Emergency unwind may trigger if HF < `{:.2}`.",
            hf, threshold, threshold - 0.05
        )).await;
    }

    // ── Alert #3: Gas reserve < 1 ETH ────────────────────────────────────────

    pub async fn alert_low_gas_reserve(&self, balance_eth: f64) {
        self.send(&format!(
            "⛽ *CORVUS LOW GAS RESERVE*\n\
             Executor ETH balance: `{:.4} ETH`\n\
             Minimum required: `1.0 ETH`\n\
             Top up immediately or all strategies will halt.",
            balance_eth
        )).await;
    }

    // ── Alert #4: Strategy panic / task death ────────────────────────────────

    pub async fn alert_strategy_panic(&self, strategy: &str, error: &str) {
        self.send(&format!(
            "💀 *CORVUS STRATEGY PANIC*\n\
             Strategy: `{}`\n\
             Error: `{}`\n\
             Task has been restarted. Monitor for recurring failures.",
            strategy, &error[..error.len().min(200)]
        )).await;
    }

    // ── Alert #5: S5 emergency unwind triggered ───────────────────────────────

    pub async fn alert_emergency_unwind(&self, hf: f64, asset: &str) {
        self.send(&format!(
            "🔴 *CORVUS S5 EMERGENCY UNWIND*\n\
             Asset: `{}`\n\
             Health factor: `{:.4}` — below emergency threshold\n\
             Position being unwound now. Check Grafana for status.",
            asset, hf
        )).await;
    }

    // ── Alert #6: Simulation divergence > 0.1% ───────────────────────────────

    pub async fn alert_sim_divergence(&self, strategy: &str, sim_pct: f64, actual_pct: f64) {
        self.send(&format!(
            "📊 *CORVUS SIM DIVERGENCE ALERT*\n\
             Strategy: `{}`\n\
             Simulated profit: `{:.4}%`\n\
             Actual on-chain: `{:.4}%`\n\
             Divergence: `{:.4}%` — exceeds 0.1% threshold.\n\
             Review simulation parameters.",
            strategy, sim_pct, actual_pct, (sim_pct - actual_pct).abs()
        )).await;
    }
}
