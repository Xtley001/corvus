use ethers::types::U256;

// ─── Compile-time function selectors (LAT-07) ────────────────────────────────
// Avoids keccak256 recomputation on the hot path every block.
pub const SEL_GET_RESERVES: [u8; 4] = [0x09, 0x02, 0xf1, 0xac]; // getReserves()
pub const SEL_SLOT0:        [u8; 4] = [0x38, 0x50, 0xc7, 0xbd]; // slot0()
pub const SEL_LIQUIDITY:    [u8; 4] = [0x18, 0x16, 0x0d, 0xdd]; // liquidity()
pub const SEL_BALANCE_OF:   [u8; 4] = [0x70, 0xa0, 0x82, 0x31]; // balanceOf(address)
pub const SEL_TICK_SPACING: [u8; 4] = [0xd0, 0xc9, 0x3a, 0x70]; // tickSpacing()

/// Integer square root — Newton's method.
/// MED-08 fix: guard against U256::MAX overflow causing divide-by-zero panic.
pub fn isqrt(n: U256) -> U256 {
    if n.is_zero() { return U256::zero(); }
    // U256::MAX + 1 overflows to 0 — guard this edge case before Newton iterations
    if n == U256::MAX {
        // sqrt(2^256 - 1) ≈ 2^128 - 1
        return (U256::from(1u128) << 128) - U256::one();
    }
    let mut x = n;
    let mut y = (x + U256::one()) / 2;
    while y < x {
        x = y;
        y = (x + n / x) / 2;
    }
    x
}

/// Optimal arb input for zero-fee constant-product AMMs.
/// Use `optimal_arb_input_with_fees` for pools with real fees.
/// = sqrt(reserveA_in × reserveB_out) − reserveA_in, capped at max_pct%
pub fn optimal_arb_input(r_in: U256, r_out: U256, max_pct: u32) -> U256 {
    match r_in.checked_mul(r_out) {
        None    => U256::zero(),
        Some(p) => isqrt(p).saturating_sub(r_in).min(r_in * max_pct / 100),
    }
}

/// MED-01 fix: Optimal arb input accounting for real AMM fees on both legs.
///
/// Correct formula for constant-product pool with fees:
///   optimal_in = sqrt(r_in × r_out × (1-fee_in) × (1-fee_out)) / (1-fee_in) - r_in
///
/// Fees are in basis points (e.g. fee_in_bps=30 means 0.30%).
/// max_pct caps the result as a percentage of r_in.
pub fn optimal_arb_input_with_fees(
    r_in:       U256,
    r_out:      U256,
    fee_in_bps: u32,   // buy leg fee, e.g. 30 = 0.30%
    fee_out_bps:u32,   // sell leg fee, e.g. 5  = 0.05%
    max_pct:    u32,
) -> U256 {
    // Represent fees as 10_000 - fee_bps so 30bps → 9970/10000
    let fi = (10_000u128 - fee_in_bps as u128) as u64;   // e.g. 9970
    let fo = (10_000u128 - fee_out_bps as u128) as u64;  // e.g. 9995

    // Scale: adjusted_product = r_in * r_out * fi * fo / 10_000^2
    let adjusted = r_in
        .checked_mul(r_out)
        .and_then(|p| p.checked_mul(U256::from(fi)))
        .and_then(|p| p.checked_mul(U256::from(fo)));

    match adjusted {
        None => U256::zero(),
        Some(p) => {
            // Divide out the two 10_000 denominators
            let scaled = p / U256::from(100_000_000u128); // 10_000^2 = 1e8
            // sqrt(r_in * r_out * (1-fee_in) * (1-fee_out)) / (1-fee_in) - r_in
            let sqrt_val = isqrt(scaled);
            // Multiply by 10_000 / fi to un-scale the fee-in factor
            let numerator = sqrt_val
                .checked_mul(U256::from(10_000u64))
                .unwrap_or(U256::zero());
            let optimal = numerator / U256::from(fi);
            optimal.saturating_sub(r_in).min(r_in * max_pct / 100)
        }
    }
}

