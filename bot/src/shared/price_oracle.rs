//! On-chain Chainlink price poller (AUDIT 2.4 FIX).
//!
//! The mempool monitor only ever populated the ETH/USD price (opportunistically,
//! from decoded `transmit()` calls). Every other consumer — liquidation sizing
//! (keyed by *token* address), cbBTC rate-arb notional, the peg monitor — then read
//! an empty map and either errored out or fell back to a wrong proxy.
//!
//! This module proactively reads each feed's `latestRoundData()` every block and
//! writes USD prices into the shared price map under BOTH conventions the codebase
//! uses:
//!   • token-address key  → used by `simulate_liquidation` (collateral/debt asset)
//!   • oracle-address key → used by main.rs / cross_dex / rate_arb (ETH & cbBTC)
//!
//! Rate feeds (cbETH/ETH, wstETH/ETH) are ETH-denominated and composed to USD by
//! multiplying by the live ETH/USD price.

use anyhow::Result;
use dashmap::DashMap;
use ethers::prelude::*;
use std::sync::Arc;
use crate::shared::addresses::base;

pub struct PriceOracle {
    provider: Arc<Provider<Ipc>>,
    fallback_eth_usd: f64,
    // pre-parsed addresses (fail fast at startup if a constant is malformed)
    eth_usd_feed:   Address,
    cbbtc_usd_feed: Address,
    cbeth_eth_feed: Address,
    wsteth_eth_feed:Address,
    weth:   Address,
    usdc:   Address,
    usdbc:  Address,
    dai:    Address,
    cbeth:  Address,
    wsteth: Address,
    cbbtc:  Address,
}

impl PriceOracle {
    pub fn new(provider: Arc<Provider<Ipc>>, fallback_eth_usd: f64) -> Result<Self> {
        Ok(Self {
            provider,
            fallback_eth_usd,
            eth_usd_feed:    base::CHAINLINK_ETH_USD.parse()?,
            cbbtc_usd_feed:  base::CHAINLINK_CBBTC_USD.parse()?,
            cbeth_eth_feed:  base::CHAINLINK_CBETH_ETH.parse()?,
            wsteth_eth_feed: base::CHAINLINK_WSTETH_ETH.parse()?,
            weth:   base::WETH.parse()?,
            usdc:   base::USDC.parse()?,
            usdbc:  base::USDBC.parse()?,
            dai:    base::DAI.parse()?,
            cbeth:  base::CBETH.parse()?,
            wsteth: base::WSTETH.parse()?,
            cbbtc:  base::CBBTC.parse()?,
        })
    }

    /// Read one Chainlink aggregator's `latestRoundData()` and return the answer
    /// scaled by `decimals` (8 for USD feeds, 18 for ETH-denominated rate feeds).
    async fn read_feed(&self, feed: Address, decimals: u32) -> Result<f64> {
        let sel = &ethers::utils::keccak256(b"latestRoundData()")[..4];
        let res = self.provider.call(
            &TransactionRequest { to: Some(feed.into()), data: Some(sel.to_vec().into()), ..Default::default() }.into(),
            None,
        ).await?;
        // returns (uint80 roundId, int256 answer, uint256, uint256, uint80) — answer at [32..64]
        if res.len() < 64 { anyhow::bail!("latestRoundData: short response for {:?}", feed); }
        let raw = U256::from_big_endian(&res[32..64]);
        // Chainlink answers for these feeds are always positive; reject a top-bit-set
        // (negative) value rather than wrapping it into a nonsense huge price.
        if raw.bit(255) { anyhow::bail!("latestRoundData: negative answer for {:?}", feed); }
        let val = raw.as_u128() as f64 / 10f64.powi(decimals as i32);
        if !(val.is_finite() && val > 0.0) { anyhow::bail!("latestRoundData: non-positive price for {:?}", feed); }
        Ok(val)
    }

    /// Poll all feeds and write USD prices into the shared map (token- and oracle-keyed).
    /// Best-effort: a single feed failure logs and is skipped; the rest still update.
    pub async fn poll(&self, out: &DashMap<Address, f64>) {
        let eth_usd = match self.read_feed(self.eth_usd_feed, 8).await {
            Ok(v)  => v,
            Err(e) => {
                tracing::warn!("PriceOracle ETH/USD read failed ({}) — using fallback ${:.0}", e, self.fallback_eth_usd);
                self.fallback_eth_usd
            }
        };
        // ETH/USD under both key conventions.
        out.insert(self.eth_usd_feed, eth_usd);
        out.insert(self.weth, eth_usd);
        // Stables — treated at peg here; the dedicated peg monitor handles depegs.
        out.insert(self.usdc, 1.0);
        out.insert(self.usdbc, 1.0);
        out.insert(self.dai, 1.0);

        if let Ok(btc) = self.read_feed(self.cbbtc_usd_feed, 8).await {
            out.insert(self.cbbtc, btc);
            out.insert(self.cbbtc_usd_feed, btc); // oracle-keyed for rate_arb/main btc_price
        } else {
            tracing::debug!("PriceOracle cbBTC/USD read failed — cbBTC priced stale");
        }
        if let Ok(r) = self.read_feed(self.cbeth_eth_feed, 18).await {
            out.insert(self.cbeth, r * eth_usd);
        }
        if let Ok(r) = self.read_feed(self.wsteth_eth_feed, 18).await {
            out.insert(self.wsteth, r * eth_usd);
        }
    }
}
