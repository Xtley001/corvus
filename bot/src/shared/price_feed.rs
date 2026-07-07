//! Price feed — Multicall3 batch for all pool reads in one round-trip.
//!
//! PHASE 4 (Accuracy):
//!   - mc3_result_bytes() replaced with proper ethers::abi::decode (was broken heuristic parser)
//!     Old code assumed 160-byte fixed stride per result — wrong for dynamic ABI encoding.

use anyhow::Result;
use dashmap::DashMap;
use ethers::prelude::*;
use std::sync::Arc;
use crate::shared::pool_discovery::{PoolRegistry, ResolvedPool, DexType};
use crate::shared::math::{SEL_GET_RESERVES, SEL_SLOT0, SEL_LIQUIDITY};
use crate::shared::addresses::base;

#[derive(Debug, Clone)]
pub struct PoolState {
    pub address:           Address,
    pub token0:            Address,
    pub token1:            Address,
    pub reserve0:          U256,
    pub reserve1:          U256,
    pub fee_bps:           u32,
    pub dex:               DexType,
    pub sqrt_price_x96:    Option<U256>,
    pub current_tick:      Option<i32>,
    pub tick_spacing:      Option<i32>,
    pub liquidity:         Option<u128>,
    /// FIX (BUG-4): real Balancer V2 pool ID fetched via `getPoolId()` on the pool.
    /// None for UniV3/Aerodrome pools. Always Some(_) for DexType::Balancer.
    /// Pool IDs are opaque bytes32 — never derive them from the pool address.
    pub balancer_pool_id:  Option<[u8; 32]>,
}

impl PoolState {
    pub fn price_t1_per_t0(&self) -> f64 {
        if self.reserve0.is_zero() { return 0.0; }
        self.reserve1.as_u128() as f64 / self.reserve0.as_u128() as f64
    }
    pub fn price_t0_per_t1(&self) -> f64 {
        if self.reserve1.is_zero() { return 0.0; }
        self.reserve0.as_u128() as f64 / self.reserve1.as_u128() as f64
    }
    pub fn rate(&self, token_in: Address) -> f64 {
        if token_in == self.token0 { self.price_t1_per_t0() } else { self.price_t0_per_t1() }
    }
    pub fn fee_factor(&self) -> f64 { 1.0 - self.fee_bps as f64 / 10_000.0 }
}

#[derive(Clone)]
pub struct PriceFeed {
    states:    DashMap<Address, PoolState>,
    provider:  Arc<Provider<Ipc>>,
    registry:  Arc<tokio::sync::RwLock<PoolRegistry>>,
    multicall: Address,
}

impl PriceFeed {
    pub async fn new(
        provider: Arc<Provider<Ipc>>,
        registry: Arc<tokio::sync::RwLock<PoolRegistry>>,
    ) -> Result<Self> {
        let multicall = base::MULTICALL3.parse()?;
        let feed = Self { states: DashMap::new(), provider: provider.clone(), registry, multicall };
        feed.reload_batched(&provider).await?;
        Ok(feed)
    }

    pub async fn update(&mut self, block: u64, provider: Arc<Provider<Ipc>>) -> Result<()> {
        if block % 1000 == 0 {
            // PHASE 5 (LAT-2): pool refresh spawned as background task to avoid write-lock blocking
            let reg   = self.registry.clone();
            let prov  = provider.clone();
            tokio::spawn(async move {
                let new_reg = PoolRegistry::build(prov).await;
                if let Ok(r) = new_reg {
                    *reg.write().await = r;
                }
            });
        }
        if let Err(e) = self.reload_batched(&provider).await {
            tracing::warn!("Multicall3 price reload failed ({}), falling back to sequential", e);
            self.reload_sequential(&provider).await?;
        }
        Ok(())
    }

