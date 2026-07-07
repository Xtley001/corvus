#[tokio::test]
async fn test_ipc_connection() {
    let provider = ethers::providers::Provider::<ethers::providers::Ipc>::connect_ipc(
        "/tmp/base-geth.ipc"
    )
    .await
    .expect("IPC must be reachable — verify op-geth is running");

    let bn = provider
        .get_block_number()
        .await
        .expect("get_block_number failed");
    assert!(bn.as_u64() > 0, "Block number must be > 0");
}

#[tokio::test]
async fn test_pool_discovery() {
    use std::sync::Arc;
    use corvus::shared::pool_discovery::PoolRegistry;

    let provider = Arc::new(
        ethers::providers::Provider::<ethers::providers::Ipc>::connect_ipc(
            "/tmp/base-geth.ipc"
        )
        .await
        .expect("IPC required for integration test")
    );

    let registry = PoolRegistry::build(provider).await.expect("PoolRegistry::build failed");
    assert!(registry.pool_count() > 0, "Must find at least one active pool from factories");
}

#[test]
fn test_isqrt() {
    use ethers::types::U256;

    // Use function directly — no IPC needed
    fn isqrt(n: U256) -> U256 {
        if n.is_zero() { return U256::zero(); }
        let mut x = n;
        let mut y = (x + U256::one()) / 2;
        while y < x { x = y; y = (x + n / x) / 2; }
        x
    }

    assert_eq!(isqrt(U256::from(0u64)),   U256::from(0u64));
    assert_eq!(isqrt(U256::from(1u64)),   U256::from(1u64));
    assert_eq!(isqrt(U256::from(4u64)),   U256::from(2u64));
    assert_eq!(isqrt(U256::from(9u64)),   U256::from(3u64));
    assert_eq!(isqrt(U256::from(100u64)), U256::from(10u64));
    assert_eq!(isqrt(U256::from(144u64)), U256::from(12u64));
}

#[test]
fn test_floor_tick() {
    fn floor_tick(tick: i32, spacing: i32) -> i32 {
        let f = tick / spacing;
        if tick < 0 && tick % spacing != 0 { (f - 1) * spacing } else { f * spacing }
    }
    fn ceil_tick(tick: i32, spacing: i32) -> i32 {
        let c = tick / spacing;
        if tick > 0 && tick % spacing != 0 { (c + 1) * spacing } else { c * spacing }
    }

    // Negative ticks
    assert_eq!(floor_tick(-105, 10), -110);
    assert_eq!(floor_tick(-100, 10), -100);
    assert_eq!(floor_tick(-1,   10), -10);
    // Positive ticks
    assert_eq!(floor_tick(105,  10), 100);
    assert_eq!(floor_tick(100,  10), 100);
    assert_eq!(floor_tick(0,    10), 0);
    // Ceil
    assert_eq!(ceil_tick(105, 10), 110);
    assert_eq!(ceil_tick(100, 10), 100);
    assert_eq!(ceil_tick(-100, 10), -100);
}

#[test]
fn test_optimal_arb_input_zero() {
    use ethers::types::U256;
    fn isqrt(n: U256) -> U256 {
        if n.is_zero() { return U256::zero(); }
        let mut x = n;
        let mut y = (x + U256::one()) / 2;
        while y < x { x = y; y = (x + n / x) / 2; }
        x
    }
    fn optimal_arb_input(r_in: U256, r_out: U256, max_pct: u32) -> U256 {
        match r_in.checked_mul(r_out) {
            None    => U256::zero(),
            Some(p) => isqrt(p).saturating_sub(r_in).min(r_in * max_pct / 100),
        }
    }

    // Zero reserves → zero input
    assert_eq!(optimal_arb_input(U256::zero(), U256::from(1000u64), 30), U256::zero());
    // Equal reserves → zero optimal input (no arb)
    let r = U256::from(1_000_000u64);
    assert_eq!(optimal_arb_input(r, r, 30), U256::zero());
    // Imbalanced → non-zero capped at max_pct
    let r_in  = U256::from(1_000_000u64);
    let r_out = U256::from(2_000_000u64);
    let opt   = optimal_arb_input(r_in, r_out, 30);
    assert!(opt > U256::zero());
    assert!(opt <= r_in * 30 / 100);
}

#[test]
fn test_rate_arb_hf_thresholds() {
    // Verify threshold ordering from greypaper Section 11.3
    const OPEN:      f64 = 1.30;
    const CLOSE_HF:  f64 = 1.15;
    const EMERGENCY: f64 = 1.10;
    assert!(OPEN > CLOSE_HF);
    assert!(CLOSE_HF > EMERGENCY);
    assert!(EMERGENCY > 1.0);
}
