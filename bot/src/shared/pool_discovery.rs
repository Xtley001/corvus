//!
//!   ADDR-CRIT-02: Morpho market IDs updated to Base mainnet IDs.
//!   POOL-MED-01:  USDbC/USDC pair kept but min_tvl raised to $500K —
//!                 USDbC is legacy/deprecated; only trade if deep enough.
//!   POOL-MED-02:  USDC/DAI pair min_tvl raised to $500K — DAI on Base
//!                 is thin; false positives at $200K cause gas waste.
//!   POOL-MED-03:  Triangle USDC→WETH→cbETH→USDC dex_type[2]=2 (Balancer)
//!                 is REMOVED — Balancer has no meaningful cbETH/USDC pool
//!                 on Base. Replaced with UniV3 (dex_type=1) routing.
//!                 The Balancer encode path is still available for future use.
//!   POOL-NOTE-01: wstETH/WETH fee tier corrected to 100bps (not 500bps) —
//!                 the highest-TVL UniV3 wstETH/WETH pool on Base uses 0.01%.

use anyhow::Result;
use dashmap::DashMap;
use ethers::prelude::*;
use std::sync::Arc;

// Token + factory addresses sourced from the canonical addresses registry.
use crate::shared::addresses::base;
pub const WETH:   &str = base::WETH;
pub const USDC:   &str = base::USDC;
pub const cbETH:  &str = base::CBETH;
pub const USDbC:  &str = base::USDBC;
pub const wstETH: &str = base::WSTETH;
pub const cbBTC:  &str = base::CBBTC;
pub const DAI:    &str = base::DAI;

pub const AERODROME_FACTORY:  &str = base::AERODROME_FACTORY;
pub const UNISWAP_V3_FACTORY: &str = base::UNISWAP_V3_FACTORY;

const TOKEN_DECIMALS: &[(&str, u32)] = &[
    (WETH,   18),
    (USDC,    6),
    (cbETH,  18),
    (USDbC,   6),
    (wstETH, 18),
    (cbBTC,   8),
    (DAI,    18),
];

fn token_decimals(token: &str) -> u32 {
    TOKEN_DECIMALS.iter()
        .find(|(addr, _)| addr.eq_ignore_ascii_case(token))
        .map(|(_, d)| *d)
        .unwrap_or(18)
}

pub struct PairSpec {
    pub name:        &'static str,
    pub token0:      &'static str,
    pub token1:      &'static str,
    pub aero_stable: bool,
    /// UniV3 fee in ppm (100=0.01%, 500=0.05%, 3000=0.3%, 10000=1%)
    pub univ3_fee:   u32,
    pub min_tvl_usd: f64,
}

/// Monitored pairs for S1 (cross-DEX arb).
/// Each pair is checked on both Aerodrome and UniV3 every block.
/// min_tvl_usd is enforced in pool discovery — thin pools are excluded.
pub const MONITORED_PAIRS: &[PairSpec] = &[
    // ── Core high-TVL pairs (always liquid) ──────────────────────────────
    PairSpec { name: "USDC/WETH",   token0: USDC,  token1: WETH,
        aero_stable: false, univ3_fee: 500,  min_tvl_usd: 1_000_000.0 },
    PairSpec { name: "cbETH/WETH",  token0: cbETH, token1: WETH,
        aero_stable: false, univ3_fee: 500,  min_tvl_usd: 500_000.0   },
    PairSpec { name: "wstETH/WETH", token0: wstETH,token1: WETH,
        // POOL-NOTE-01: UniV3 wstETH/WETH highest TVL pool on Base uses 100bps (0.01%)
        aero_stable: false, univ3_fee: 100,  min_tvl_usd: 200_000.0   },
    PairSpec { name: "cbBTC/USDC",  token0: cbBTC, token1: USDC,
        aero_stable: false, univ3_fee: 3000, min_tvl_usd: 100_000.0   },
    PairSpec { name: "cbBTC/WETH",  token0: cbBTC, token1: WETH,
        aero_stable: false, univ3_fee: 3000, min_tvl_usd: 100_000.0   },

    // ── Stablecoin pairs (high min_tvl — only trade when meaningfully liquid) ──
    // POOL-MED-01: USDbC is deprecated/legacy — raised threshold to $500K so
    // we only engage when residual depth actually justifies gas cost.
    PairSpec { name: "USDbC/USDC",  token0: USDbC, token1: USDC,
        aero_stable: true,  univ3_fee: 500,  min_tvl_usd: 500_000.0   },
    // POOL-MED-02: DAI on Base is thin vs USDC ecosystem — raised to $500K.
    PairSpec { name: "USDC/DAI",    token0: USDC,  token1: DAI,
        aero_stable: true,  univ3_fee: 100,  min_tvl_usd: 500_000.0   },
];

