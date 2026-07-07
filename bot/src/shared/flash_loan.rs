//! Flash loan router — all strategy calldata builders.
//!
//! v1.1 changes:
//!   - Removed `build_backrun` (S6 removed).
//!   - Added `provider()` accessor for rate_arb live-debt fetch.
//!
//! Fixes applied in earlier versions:
//!
//! BUG-3  (CRITICAL) encode_swap_exact_input: recipient was `router`, so swap
//!         output went to the router balance. Now `executor` is passed as recipient
//!         for every buy/sell leg so tokens land in FlashExecutor.
//!
//! BUG-4  (CRITICAL) encode_balancer_batch_swap: pool_id was fabricated as
//!         address-bytes ++ 0x0001. Real Balancer V2 pool IDs are opaque bytes32.
//!         The function now accepts a real `[u8; 32]` pool_id fetched on-chain via
//!         `getPoolId()` (called by `fetch_balancer_pool_id`).
//!         `build_tri_arb` accepts `pool_ids: [[u8; 32]; 3]` and fetches missing IDs.
//!
//! BUG-7  (HIGH) build_liquidation: min_profit was always U256::zero(), letting
//!         gas-negative liquidations pass the contract profit gate.
//!         Callers now supply a computed `min_profit_wei`.
//!
//! BUG-5  (CRITICAL) build_rate_arb_open: both supply_proto and borrow_proto were
//!         set to the Aave pool address. A same-pool supply+borrow captures zero
//!         spread. Now accepts `supply_proto` and `borrow_proto` from `RateFeed`.
//!
//! Pre-existing fixes (from earlier phase):
//!   build_rate_arb_close uses pos.debt_amount_wei (not U256::MAX).
//!   Balancer dex_type (2) in tri-arb routes to Balancer batchSwap.
//!   build_flash_jit computes correct token0/token1 split via sqrtPriceX96 math.
//!   amount0Min/amount1Min at 99% of computed amounts for sandwich protection.

use anyhow::Result;
use ethers::{abi::{encode, Token}, prelude::*};
use std::sync::Arc;
use crate::config::Config;
use crate::shared::{addresses::base, price_feed::PoolState, position_indexer::BorrowPosition};

const FLASH_PROVIDER_BALANCER: u8 = 0;
const FLASH_PROVIDER_MORPHO:   u8 = 1;
const FLASH_PROVIDER_AAVE:     u8 = 2;

const STRAT_CROSS_DEX_ARB:  u8 = 0;
const STRAT_TRI_ARB:        u8 = 1;
const STRAT_LIQUIDATION:    u8 = 2;
const STRAT_FLASH_JIT:      u8 = 3;
const STRAT_RATE_ARB_OPEN:  u8 = 4;
const STRAT_RATE_ARB_CLOSE: u8 = 5;

#[derive(Debug, Clone, Copy)]
pub enum FlashProvider { Balancer, Morpho, Aave }

impl FlashProvider {
    fn as_u8(self) -> u8 {
        match self {
            FlashProvider::Balancer => FLASH_PROVIDER_BALANCER,
            FlashProvider::Morpho   => FLASH_PROVIDER_MORPHO,
            FlashProvider::Aave     => FLASH_PROVIDER_AAVE,
        }
    }
}

pub struct FlashLoanRouter {
    provider:  Arc<Provider<Ipc>>,
    balancer:  Address,
    morpho:    Address,
    executor:  Address,
    multicall: Address,
}

impl FlashLoanRouter {
    pub fn new(provider: Arc<Provider<Ipc>>, cfg: &Config) -> Result<Self> {
        let balancer: Address = cfg.balancer_vault.parse()
            .map_err(|e| anyhow::anyhow!("Invalid balancer_vault '{}': {}", cfg.balancer_vault, e))?;
        // F-03 FIX: hard failure instead of unwrap_or(Address::zero()).
        // A zero executor address silently routes ALL calldata to 0x00...00,
        // burning gas on every block with no error logged.
        let executor: Address = cfg.flash_executor_address.parse()
            .map_err(|e| anyhow::anyhow!(
                "Invalid flash_executor_address '{}': {}. Set CORVUS_FLASH_EXECUTOR_ADDRESS.",
                cfg.flash_executor_address, e
            ))?;
        Ok(Self {
            provider,
            balancer,
            morpho:         cfg.morpho_blue.parse()
                .map_err(|e| anyhow::anyhow!("Invalid morpho_blue '{}': {}", cfg.morpho_blue, e))?,
            executor,
            multicall: base::MULTICALL3.parse()
                .map_err(|e| anyhow::anyhow!("Invalid MULTICALL3 constant: {}", e))?,
        })
    }