    async fn reload_batched(&self, provider: &Arc<Provider<Ipc>>) -> Result<()> {
        let pools = self.registry.read().await.all();
        if pools.is_empty() { return Ok(()); }

        let mut calls: Vec<(Address, Vec<u8>)> = Vec::new();
        for pool in &pools {
            match pool.dex {
                DexType::Aerodrome => { calls.push((pool.address, SEL_GET_RESERVES.to_vec())); }
                DexType::UniswapV3 => {
                    calls.push((pool.address, SEL_SLOT0.to_vec()));
                    calls.push((pool.address, SEL_LIQUIDITY.to_vec()));
                }
                DexType::Balancer => {}
            }
        }

        let mc3_calls: Vec<ethers::abi::Token> = calls.iter().map(|(addr, data)| {
            ethers::abi::Token::Tuple(vec![
                ethers::abi::Token::Address(*addr),
                ethers::abi::Token::Bool(true),
                ethers::abi::Token::Bytes(data.clone()),
            ])
        }).collect();

        let encoded = ethers::abi::encode(&[ethers::abi::Token::Array(mc3_calls)]);
        let sel      = &ethers::utils::keccak256(b"aggregate3((address,bool,bytes)[])")[..4];
        let calldata = [sel, encoded.as_slice()].concat();

        let res = provider.call(
            &TransactionRequest { to: Some(self.multicall.into()), data: Some(calldata.into()), ..Default::default() }.into(),
            None,
        ).await?;

        let mut call_idx = 0usize;
        for pool in &pools {
            match pool.dex {
                DexType::Aerodrome => {
                    if let Some(state) = extract_aero_state_from_mc3(&res, call_idx, pool) {
                        self.states.insert(pool.address, state);
                    }
                    call_idx += 1;
                }
                DexType::UniswapV3 => {
                    if let Some(state) = extract_uni_state_from_mc3(&res, call_idx, call_idx + 1, pool) {
                        self.states.insert(pool.address, state);
                    }
                    call_idx += 2;
                }
                DexType::Balancer => {}
            }
        }
        Ok(())
    }

    async fn reload_sequential(&self, provider: &Arc<Provider<Ipc>>) -> Result<()> {
        let pools = self.registry.read().await.all();
        for pool in pools {
            let state = match pool.dex {
                DexType::Aerodrome => fetch_aero_state(provider, &pool).await,
                DexType::UniswapV3 => fetch_uni_state(provider, &pool).await,
                DexType::Balancer  => continue,
            };
            if let Ok(s) = state { self.states.insert(pool.address, s); }
        }
        Ok(())
    }

    pub fn get_by_tokens(&self, t0: Address, t1: Address, dex: DexType) -> Option<PoolState> {
        self.states.iter().find(|e| {
            let s = e.value();
            s.dex == dex && ((s.token0 == t0 && s.token1 == t1) || (s.token0 == t1 && s.token1 == t0))
        }).map(|e| e.value().clone())
    }

    pub fn get(&self, addr: Address) -> Option<PoolState> {
        self.states.get(&addr).map(|s| s.clone())
    }
}

// ─── PHASE 4 FIX: proper Multicall3 ABI decode ───────────────────────────────
//
// Old code used a heuristic stride of 160 bytes per result, which does not match
// the actual dynamic ABI encoding of (bool, bytes)[] tuples.
// Fix: use ethers::abi::decode — identical to the correct implementation in flash_loan.rs.

fn mc3_result_bytes(mc3_res: &Bytes, idx: usize) -> Option<Vec<u8>> {
    use ethers::abi::{decode, ParamType};

    let result_type = ParamType::Array(Box::new(
        ParamType::Tuple(vec![ParamType::Bool, ParamType::Bytes])
    ));

    match decode(&[result_type], mc3_res) {
        Ok(tokens) => {
            let results = tokens.into_iter().next()?.into_array()?;
            if let ethers::abi::Token::Tuple(fields) = results.get(idx)? {
                // fields[0] = success bool, fields[1] = returnData bytes
                if fields[0] == ethers::abi::Token::Bool(true) {
                    if let ethers::abi::Token::Bytes(b) = &fields[1] {
                        return Some(b.clone());
                    }
                }
            }
            None
        }
        Err(e) => {
            tracing::debug!("mc3_result_bytes ABI decode failed at idx {}: {}", idx, e);
            None
        }
    }
}

fn decode_int24(slot0: &[u8], offset: usize) -> i32 {
    if slot0.len() < offset + 32 { return 0; }
    let b0 = slot0[offset + 29];
    let b1 = slot0[offset + 30];
    let b2 = slot0[offset + 31];
    let raw = ((b0 as u32) << 16) | ((b1 as u32) << 8) | (b2 as u32);
    if b0 & 0x80 != 0 { (raw | 0xFF00_0000) as i32 } else { raw as i32 }
}