pub struct TriangleSpec {
    pub name:        &'static str,
    pub tokens:      [&'static str; 3],
    pub aero_stable: [bool; 3],
    pub univ3_fees:  [u32; 3],
    /// 0=Aerodrome, 1=UniV3, 2=Balancer (requires Balancer pool to exist)
    pub dex_types:   [u8; 3],
}

/// Monitored triangles for S2 (tri-arb).
///
/// POOL-MED-03: The original triangle `USDC→WETH→cbETH→USDC` used
/// dex_type[2]=2 (Balancer) for the cbETH→USDC leg. Balancer has no
/// active cbETH/USDC pool on Base — every attempt would revert.
/// Fixed to dex_type[2]=1 (UniV3 500bps).
pub const MONITORED_TRIANGLES: &[TriangleSpec] = &[
    // USDC → WETH → cbETH → USDC
    // Leg 0: Aerodrome USDC/WETH (volatile)
    // Leg 1: UniV3 500bps WETH/cbETH
    // Leg 2: UniV3 500bps cbETH/USDC  ← FIXED (was Balancer dex_type=2)
    TriangleSpec { name: "USDC→WETH→cbETH→USDC",
        tokens:      [USDC, WETH, cbETH],
        aero_stable: [false, false, false],
        univ3_fees:  [500, 500, 500],
        dex_types:   [0, 1, 1] },  // ← was [0, 1, 2]

    // USDC → USDbC → WETH → USDC (stablecoin bridge arb)
    // Only fires when USDbC still has depth; pool gating handles it.
    TriangleSpec { name: "USDC→USDbC→WETH→USDC",
        tokens:      [USDC, USDbC, WETH],
        aero_stable: [true, false, false],
        univ3_fees:  [500, 500, 500],
        dex_types:   [0, 0, 1] },

    // USDC → cbBTC → WETH → USDC
    TriangleSpec { name: "USDC→cbBTC→WETH→USDC",
        tokens:      [USDC, cbBTC, WETH],
        aero_stable: [false, false, false],
        univ3_fees:  [3000, 3000, 500],
        dex_types:   [1, 1, 0] },

    // WETH → cbBTC → USDC → WETH (reverse)
    TriangleSpec { name: "WETH→cbBTC→USDC→WETH",
        tokens:      [WETH, cbBTC, USDC],
        aero_stable: [false, false, false],
        univ3_fees:  [3000, 3000, 500],
        dex_types:   [0, 1, 1] },

    // cbBTC → WETH → USDC → cbBTC
    TriangleSpec { name: "cbBTC→WETH→USDC→cbBTC",
        tokens:      [cbBTC, WETH, USDC],
        aero_stable: [false, false, false],
        univ3_fees:  [3000, 500, 3000],
        dex_types:   [1, 0, 1] },

    // USDC → WETH → wstETH → USDC (new — wstETH has good liquidity on Base)
    TriangleSpec { name: "USDC→WETH→wstETH→USDC",
        tokens:      [USDC, WETH, wstETH],
        aero_stable: [false, false, false],
        univ3_fees:  [500, 100, 500],
        dex_types:   [0, 1, 1] },
];

#[derive(Debug, Clone, Copy, PartialEq)]
pub enum DexType { Aerodrome, UniswapV3, Balancer }

#[derive(Debug, Clone)]
pub struct ResolvedPool {
    pub address:  Address,
    pub dex:      DexType,
    pub token0:   Address,
    pub token1:   Address,
    pub fee_bps:  u32,
    pub tvl_usd:  f64,
}

pub struct PoolRegistry {
    pools:        DashMap<(Address, Address, u8), ResolvedPool>,
    aero_factory: Address,
    uni_factory:  Address,
}

impl PoolRegistry {
    pub async fn build(provider: Arc<Provider<Ipc>>) -> Result<Self> {
        let registry = Self {
            pools:        DashMap::new(),
            aero_factory: AERODROME_FACTORY.parse()?,
            uni_factory:  UNISWAP_V3_FACTORY.parse()?,
        };
        registry.refresh(&provider).await?;
        Ok(registry)
    }