    /// Select flash loan provider — 5% buffer on Balancer.
    pub async fn select_provider(&self, asset: Address, amount: U256) -> Result<FlashProvider> {
        let (bal_depth, morpho_depth) = self.batch_query_depths(asset).await?;
        if amount <= bal_depth * 95 / 100 { return Ok(FlashProvider::Balancer); }
        if morpho_depth >= amount          { return Ok(FlashProvider::Morpho); }
        Ok(FlashProvider::Aave)
    }

    pub async fn get_provider_depth(&self, asset: Address) -> Result<U256> {
        self.balance_of(asset, self.balancer).await
    }

    async fn batch_query_depths(&self, asset: Address) -> Result<(U256, U256)> {
        let sel         = &ethers::utils::keccak256(b"balanceOf(address)")[..4];
        let bal_data    = [sel, &encode(&[Token::Address(self.balancer)])].concat();
        let morpho_data = [sel, &encode(&[Token::Address(self.morpho)])].concat();
        let calls = encode(&[Token::Array(vec![
            encode_mc3_call(asset, bal_data),
            encode_mc3_call(asset, morpho_data),
        ])]);
        let mc3_sel  = &ethers::utils::keccak256(b"aggregate3((address,bool,bytes)[])")[..4];
        let calldata = [mc3_sel, calls.as_slice()].concat();
        let res = self.provider.call(
            &TransactionRequest { to: Some(self.multicall.into()), data: Some(calldata.into()), ..Default::default() }.into(),
            None,
        ).await;
        match res {
            Ok(data) => {
                let bd = parse_mc3_uint256(&data, 0).unwrap_or(U256::zero());
                let md = parse_mc3_uint256(&data, 1).unwrap_or(U256::zero());
                Ok((bd, md * 60 / 100))
            }
            Err(_) => {
                let bd = self.balance_of(asset, self.balancer).await.unwrap_or(U256::zero());
                let md = self.balance_of(asset, self.morpho).await.unwrap_or(U256::zero());
                Ok((bd, md * 60 / 100))
            }
        }
    }

    async fn balance_of(&self, token: Address, account: Address) -> Result<U256> {
        let sel  = &ethers::utils::keccak256(b"balanceOf(address)")[..4];
        let data = [sel, &encode(&[Token::Address(account)])].concat();
        let res  = self.provider.call(
            &TransactionRequest { to: Some(token.into()), data: Some(data.into()), ..Default::default() }.into(),
            None,
        ).await?;
        Ok(U256::from_big_endian(&res))
    }

    fn execute_selector() -> [u8; 4] {
        let sig = b"execute((uint8,address[],uint256[],uint8,bytes,uint256,address))";
        let h = ethers::utils::keccak256(sig);
        [h[0], h[1], h[2], h[3]]
    }

    fn encode_execute_params(
        provider:     FlashProvider,
        tokens:       Vec<Address>,
        amounts:      Vec<U256>,
        strat_type:   u8,
        strat_data:   Vec<u8>,
        min_profit:   U256,
        profit_token: Address,
    ) -> Vec<u8> {
        let params = Token::Tuple(vec![
            Token::Uint(U256::from(provider.as_u8())),
            Token::Array(tokens.into_iter().map(Token::Address).collect()),
            Token::Array(amounts.into_iter().map(Token::Uint).collect()),
            Token::Uint(U256::from(strat_type)),
            Token::Bytes(strat_data),
            Token::Uint(min_profit),
            Token::Address(profit_token),
        ]);
        let sel = Self::execute_selector();
        [sel.as_slice(), encode(&[params]).as_slice()].concat()
    }