/// Simulate one leg of a constant-product AMM swap.
/// Returns the output amount for `amount_in` into a pool with reserves (r_in, r_out).
/// fee_bps: e.g. 30 for 0.30%.
pub fn amm_out(r_in: U256, r_out: U256, amount_in: U256, fee_bps: u32) -> U256 {
    if r_in.is_zero() || r_out.is_zero() || amount_in.is_zero() {
        return U256::zero();
    }
    // amount_in_with_fee = amount_in * (10_000 - fee_bps)
    let fee_num = U256::from(10_000u32 - fee_bps);
    let amount_in_fee = amount_in.checked_mul(fee_num).unwrap_or(U256::zero());
    // out = (amount_in_fee * r_out) / (r_in * 10_000 + amount_in_fee)
    let numerator   = amount_in_fee.checked_mul(r_out).unwrap_or(U256::zero());
    let denominator = r_in.checked_mul(U256::from(10_000u32))
        .unwrap_or(U256::MAX)
        .saturating_add(amount_in_fee);
    if denominator.is_zero() { U256::zero() } else { numerator / denominator }
}

/// PROFIT-01 / PROFIT-02: Simulate one StableSwap (Aerodrome stable pool) swap leg.
/// StableSwap uses x³y + xy³ = k invariant — flatter than constant-product near parity.
/// This is a Newton's-method approximation of the stable swap output.
pub fn stable_amm_out(r_in: U256, r_out: U256, amount_in: U256, fee_bps: u32) -> U256 {
    if r_in.is_zero() || r_out.is_zero() || amount_in.is_zero() {
        return U256::zero();
    }
    // Apply fee to input
    let fee_num = U256::from(10_000u32 - fee_bps);
    let amount_in_after_fee = amount_in.checked_mul(fee_num).unwrap_or(U256::zero()) / U256::from(10_000u32);
    let new_r_in = r_in + amount_in_after_fee;

    // k = x³y + xy³ = xy(x² + y²)
    // Compute k with original reserves
    let r_in_sq  = r_in.checked_mul(r_in).unwrap_or(U256::MAX);
    let r_out_sq = r_out.checked_mul(r_out).unwrap_or(U256::MAX);
    let sum_sq   = r_in_sq.saturating_add(r_out_sq);
    let k        = r_in.checked_mul(r_out).unwrap_or(U256::MAX)
                       .checked_mul(sum_sq).unwrap_or(U256::MAX);

    // Newton's method to find new_r_out given new_r_in and k
    // f(y) = x³y + xy³ - k = 0  →  y = k / (x*(x² + y²))  (iterated)
    let mut y = r_out;
    for _ in 0..64 {
        let y_sq  = y.checked_mul(y).unwrap_or(U256::MAX);
        let new_r_in_sq = new_r_in.checked_mul(new_r_in).unwrap_or(U256::MAX);
        let denom = new_r_in.checked_mul(new_r_in_sq.saturating_add(y_sq)).unwrap_or(U256::MAX);
        if denom.is_zero() { break; }
        let new_y = k / denom;
        if new_y >= y { break; } // converged
        let delta = y - new_y;
        y = new_y;
        if delta <= U256::one() { break; }
    }
    r_out.saturating_sub(y)
}

/// Tick floor to nearest tick_spacing (handles negative ticks correctly)
pub fn floor_tick(tick: i32, spacing: i32) -> i32 {
    let f = tick / spacing;
    if tick < 0 && tick % spacing != 0 { (f - 1) * spacing } else { f * spacing }
}

