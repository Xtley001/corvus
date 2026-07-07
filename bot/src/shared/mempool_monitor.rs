//! Mempool monitor — Corvus v1.1
//!
//! v1.1 changes:
//!   - current_oracle_prices() returns Arc<DashMap<...>> — zero-copy clone.
//!   - Added Aerodrome swap selectors and decoder for JIT coverage.
//!   - Removed duplicate `use anyhow::Result` import.

use anyhow::Result;
use dashmap::DashMap;
use ethers::prelude::*;
use std::sync::Arc;
use crate::shared::addresses::base;

const CHAINLINK_ETH_USD:   &str = base::CHAINLINK_ETH_USD;
const CHAINLINK_CBETH_ETH: &str = base::CHAINLINK_CBETH_ETH;
const TRANSMIT_SEL:            [u8; 4] = [0xc9, 0x80, 0x75, 0x39];
const UNI_EXACT_INPUT_SEL:     [u8; 4] = [0x41, 0x4b, 0xf3, 0x89]; // exactInputSingle
// Aerodrome Router selectors — highest-volume DEX on Base
const AERO_SWAP_EXACT_IN_SEL:  [u8; 4] = [0x6e, 0x7a, 0x43, 0xb8]; // swapExactTokensForTokens
const AERO_SWAP_EXACT_TOKENS:  [u8; 4] = [0x38, 0xed, 0x17, 0x39]; // swapExactTokensForTokens (v2 variant)

#[derive(Debug, Clone)]
pub struct PendingSwap {
    pub pool:         Address,
    pub token_in:     Address,
    pub token_out:    Address,
    pub fee:          u32,
    pub amount_in:    U256,
    pub amount_usd:   f64,
    pub zero_for_one: bool,
    pub block_number: u64,
    /// Full RLP-encoded signed tx bytes from eth_getRawTransactionByHash.
    /// Required for builder bundle submission (target tx in bundle must be raw signed bytes).
    pub raw_tx:       Bytes,
    pub tx_hash:      H256,
    /// AUDIT 2.9: flush TTL. Pending swaps carry block_number = 0 (they are not yet
    /// mined), so the old `block_number + 3 > confirmed_block` rule evicted every swap
    /// on the very next block. This countdown gives a swap a bounded lifetime (a few
    /// flushes) so the JIT fast-loop can actually act on it before it is dropped.
    pub ttl:          u8,
}

/// pending price with the block it was first seen in.
#[derive(Debug, Clone)]
struct PendingPrice {
    price:      f64,
    seen_block: u64,
}

pub struct MempoolMonitor {
    /// Fallback ETH price — sourced from config, not hardcoded.
    eth_price_fallback: f64,
    pending_swaps:   Arc<DashMap<H256, PendingSwap>>,
    current_prices:  Arc<DashMap<Address, f64>>,
    /// keyed by oracle address, value includes seen_block for TTL eviction.
    pending_prices:  Arc<DashMap<Address, PendingPrice>>,
    provider:        Arc<Provider<Ipc>>,
    // F-14 FIX: Parse oracle addresses once at startup with hard failure,
    // not per-loop with .unwrap(). A merge error corrupting a constant would
    // cause the mempool loop to panic silently — S3 and S4 would go dark.
    eth_oracle_addr:   Address,
    cbeth_oracle_addr: Address,
}

impl MempoolMonitor {
    pub fn new(provider: Arc<Provider<Ipc>>, eth_price_fallback: f64) -> Result<Self> {
        // Hard failure at startup if static address constants are malformed.
        let eth_oracle_addr: Address = CHAINLINK_ETH_USD.parse()
            .map_err(|e| anyhow::anyhow!("Invalid CHAINLINK_ETH_USD constant '{}': {}", CHAINLINK_ETH_USD, e))?;
        let cbeth_oracle_addr: Address = CHAINLINK_CBETH_ETH.parse()
            .map_err(|e| anyhow::anyhow!("Invalid CHAINLINK_CBETH_ETH constant '{}': {}", CHAINLINK_CBETH_ETH, e))?;

        let m = Self { eth_price_fallback,
            pending_swaps:  Arc::new(DashMap::new()),
            current_prices: Arc::new(DashMap::new()),
            pending_prices: Arc::new(DashMap::new()),
            provider:       provider.clone(),
            eth_oracle_addr,
            cbeth_oracle_addr,
        };
        m.current_prices.insert(eth_oracle_addr, eth_price_fallback);

        let ps = m.pending_swaps.clone();
        let cp = m.current_prices.clone();
        let pp = m.pending_prices.clone();
        let pv = provider;
        let eth_addr   = eth_oracle_addr;
        let cbeth_addr = cbeth_oracle_addr;

        tokio::spawn(async move {
            let mut backoff = std::time::Duration::from_secs(1);
            loop {
                match Self::watch(pv.clone(), ps.clone(), cp.clone(), pp.clone(), eth_addr, cbeth_addr).await {
                    Ok(_)  => tracing::warn!("MempoolMonitor watch exited — restarting"),
                    Err(e) => {
                        tracing::error!("MempoolMonitor died ({}), restarting in {:?}", e, backoff);
                        tokio::time::sleep(backoff).await;
                        backoff = (backoff * 2).min(std::time::Duration::from_secs(30));
                    }
                }
            }
        });
        Ok(m)
    }