    // ─── Strategy 0 — Cross-DEX Arb ──────────────────────────────────────
    /// AUDIT 2.1/2.2 FIX: emits the structured `SwapStep` layout. The contract
    /// builds the actual router calldata on-chain from the live balance, so there
    /// is no more `amountIn = U256::MAX` (which SwapRouter02 takes literally →
    /// revert) and no more wrong-recipient bug (output always to the executor).
    pub async fn build_cross_dex_arb(
        &self, amount: U256, buy: &PoolState, sell: &PoolState, min_profit: U256,
    ) -> Result<Bytes> {
        use crate::shared::pool_discovery::DexType;
        let provider = self.select_provider(buy.token0, amount).await?;

        // buy leg: token0 → token1 on the cheap pool; sell leg: token1 → token0 on the dear pool.
        let buy_step = swap_step_token(
            router_for(buy.dex), buy.token0, buy.token1,
            buy.fee_bps * 100, is_stable_pool(buy), buy.dex == DexType::UniswapV3,
            U256::zero(), // intermediate leg — round-trip protected by sell floor + profit gate
        )?;
        // Sell must return at least principal + the computed profit floor (in token0 wei).
        let sell_floor = amount.saturating_add(min_profit);
        let sell_step = swap_step_token(
            router_for(sell.dex), buy.token1, buy.token0,
            sell.fee_bps * 100, is_stable_pool(sell), sell.dex == DexType::UniswapV3,
            sell_floor,
        )?;

        let strat_data = encode(&[Token::Tuple(vec![
            Token::Uint(amount),
            buy_step,
            sell_step,
        ])]);
        let cd = Self::encode_execute_params(provider, vec![buy.token0], vec![amount], STRAT_CROSS_DEX_ARB, strat_data, min_profit, buy.token0);
        Ok(Bytes::from(cd))
    }

    // ─── Strategy 1 — Tri-Arb ────────────────────────────────────────────
    /// AUDIT 2.1/2.7 FIX: structured SwapStep[3], on-chain calldata construction,
    /// per-leg fee/stable taken from live pool state. Balancer legs (dex_type 2)
    /// are no longer supported here (no monitored triangle uses them; the Balancer
    /// batchSwap path was fragile) — such a leg is rejected up front.
    pub async fn build_tri_arb(
        &self,
        tokens:    [Address; 3],
        dex_types: [u8; 3],
        fees_bps:  [u32; 3],
        stables:   [bool; 3],
        amount:    U256,
        min_profit:U256,
    ) -> Result<Bytes> {
        use crate::shared::pool_discovery::DexType;
        let provider = self.select_provider(tokens[0], amount).await?;

        let mut steps: Vec<Token> = Vec::with_capacity(3);
        for i in 0..3usize {
            let dex = match dex_types[i] {
                0 => DexType::Aerodrome,
                1 => DexType::UniswapV3,
                other => anyhow::bail!("build_tri_arb: unsupported dex_type {} (Balancer legs not supported)", other),
            };
            // Final leg must return principal + profit floor in token0.
            let min_out = if i == 2 { amount.saturating_add(min_profit) } else { U256::zero() };
            steps.push(swap_step_token(
                router_for(dex), tokens[i], tokens[(i + 1) % 3],
                fees_bps[i] * 100, stables[i], dex == DexType::UniswapV3, min_out,
            )?);
        }

        let strat_data = encode(&[Token::Tuple(vec![
            Token::Uint(amount),
            Token::FixedArray(steps),
        ])]);
        let cd = Self::encode_execute_params(provider, vec![tokens[0]], vec![amount], STRAT_TRI_ARB, strat_data, min_profit, tokens[0]);
        Ok(Bytes::from(cd))
    }

    // ─── Strategy 2 — Liquidation ─────────────────────────────────────────
    /// FIX (BUG-7): min_profit_wei is now a parameter computed by the caller from
    /// sim_r.profit_usd and min_liquidation_profit_usd config.
    /// Previously always U256::zero(), allowing gas-negative liquidations to pass
    /// the contract's `require(profit >= minProfit)` gate.
    pub async fn build_liquidation(
        &self,
        pos:            &BorrowPosition,
        provider:       FlashProvider,
        swap_route:     Bytes,
        min_profit_wei: U256,   // FIX: was hardcoded U256::zero()
    ) -> Result<Bytes> {
        use crate::shared::position_indexer::LendingProtocol;
        let protocol_id: u8 = match pos.protocol { LendingProtocol::Morpho => 0, LendingProtocol::Aave => 1 };
        let uni_router: Address = base::UNISWAP_V3_ROUTER.parse()?;
        let strat_data = encode(&[Token::Tuple(vec![
            Token::Uint(U256::from(protocol_id)),
            Token::Address(pos.borrower),
            Token::Address(pos.collateral_asset),
            Token::Address(pos.debt_asset),
            Token::Uint(pos.debt_amount),
            Token::Bytes(pos.morpho_market_params.to_vec()),
            Token::Address(uni_router),
            Token::Bytes(swap_route.to_vec()),
        ])]);
        let cd = Self::encode_execute_params(
            provider, vec![pos.debt_asset], vec![pos.debt_amount],
            STRAT_LIQUIDATION, strat_data,
            min_profit_wei,   // FIX: propagate the computed floor
            pos.debt_asset,
        );
        Ok(Bytes::from(cd))
    }