    pub async fn refresh(&self, provider: &Arc<Provider<Ipc>>) -> Result<()> {
        for pair in MONITORED_PAIRS {
            let s0: Address = pair.token0.parse()?;
            let s1: Address = pair.token1.parse()?;
            // AUDIT 2.7 FIX: on-chain pools order tokens by address, and getReserves()/
            // slot0-derived reserves come back in that canonical order. Store token0/token1
            // sorted by address so reserve0↔token0 and reserve1↔token1 line up. Otherwise
            // every pair whose spec order ≠ address order had inverted prices.
            let (t0, t1) = if s0 < s1 { (s0, s1) } else { (s1, s0) };

            if let Ok(addr) = eth_call_get_pool_aero(provider, self.aero_factory, t0, t1, pair.aero_stable).await {
                if addr != Address::zero() {
                    let live_fee = query_aero_fee(provider, self.aero_factory, addr, pair.aero_stable).await
                        .unwrap_or(30);
                    // If TVL fetch fails, treat as 0 — pool skips min_tvl filter rather
                    // than being silently registered with a phantom $0 TVL.
                    let tvl = match approx_tvl(provider, t0, addr).await {
                        Ok(v) => v,
                        Err(e) => {
                            tracing::warn!("approx_tvl failed for Aerodrome {} ({:?}): {} — skipping pool", pair.name, addr, e);
                            0.0
                        }
                    };
                    if tvl >= pair.min_tvl_usd {
                        self.pools.insert((t0, t1, 0), ResolvedPool {
                            address: addr, dex: DexType::Aerodrome,
                            token0: t0, token1: t1, fee_bps: live_fee, tvl_usd: tvl,
                        });
                        tracing::debug!("Pool registered: Aerodrome {} tvl=${:.0}", pair.name, tvl);
                    } else {
                        tracing::info!(
                            "Pool SKIPPED (thin): Aerodrome {} tvl=${:.0} < min=${:.0}",
                            pair.name, tvl, pair.min_tvl_usd
                        );
                    }
                }
            }

            if let Ok(addr) = eth_call_get_pool_uni(provider, self.uni_factory, t0, t1, pair.univ3_fee).await {
                if addr != Address::zero() {
                    let tvl = match approx_tvl(provider, t0, addr).await {
                        Ok(v) => v,
                        Err(e) => {
                            tracing::warn!("approx_tvl failed for UniV3 {} ({:?}): {} — skipping pool", pair.name, addr, e);
                            0.0
                        }
                    };
                    if tvl >= pair.min_tvl_usd {
                        self.pools.insert((t0, t1, 1), ResolvedPool {
                            address: addr, dex: DexType::UniswapV3,
                            token0: t0, token1: t1,
                            fee_bps: pair.univ3_fee / 100,
                            tvl_usd: tvl,
                        });
                        tracing::debug!("Pool registered: UniV3 {} fee={}ppm tvl=${:.0}", pair.name, pair.univ3_fee, tvl);
                    } else {
                        tracing::info!(
                            "Pool SKIPPED (thin): UniV3 {} tvl=${:.0} < min=${:.0}",
                            pair.name, tvl, pair.min_tvl_usd
                        );
                    }
                } else {
                    tracing::info!("Pool NOT FOUND: UniV3 {} fee={}ppm (pool address = zero)", pair.name, pair.univ3_fee);
                }
            }
        }
        tracing::info!("PoolRegistry refreshed: {} pools active", self.pools.len());
        Ok(())
    }