fn extract_aero_state_from_mc3(res: &Bytes, idx: usize, pool: &ResolvedPool) -> Option<PoolState> {
    let data = mc3_result_bytes(res, idx)?;
    if data.len() < 64 { return None; }
    Some(PoolState {
        address: pool.address, token0: pool.token0, token1: pool.token1,
        reserve0: U256::from_big_endian(&data[0..32]),
        reserve1: U256::from_big_endian(&data[32..64]),
        fee_bps: pool.fee_bps, dex: DexType::Aerodrome,
        sqrt_price_x96: None, current_tick: None, tick_spacing: None, liquidity: None,
        balancer_pool_id: None,
    })
}

fn extract_uni_state_from_mc3(res: &Bytes, slot0_idx: usize, liq_idx: usize, pool: &ResolvedPool) -> Option<PoolState> {
    let slot0_data = mc3_result_bytes(res, slot0_idx)?;
    let liq_data   = mc3_result_bytes(res, liq_idx)?;
    if slot0_data.len() < 64 || liq_data.len() < 32 { return None; }

    let sqp  = U256::from_big_endian(&slot0_data[0..32]);
    let tick = decode_int24(&slot0_data, 32);
    let liq  = U256::from_big_endian(&liq_data[0..32]).as_u128();

    // Use full 256-bit divide for precision (PHASE 2 fix)
    let q96   = U256::from(1u128) << 96;
    let sq_hi = (sqp / q96).as_u128() as f64;
    let sq_lo = (sqp % q96).as_u128() as f64 / (1u128 << 96) as f64;
    let sq    = (sq_hi + sq_lo).max(1e-30);

    let reserve0 = U256::from((liq as f64 / sq) as u128);
    let reserve1 = U256::from((liq as f64 * sq) as u128);

    Some(PoolState {
        address: pool.address, token0: pool.token0, token1: pool.token1,
        reserve0, reserve1,
        fee_bps: pool.fee_bps, dex: DexType::UniswapV3,
        sqrt_price_x96: Some(sqp), current_tick: Some(tick),
        tick_spacing: None, liquidity: Some(liq),
        balancer_pool_id: None,
    })
}

async fn fetch_aero_state(provider: &Arc<Provider<Ipc>>, pool: &ResolvedPool) -> Result<PoolState> {
    let res = provider.call(
        &TransactionRequest { to: Some(pool.address.into()), data: Some(SEL_GET_RESERVES.to_vec().into()), ..Default::default() }.into(),
        None,
    ).await?;
    if res.len() < 64 { anyhow::bail!("getReserves too short"); }
    Ok(PoolState {
        address: pool.address, token0: pool.token0, token1: pool.token1,
        reserve0: U256::from_big_endian(&res[0..32]),
        reserve1: U256::from_big_endian(&res[32..64]),
        fee_bps: pool.fee_bps, dex: DexType::Aerodrome,
        sqrt_price_x96: None, current_tick: None, tick_spacing: None, liquidity: None,
        balancer_pool_id: None,
    })
}

async fn fetch_uni_state(provider: &Arc<Provider<Ipc>>, pool: &ResolvedPool) -> Result<PoolState> {
    let slot0 = provider.call(
        &TransactionRequest { to: Some(pool.address.into()), data: Some(SEL_SLOT0.to_vec().into()), ..Default::default() }.into(),
        None,
    ).await?;
    let liq_res = provider.call(
        &TransactionRequest { to: Some(pool.address.into()), data: Some(SEL_LIQUIDITY.to_vec().into()), ..Default::default() }.into(),
        None,
    ).await?;
    if slot0.len() < 64 { anyhow::bail!("slot0 too short"); }
    let sqp  = U256::from_big_endian(&slot0[0..32]);
    let tick = decode_int24(&slot0, 32);
    let liq  = U256::from_big_endian(&liq_res).as_u128();

    let q96   = U256::from(1u128) << 96;
    let sq_hi = (sqp / q96).as_u128() as f64;
    let sq_lo = (sqp % q96).as_u128() as f64 / (1u128 << 96) as f64;
    let sq    = (sq_hi + sq_lo).max(1e-30);

    Ok(PoolState {
        address: pool.address, token0: pool.token0, token1: pool.token1,
        reserve0: U256::from((liq as f64 / sq) as u128),
        reserve1: U256::from((liq as f64 * sq) as u128),
        fee_bps: pool.fee_bps, dex: DexType::UniswapV3,
        sqrt_price_x96: Some(sqp), current_tick: Some(tick),
        tick_spacing: None, liquidity: Some(liq),
        balancer_pool_id: None,
    })
}