/// Tick ceiling to nearest tick_spacing.
/// Make negative-tick behaviour explicit rather than relying on
///              Rust truncation-toward-zero semantics (correct but fragile).
pub fn ceil_tick(tick: i32, spacing: i32) -> i32 {
    let c = tick / spacing;
    if tick > 0 && tick % spacing != 0 {
        // Positive, non-multiple: round up
        (c + 1) * spacing
    } else if tick < 0 && tick % spacing != 0 {
        // Negative, non-multiple: Rust truncates toward zero, so c*spacing IS the ceiling
        c * spacing
    } else {
        // Exact multiple or zero: already on boundary
        c * spacing
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn isqrt_basic() {
        assert_eq!(isqrt(U256::from(9u64)),   U256::from(3u64));
        assert_eq!(isqrt(U256::from(100u64)), U256::from(10u64));
        assert_eq!(isqrt(U256::zero()),       U256::zero());
    }

    // MED-08: U256::MAX must not panic
    #[test]
    fn isqrt_max_no_panic() {
        let r = isqrt(U256::MAX);
        // Should be approximately 2^128 - 1
        assert!(r > U256::from(u128::MAX / 2));
    }

    #[test]
    fn floor_tick_negative() {
        assert_eq!(floor_tick(-105, 10), -110);
        assert_eq!(floor_tick(-100, 10), -100);
        assert_eq!(floor_tick(105, 10),   100);
    }

    // explicit negative ceil_tick tests
    #[test]
    fn ceil_tick_negative() {
        assert_eq!(ceil_tick(-95,  10), -90);
        assert_eq!(ceil_tick(-100, 10), -100);
        assert_eq!(ceil_tick(-1,   10), 0);
        assert_eq!(ceil_tick(-101, 10), -100);
        assert_eq!(ceil_tick(1,    10), 10);
        assert_eq!(ceil_tick(10,   10), 10);
    }

    /// Regression: S1 reserve bug — passing the wrong reserve (token1 instead of token0
    /// for the sell pool) gives a dimensionally wrong result.
    /// correct call: (r_in=USDC_buy, r_out=USDC_sell) — same token, same units.
    /// buggy  call: (r_in=USDC_buy, r_out=WETH_sell)  — units don't match → wrong size.
    #[test]
    fn s1_reserve_bug_regression() {
        // Simulate USDC/WETH pools: USDC reserves ~1M, WETH reserves ~300 (≈$3k/ETH).
        // The sell pool holds MORE USDC than the buy pool, so an arb is actually
        // profitable and the optimal input is non-zero. (With near-balanced reserves
        // and 30+5bps fees the correct optimal input is legitimately 0 — no trade —
        // which is why the original 0.99e12 value made this regression assert fail.)
        let usdc_in_buy_pool  = U256::from(1_000_000_000_000u64); // $1M USDC (6 dec)
        let usdc_in_sell_pool = U256::from(1_100_000_000_000u64); // $1.1M USDC — real spread
        let weth_in_sell_pool = U256::from(        330_000_000_000_000_000u128); // ~330 WETH (18 dec)

        let correct_size = optimal_arb_input_with_fees(usdc_in_buy_pool, usdc_in_sell_pool, 30, 5, 30);
        let buggy_size   = optimal_arb_input_with_fees(usdc_in_buy_pool, weth_in_sell_pool, 30, 5, 30);

        // Correct result: a few thousand USDC (small relative to spread)
        // Buggy result:   sqrt(1e12 * 330e18) ≈ 1.8e16 — nonsensically large, in the wrong units
        assert!(correct_size > U256::zero(), "correct sizing must be non-zero");
        assert_ne!(correct_size, buggy_size, "same-token and mixed-token calls must differ");
        // The correct USDC size should be in sensible USDC range (6 decimals, < $500K)
        assert!(correct_size < U256::from(500_000_000_000u64), "correct size must be < $500K USDC");
        // The buggy size produces a number in the WETH-unit range — either huge or capped
        // Either way it's not equal to the correct one.
    }

    // F-02 FIX: Restored missing #[test] fn header. The body below was orphaned at
    // module scope inside mod tests — executable statements outside any fn block are
    // a compile error in Rust. The stray closing brace on the last line was also
    // removed (it prematurely closed mod tests, breaking amm_out_basic).
    #[test]
    fn fee_adjusted_optimal_is_smaller() {
        let r_in  = U256::from(1_000_000_000u64);
        let r_out = U256::from(1_500_000_000u64);
        let with_fees = optimal_arb_input_with_fees(r_in, r_out, 30, 30, 30);
        let without   = optimal_arb_input(r_in, r_out, 30);
        // With fees, optimal amount is smaller (fees reduce profitability)
        assert!(with_fees <= without, "fee-adjusted optimal must be ≤ zero-fee optimal");
    }

    #[test]
    fn amm_out_basic() {
        // 1 ETH in, 1000 USDC reserve, 1000 ETH reserve, 30bps fee
        let r_in  = U256::from(1_000u64);
        let r_out = U256::from(1_000_000u64); // 1000 ETH → 1M USDC reserves
        let out   = amm_out(r_in, r_out, U256::from(1u64), 30);
        assert!(out > U256::zero());
        assert!(out < r_out);
    }
}