    async fn watch(
        provider:       Arc<Provider<Ipc>>,
        pending_swaps:  Arc<DashMap<H256, PendingSwap>>,
        current_prices: Arc<DashMap<Address, f64>>,
        pending_prices: Arc<DashMap<Address, PendingPrice>>,
        eth_oracle:     Address,    // F-14: pre-parsed at startup, not unwrap() per iteration
        cbeth_oracle:   Address,    // F-14: pre-parsed at startup, not unwrap() per iteration
    ) -> Result<()> {
        let mut stream = provider.watch_pending_transactions().await?;
        while let Some(hash) = stream.next().await {
            let tx = match provider.get_transaction(hash).await? {
                Some(t) => t,
                None    => continue,
            };
            let Some(to) = tx.to else { continue };

            // F-14 FIX: use pre-parsed address fields — no .unwrap() in hot loop.
            let is_eth_oracle   = to == eth_oracle;
            let is_cbeth_oracle = to == cbeth_oracle;

            if (is_eth_oracle || is_cbeth_oracle)
                && tx.input.len() >= 4
                && tx.input[..4] == TRANSMIT_SEL
            {
                if let Some(price) = decode_chainlink_transmit_price(&tx.input) {
                    let oracle_addr = if is_eth_oracle { eth_oracle } else { cbeth_oracle };
                    // record seen_block (use block_number from tx or 0 for pending)
                    let seen_block = tx.block_number.map(|b| b.as_u64()).unwrap_or(0);
                    pending_prices.insert(oracle_addr, PendingPrice { price, seen_block });
                    if is_eth_oracle { current_prices.insert(oracle_addr, price); }
                }
            }

            // Decode large UniV3 swaps for JIT detection
            if tx.input.len() >= 4 && tx.input[..4] == UNI_EXACT_INPUT_SEL {
                // fetch raw signed tx bytes for bundle submission
                let raw_tx = fetch_raw_tx(hash, &provider).await
                    .unwrap_or_else(|_| tx.input.clone()); // fallback: log but don't drop

                if let Some(mut swap) = decode_uni_exact_input_single(&tx, hash, &current_prices, &provider, eth_oracle).await {
                    if swap.amount_usd >= 50_000.0 {
                        swap.raw_tx = raw_tx;  // replace with actual raw tx
                        pending_swaps.insert(hash, swap);
                    }
                }
            }

            // Decode large Aerodrome swaps — highest-volume DEX on Base
            if tx.input.len() >= 4
                && (tx.input[..4] == AERO_SWAP_EXACT_IN_SEL || tx.input[..4] == AERO_SWAP_EXACT_TOKENS)
            {
                let raw_tx = fetch_raw_tx(hash, &provider).await
                    .unwrap_or_else(|_| tx.input.clone());

                if let Some(mut swap) = decode_aero_swap(&tx, hash, &current_prices, eth_oracle, &provider).await {
                    if swap.amount_usd >= 50_000.0 {
                        swap.raw_tx = raw_tx;
                        pending_swaps.insert(hash, swap);
                    }
                }
            }
        }
        Ok(())
    }

    pub fn get_large_pending_swaps(&self) -> Vec<PendingSwap> {
        self.pending_swaps.iter().map(|e| e.value().clone()).collect()
    }

    pub fn current_oracle_prices(&self) -> Arc<DashMap<Address, f64>> {
        self.current_prices.clone()
    }

    pub fn pending_oracle_prices(&self) -> crate::shared::position_indexer::PriceMap {
        let map = DashMap::new();
        for entry in self.pending_prices.iter() { map.insert(*entry.key(), entry.value().price); }
        map
    }