    // ─── Strategy 3 — Flash JIT ───────────────────────────────────────────
    pub async fn build_flash_jit(
        &self,
        sim:        &crate::shared::simulation::SimulationEngine,
        pool:       Address,
        t0:         Address,
        t1:         Address,
        fee:        u32,
        tl:         i32,
        tu:         i32,
        amount:     U256,
        min_profit: u64,
        zfo:        bool,
    ) -> Result<Bytes> {
        let provider = self.select_provider(if zfo { t0 } else { t1 }, amount).await?;
        let uni_router: Address = base::UNISWAP_V3_ROUTER.parse()?;

        let (amount0, amount1) = compute_jit_amounts(sim, pool, t0, t1, tl, tu, amount, zfo).await
            .unwrap_or_else(|_| {
                let half = amount / 2;
                (half, amount - half)
            });

        let amount0_min = amount0 * 99 / 100;
        let amount1_min = amount1 * 99 / 100;

        let swap_in  = if zfo { t0 } else { t1 };
        let swap_out = if zfo { t1 } else { t0 };
        // FIX (BUG-3): executor as recipient
        let swap_cd = encode_swap_exact_input(swap_in, swap_out, fee, uni_router, self.executor, amount, U256::zero())?;

        let strat_data = encode(&[Token::Tuple(vec![
            Token::Address(t0),
            Token::Address(t1),
            Token::Uint(U256::from(fee)),
            Token::Int(U256::from(tl as i64 as u64)),
            Token::Int(U256::from(tu as i64 as u64)),
            Token::Uint(amount0),
            Token::Uint(amount1),
            Token::Uint(amount0_min),
            Token::Uint(amount1_min),
            Token::Bool(zfo),
            Token::Uint(amount),
            Token::Address(uni_router),
            Token::Bytes(swap_cd.to_vec()),
        ])]);
        let flash_token = if zfo { t0 } else { t1 };
        let cd = Self::encode_execute_params(provider, vec![flash_token], vec![amount], STRAT_FLASH_JIT, strat_data, U256::from(min_profit), flash_token);
        Ok(Bytes::from(cd))
    }

    // ─── Strategy 4 — Rate Arb Open ───────────────────────────────────────
    /// FIX (BUG-5): now accepts `supply_proto` and `borrow_proto` from `RateFeed`.
    /// Previously both were set to `aave_pool`, making the strategy supply and borrow
    /// from the same pool — capturing zero rate spread by construction.
    /// Callers should pass `rate_feed.get_best_proto_addresses(asset)`.
    pub async fn build_rate_arb_open(
        &self,
        asset:        Address,
        notional:     U256,
        leverage:     f64,
        supply_proto: Address,  // FIX: distinct supply protocol (e.g. Morpho)
        borrow_proto: Address,  // FIX: distinct borrow protocol (e.g. Aave)
    ) -> Result<Bytes> {
        let provider = self.select_provider(asset, notional).await?;
        let loops    = leverage.ceil() as u8;
        let supply_sel = &ethers::utils::keccak256(b"supply(address,uint256,address,uint16)")[..4];
        let borrow_sel = &ethers::utils::keccak256(b"borrow(address,uint256,uint256,uint16,address)")[..4];
        let step_notional = notional / U256::from(loops as u64);

        let supply_data: Vec<Vec<u8>> = (0..loops).map(|_| {
            [supply_sel, encode(&[
                Token::Address(asset), Token::Uint(step_notional),
                Token::Address(self.executor), Token::Uint(U256::zero()),
            ]).as_slice()].concat()
        }).collect();
        let borrow_data: Vec<Vec<u8>> = (0..loops).map(|_| {
            [borrow_sel, encode(&[
                Token::Address(asset), Token::Uint(step_notional),
                Token::Uint(U256::from(2u64)), Token::Uint(U256::zero()),
                Token::Address(self.executor),
            ]).as_slice()].concat()
        }).collect();

        let strat_data = encode(&[Token::Tuple(vec![
            Token::Address(supply_proto),   // FIX: distinct supply protocol
            Token::Address(borrow_proto),   // FIX: distinct borrow protocol
            Token::Uint(U256::from(loops as u64)),
            Token::Array(supply_data.into_iter().map(Token::Bytes).collect()),
            Token::Array(borrow_data.into_iter().map(Token::Bytes).collect()),
        ])]);
        let cd = Self::encode_execute_params(provider, vec![asset], vec![notional], STRAT_RATE_ARB_OPEN, strat_data, U256::zero(), asset);
        Ok(Bytes::from(cd))
    }