    pub fn get(&self, t0: Address, t1: Address, dex: u8) -> Option<ResolvedPool> {
        self.pools.get(&(t0, t1, dex)).map(|r| r.clone())
            .or_else(|| self.pools.get(&(t1, t0, dex)).map(|r| r.clone()))
    }

    pub fn all(&self)        -> Vec<ResolvedPool> { self.pools.iter().map(|e| e.value().clone()).collect() }
    pub fn pool_count(&self) -> usize              { self.pools.len() }
}

async fn eth_call_get_pool_aero(p: &Arc<Provider<Ipc>>, factory: Address, t0: Address, t1: Address, stable: bool) -> Result<Address> {
    let sel  = &ethers::utils::keccak256(b"getPool(address,address,bool)")[..4];
    let data = [sel, &ethers::abi::encode(&[Token::Address(t0), Token::Address(t1), Token::Bool(stable)])].concat();
    let res  = p.call(&tx_req(factory, data), None).await?;
    if res.len() < 32 { anyhow::bail!("getPool(aero): short response"); }
    Ok(Address::from_slice(&res[12..32]))
}

async fn eth_call_get_pool_uni(p: &Arc<Provider<Ipc>>, factory: Address, t0: Address, t1: Address, fee: u32) -> Result<Address> {
    let sel  = &ethers::utils::keccak256(b"getPool(address,address,uint24)")[..4];
    let data = [sel, &ethers::abi::encode(&[Token::Address(t0), Token::Address(t1), Token::Uint(fee.into())])].concat();
    let res  = p.call(&tx_req(factory, data), None).await?;
    if res.len() < 32 { anyhow::bail!("getPool(uni): short response"); }
    Ok(Address::from_slice(&res[12..32]))
}

async fn query_aero_fee(p: &Arc<Provider<Ipc>>, factory: Address, pool: Address, stable: bool) -> Result<u32> {
    let sel  = &ethers::utils::keccak256(b"getFee(address,bool)")[..4];
    let data = [sel, &ethers::abi::encode(&[Token::Address(pool), Token::Bool(stable)])].concat();
    let res  = p.call(&tx_req(factory, data), None).await?;
    let fee  = U256::from_big_endian(&res).as_u32();
    Ok(fee.min(100))
}

// AUDIT 7.4/11 FIX: value the pool in USD, not raw token units. The old version
// returned `token_balance * 2` (e.g. a $600K WETH pool read as "200"), so every
// `min_tvl_usd` gate was meaningless — excluding good pools and admitting junk.
// These reference prices are COARSE and used ONLY for the inclusion gate; every
// executed trade is still sized/checked against live prices in the sim.
const ETH_REF_USD: f64 = 2_500.0;
const BTC_REF_USD: f64 = 65_000.0;

fn rough_usd_per_token(token: Address) -> f64 {
    let hex = format!("{:?}", token).to_lowercase();
    if hex.contains("833589") || hex.contains("d9aaec") || hex.contains("50c572") {
        1.0 // USDC / USDbC / DAI ≈ $1
    } else if hex.contains("cbb7c0") {
        BTC_REF_USD // cbBTC
    } else {
        ETH_REF_USD // WETH / cbETH / wstETH (all ~ETH-priced)
    }
}

async fn approx_tvl(p: &Arc<Provider<Ipc>>, token: Address, pool: Address) -> Result<f64> {
    let sel  = &ethers::utils::keccak256(b"balanceOf(address)")[..4];
    let data = [sel, &ethers::abi::encode(&[Token::Address(pool)])].concat();
    let res  = p.call(&tx_req(token, data), None).await?;
    let bal  = U256::from_big_endian(&res);
    let dec  = token_decimals(&format!("{:?}", token));
    let unit = 10f64.powi(dec as i32);
    let token_units = bal.as_u128() as f64 / unit;
    // one side × price × 2 (assume roughly balanced pool value)
    Ok(token_units * rough_usd_per_token(token) * 2.0)
}

fn tx_req(to: Address, data: Vec<u8>) -> ethers::types::transaction::eip2718::TypedTransaction {
    // ethers 2.0 Middleware::call takes &TypedTransaction, not &TransactionRequest.
    TransactionRequest { to: Some(to.into()), data: Some(data.into()), ..Default::default() }.into()
}

use ethers::abi::Token;