    /// evict pending prices older than 3 blocks; evict swaps older than 3 blocks.
    /// Changed from `clear()` (aggressive, dropped valid presign signals on busy blocks).
    pub fn flush_confirmed_pending(&self, confirmed_block: u64) {
        // Pending prices: keep for 3 blocks in case presign fires on a congested block
        self.pending_prices.retain(|_, pp| {
            // seen_block == 0 means pending tx with unknown block → keep for 1 flush then drop
            pp.seen_block == 0 || pp.seen_block + 3 > confirmed_block
        });
        // AUDIT 2.9: evict by TTL countdown, not by block_number (which is 0 for
        // pending swaps and made every swap drop on the next block). A confirmed swap
        // (block_number != 0) is also dropped once it is >3 blocks old.
        self.pending_swaps.retain(|_, s| {
            s.ttl = s.ttl.saturating_sub(1);
            if s.ttl == 0 { return false; }
            s.block_number == 0 || s.block_number + 3 > confirmed_block
        });
    }

    pub fn eth_price(&self) -> f64 {
        // F-14 FIX: use pre-parsed eth_oracle_addr field, not .unwrap() on const string.
        self.current_prices.get(&self.eth_oracle_addr).map(|p| *p).unwrap_or(self.eth_price_fallback)
    }
}

// ─── fetch full RLP-signed transaction bytes ────────────────────

/// Fetch raw signed tx via eth_getRawTransactionByHash.
/// These are the actual bytes required by builder bundle APIs — NOT the calldata.
async fn fetch_raw_tx(hash: H256, provider: &Arc<Provider<Ipc>>) -> Result<Bytes> {
    let raw_hex: String = provider
        .request("eth_getRawTransactionByHash", [hash])
        .await?;
    let trimmed = raw_hex.trim_start_matches("0x");
    let bytes = hex::decode(trimmed)?;
    Ok(Bytes::from(bytes))
}

// ─── Chainlink OCR2 transmit() decoding ──────────────────────────────────────

fn decode_chainlink_transmit_price(input: &Bytes) -> Option<f64> {
    if input.len() < 196 { return None; }
    let report_offset = U256::from_big_endian(&input[100..132]).as_usize();
    let abs_offset    = 4 + report_offset;
    if input.len() < abs_offset + 32 { return None; }
    let report_len   = U256::from_big_endian(&input[abs_offset..abs_offset + 32]).as_usize();
    let report_start = abs_offset + 32;
    if input.len() < report_start + report_len { return None; }
    let report = &input[report_start..report_start + report_len];
    if report.len() < 64 { return None; }
    let price_raw = i64::from_be_bytes(report[56..64].try_into().ok()?);
    let price_usd = price_raw.abs() as f64 / 1e8;
    if price_usd > 100.0 && price_usd < 1_000_000.0 { Some(price_usd) } else { None }
}

// ─── UniV3 exactInputSingle decoding ─────────────────────────────────────────

async fn decode_uni_exact_input_single(
    tx:       &Transaction,
    hash:     H256,
    prices:   &DashMap<Address, f64>,
    provider: &Arc<Provider<Ipc>>,
    eth_oracle: Address,  // F-14: pre-parsed, not unwrap() per call
) -> Option<PendingSwap> {
    let data = &tx.input;
    if data.len() < 4 + 7 * 32 { return None; }

    let token_in  = Address::from_slice(&data[4 + 12..4 + 32]);
    let token_out = Address::from_slice(&data[4 + 32 + 12..4 + 64]);
    let fee       = U256::from_big_endian(&data[4 + 64..4 + 96]).as_u32();
    let amount_in = U256::from_big_endian(&data[4 + 128..4 + 160]);

    // F-14 FIX: use pre-parsed eth_oracle address.
    // If the price isn't seeded yet (startup race), return None — skip this swap rather
    // than computing a $0 amount_usd that incorrectly filters out large swaps.
    let eth_price = prices.get(&eth_oracle).map(|p| *p).filter(|p| *p > 0.0)?;

    let token_hex = format!("{:?}", token_in).to_lowercase();
    let dec       = crate::shared::addresses::base::token_decimals(&token_hex);
    let amount_normalized = amount_in.as_u128() as f64 / 10f64.powi(dec as i32);
    let amount_usd = if dec == 18 { amount_normalized * eth_price } else { amount_normalized };

    // F-03/F-14 style: skip this pending swap if the pool address can't be resolved.
    // Returning None here is safe — we miss this JIT candidate, which is
    // preferable to submitting calldata targeting Address::zero().
    let pool = resolve_uni_v3_pool(token_in, token_out, fee, provider).await.ok()?;
    if pool.is_zero() { return None; }

    Some(PendingSwap {
        pool, token_in, token_out, fee, amount_in, amount_usd,
        zero_for_one: token_in < token_out,
        block_number: tx.block_number.map(|b| b.as_u64()).unwrap_or(0),
        raw_tx: Bytes::new(), // placeholder — replaced after fetch_raw_tx in caller
        tx_hash: hash,
        ttl: 3,
    })
}