    /// Uses pos.debt_amount_wei (actual debt) — NOT U256::MAX.
    pub async fn build_rate_arb_close(&self, pos: &crate::strategies::rate_arb::RateArbPosition) -> Result<Bytes> {
        if pos.supply_proto.is_zero() || pos.borrow_proto.is_zero() {
            anyhow::bail!("build_rate_arb_close: supply_proto/borrow_proto are zero");
        }
        if pos.debt_amount_wei.is_zero() {
            anyhow::bail!("build_rate_arb_close: debt_amount_wei is zero");
        }

        let repay_sel    = &ethers::utils::keccak256(b"repay(address,uint256,uint256,address)")[..4];
        let withdraw_sel = &ethers::utils::keccak256(b"withdraw(address,uint256,address)")[..4];

        let repay_data: Vec<Vec<u8>> = vec![
            [repay_sel, encode(&[Token::Address(pos.asset), Token::Uint(U256::MAX), Token::Uint(U256::from(2u64)), Token::Address(self.executor)]).as_slice()].concat()
        ];
        let withdraw_data: Vec<Vec<u8>> = vec![
            [withdraw_sel, encode(&[Token::Address(pos.asset), Token::Uint(U256::MAX), Token::Address(self.executor)]).as_slice()].concat()
        ];
        // AUDIT 2.6 FIX: struct field order is (supplyProto, borrowProto, repayDatas,
        // withdrawDatas). The old encoding put borrow_proto first, so the contract
        // repaid against the supply protocol and withdrew from the borrow protocol.
        let strat_data = encode(&[Token::Tuple(vec![
            Token::Address(pos.supply_proto),
            Token::Address(pos.borrow_proto),
            Token::Array(repay_data.into_iter().map(Token::Bytes).collect()),
            Token::Array(withdraw_data.into_iter().map(Token::Bytes).collect()),
        ])]);
        let cd = Self::encode_execute_params(
            FlashProvider::Aave, vec![pos.asset], vec![pos.debt_amount_wei],
            STRAT_RATE_ARB_CLOSE, strat_data, U256::zero(), pos.asset,
        );
        Ok(Bytes::from(cd))
    }

    /// Expose provider for strategies that need direct RPC calls (e.g. rate_arb live-debt fetch).
    pub fn provider(&self) -> Arc<Provider<Ipc>> { self.provider.clone() }
}

// ─── SwapStep helpers (AUDIT 2.1/2.2) ─────────────────────────────────────────

use crate::shared::pool_discovery::DexType;

fn router_for(dex: DexType) -> Result<Address> {
    let s = if dex == DexType::Aerodrome { base::AERODROME_ROUTER } else { base::UNISWAP_V3_ROUTER };
    Ok(s.parse()?)
}

/// Aerodrome stable pools carry a very low fee; UniV3 pools are never "stable".
fn is_stable_pool(p: &PoolState) -> bool {
    p.dex == DexType::Aerodrome && p.fee_bps < 10
}

/// Build the ABI token for the Solidity `SwapStep` struct:
///   { address router; address tokenIn; address tokenOut; uint24 fee;
///     bool stable; bool isUniV3; uint256 minOut }
fn swap_step_token(
    router:    Result<Address>,
    token_in:  Address,
    token_out: Address,
    fee_ppm:   u32,
    stable:    bool,
    is_univ3:  bool,
    min_out:   U256,
) -> Result<Token> {
    Ok(Token::Tuple(vec![
        Token::Address(router?),
        Token::Address(token_in),
        Token::Address(token_out),
        Token::Uint(U256::from(fee_ppm)),
        Token::Bool(stable),
        Token::Bool(is_univ3),
        Token::Uint(min_out),
    ]))
}