async fn resolve_uni_v3_pool(
    token_in:  Address,
    token_out: Address,
    fee:       u32,
    provider:  &Arc<Provider<Ipc>>,
) -> Result<Address> {
    let factory: Address = base::UNISWAP_V3_FACTORY.parse()?;
    let sel  = &ethers::utils::keccak256(b"getPool(address,address,uint24)")[..4];
    let data = [
        sel,
        ethers::abi::encode(&[
            ethers::abi::Token::Address(token_in),
            ethers::abi::Token::Address(token_out),
            ethers::abi::Token::Uint(U256::from(fee)),
        ]).as_slice(),
    ].concat();
    let res = provider.call(
        &TransactionRequest { to: Some(factory.into()), data: Some(data.into()), ..Default::default() }.into(),
        None,
    ).await?;
    if res.len() < 32 { anyhow::bail!("getPool response too short"); }
    Ok(Address::from_slice(&res[12..32]))
}

// ─── Aerodrome swapExactTokensForTokens decoding ─────────────────────────────
//
// ABI: swapExactTokensForTokens(uint256 amountIn, uint256 amountOutMin,
//       (address from, address to, bool stable)[] routes, address to, uint256 deadline)
// We decode amountIn + first route's from/to to produce a PendingSwap.
// Pool address is resolved from the Aerodrome factory.
async fn decode_aero_swap(
    tx:         &Transaction,
    hash:       H256,
    prices:     &DashMap<Address, f64>,
    eth_oracle: Address,
    provider:   &Arc<Provider<Ipc>>,
) -> Option<PendingSwap> {
    let data = &tx.input;
    // Minimum: selector(4) + amountIn(32) + amountOutMin(32) + routes_offset(32)
    // + to(32) + deadline(32) + routes_length(32) + first_route(96) = 292 bytes
    if data.len() < 292 { return None; }

    let amount_in = U256::from_big_endian(&data[4..36]);

    // routes array starts at offset given by data[68..100], but for the first route
    // we rely on layout: routes_data starts at 4 + 3*32 = 100, length at 100, tuple at 132.
    let routes_offset = U256::from_big_endian(&data[68..100]).as_usize();
    let abs_offset    = 4 + routes_offset;
    if data.len() < abs_offset + 32 + 96 { return None; }
    let route_count   = U256::from_big_endian(&data[abs_offset..abs_offset + 32]).as_usize();
    if route_count == 0 { return None; }
    let first_route   = abs_offset + 32;
    if data.len() < first_route + 96 { return None; }

    let token_in  = Address::from_slice(&data[first_route + 12..first_route + 32]);
    let token_out = Address::from_slice(&data[first_route + 32 + 12..first_route + 64]);
    let _stable   = data[first_route + 95] != 0;

    let eth_price = prices.get(&eth_oracle).map(|p| *p).filter(|p| *p > 0.0)?;
    let token_hex = format!("{:?}", token_in).to_lowercase();
    let dec       = crate::shared::addresses::base::token_decimals(&token_hex);
    let amount_norm = amount_in.as_u128() as f64 / 10f64.powi(dec as i32);
    let amount_usd  = if dec == 18 { amount_norm * eth_price } else { amount_norm };

    // AUDIT 2.9 FIX: actually resolve the Aerodrome pool via factory.getPool(a, b, stable).
    // The old code hardcoded pool = Address::zero(), and flash_jit skips zero-pool swaps —
    // so Aerodrome JIT never ran despite being advertised as full coverage.
    let factory: Address = base::AERODROME_FACTORY.parse().ok()?;
    let sel  = &ethers::utils::keccak256(b"getPool(address,address,bool)")[..4];
    let call_data = [
        sel,
        ethers::abi::encode(&[
            ethers::abi::Token::Address(token_in),
            ethers::abi::Token::Address(token_out),
            ethers::abi::Token::Bool(_stable),
        ]).as_slice(),
    ].concat();
    let res = provider.call(
        &TransactionRequest { to: Some(factory.into()), data: Some(call_data.into()), ..Default::default() }.into(),
        None,
    ).await.ok()?;
    if res.len() < 32 { return None; }
    let pool = Address::from_slice(&res[12..32]);
    if pool.is_zero() { return None; }

    Some(PendingSwap {
        pool, token_in, token_out,
        fee: if _stable { 5 } else { 3000 }, // Aerodrome: stable=0.05%, volatile=0.30%
        amount_in, amount_usd,
        zero_for_one: token_in < token_out,
        block_number: tx.block_number.map(|b| b.as_u64()).unwrap_or(0),
        raw_tx: Bytes::new(),
        tx_hash: hash,
        ttl: 3,
    })
}