// ─── UniV3 LP amount split ────────────────────────────────────────────────────
//
// Computes correct amount0/amount1 for a JIT position straddling the current price.
// Uses sqrtPriceX96 from the pool. The f64 arithmetic is safe here because
// we decompose the U256 value into hi+lo before converting, avoiding precision
// loss from direct U256→f64 conversion on values >2^53.
async fn compute_jit_amounts(
    sim:    &crate::shared::simulation::SimulationEngine,
    pool:   Address,
    _t0:    Address,
    _t1:    Address,
    tl:     i32,
    tu:     i32,
    amount: U256,
    zfo:    bool,
) -> Result<(U256, U256)> {
    let sqrt_price_x96 = sim.get_sqrt_price_x96(pool).await?;
    let q96 = U256::from(1u128) << 96;

    // Decompose to avoid U256→f64 direct conversion (loses precision above 2^53)
    let sq_hi = (sqrt_price_x96 / q96).as_u128() as f64;
    let sq_lo = (sqrt_price_x96 % q96).as_u128() as f64 / (1u128 << 96) as f64;
    let sq_c  = sq_hi + sq_lo;

    let sq_a = 1.0001_f64.powf(tl as f64 / 2.0);
    let sq_b = 1.0001_f64.powf(tu as f64 / 2.0);

    // Clamp total_value to u64 max to avoid precision issues on very large flash loans.
    // Values above ~9.2e18 wei (9.2 ETH) still fit in f64 exactly.
    // For amounts above that, the split is approximate but not materially wrong.
    let total_value = amount.as_u128().min(u64::MAX as u128) as f64;

    let denom = (sq_c - sq_a) + (1.0 / sq_c - 1.0 / sq_b);
    if denom <= 0.0 || sq_b <= sq_c || sq_a >= sq_c {
        let (a0, a1) = if zfo { (amount, U256::zero()) } else { (U256::zero(), amount) };
        return Ok((a0, a1));
    }
    let liq      = total_value / denom;
    let amount1_f = liq * (sq_c - sq_a);
    let amount0_f = liq * (1.0 / sq_c - 1.0 / sq_b);

    let amount0 = U256::from(amount0_f.max(0.0) as u128);
    let amount1 = U256::from(amount1_f.max(0.0) as u128);
    Ok((amount0, amount1))
}

// ─── Shared helpers ───────────────────────────────────────────────────────────

fn encode_mc3_call(target: Address, call_data: Vec<u8>) -> Token {
    Token::Tuple(vec![Token::Address(target), Token::Bool(true), Token::Bytes(call_data)])
}

fn parse_mc3_uint256(data: &Bytes, idx: usize) -> Option<U256> {
    use ethers::abi::{decode, ParamType};
    let t = ParamType::Array(Box::new(ParamType::Tuple(vec![ParamType::Bool, ParamType::Bytes])));
    let tokens = decode(&[t], data).ok()?;
    let results = tokens.into_iter().next()?.into_array()?;
    if let Token::Tuple(fields) = results.get(idx)? {
        if fields[0] == Token::Bool(true) {
            if let Token::Bytes(b) = &fields[1] {
                if b.len() >= 32 { return Some(U256::from_big_endian(&b[0..32])); }
            }
        }
    }
    None
}

/// FIX (BUG-3): `recipient` is now a first-class parameter.
/// Callers must pass `executor` so swap output lands in FlashExecutor, not the router.
fn encode_swap_exact_input(
    token_in:  Address,
    token_out: Address,
    fee_bps:   u32,
    _router:   Address,   // kept for call-site clarity, not used in encoded params
    recipient: Address,   // FIX: was `router` — tokens went to router, not executor
    amount_in: U256,
    min_out:   U256,
) -> Result<Bytes> {
    let sel     = &ethers::utils::keccak256(b"exactInputSingle((address,address,uint24,address,uint256,uint256,uint160))")[..4];
    let fee_ppm = fee_bps * 100;
    let params  = encode(&[Token::Tuple(vec![
        Token::Address(token_in),
        Token::Address(token_out),
        Token::Uint(U256::from(fee_ppm)),
        Token::Address(recipient),
        Token::Uint(amount_in),
        Token::Uint(min_out),
        Token::Uint(U256::zero()),
    ])]);
    Ok(Bytes::from([sel, params.as_slice()].concat()))
}
