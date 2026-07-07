//! Canonical Base Mainnet contract addresses  v1.0
//! All addresses verified against official on-chain sources:
//!   • Uniswap V3 Base Deployments (docs.uniswap.org)
//!   • Aerodrome GitHub (aerodrome-finance/contracts)
//!   • BaseScan verified contract pages
//!   • Chainlink data.chain.link/feeds/base
//!   • Morpho app.morpho.org/base
//!   • Aave deployed-contracts repo

pub mod base {
    // ── Flash loan providers ──────────────────────────────────────────────
    /// Balancer V2 Vault — same address on all EVM chains. Verified BaseScan.
    pub const BALANCER_VAULT: &str = "0xBA12222222228d8Ba445958a75a0704d566BF2C8";
    /// Morpho Blue — verified BaseScan ($4.2B TVL, Dec 2025).
    pub const MORPHO_BLUE:    &str = "0xBBBBBbbBBb9cC5e90e3b3Af64bdAF62C37EEFFCb";

    // ── Lending protocols ─────────────────────────────────────────────────
    /// Aave V3 Pool Addresses Provider — verified BaseScan, used by Aave Oracle V3.
    pub const AAVE_V3_ADDRESSES_PROVIDER: &str = "0xe20fCBdBfFC4Dd138cE8b2E6FBb6CB49777ad64D";
    /// Aave V3 Pool Proxy — resolved from provider.getPool(), verified BaseScan.
    pub const AAVE_V3_POOL_PROXY:         &str = "0xA238Dd80C259a72e81d7e4664a9801593F98d1c5";
    /// Morpho Adaptive IRM — used for market IRM lookup.
    pub const MORPHO_ADAPTIVE_IRM:        &str = "0x870aC11D48B15DB9a138Cf899d20F13F79Ba00BC";

    // ── DEX infrastructure ────────────────────────────────────────────────
    /// Verified: Uniswap V3 Base Deployments (docs.uniswap.org/contracts/v3/reference/deployments/base-deployments)
    pub const UNISWAP_V3_FACTORY:   &str = "0x33128a8fC17869897dcE68Ed026d694621f6FDfD";
    /// SwapRouter02 — verified Uniswap official Base deployments doc.
    pub const UNISWAP_V3_ROUTER:    &str = "0x2626664c2603336E57B271c5C0b26F421741e481";
    /// ADDR-CRIT-01 FIXED: QuoterV2. Was 0x3d4e44Eb...35f8 (WRONG).
    /// Correct value from official Uniswap Base deployments doc: 0x3d4e44Eb...76a
    pub const UNISWAP_V3_QUOTER_V2: &str = "0x3d4e44Eb1374240CE5F1B871ab261CD16335B76a";
    /// NonfungiblePositionManager — verified Uniswap official Base deployments doc.
    pub const UNI_POS_MANAGER:      &str = "0x03a520b32C04BF3bEEf7BEb72E919cf822Ed34f1";
    /// Aerodrome Router — verified BaseScan (1.7M+ txns), Aerodrome GitHub.
    pub const AERODROME_ROUTER:     &str = "0xcF77a3Ba9A5CA399B7c97c74d54e5b1Beb874E43";
    /// Aerodrome Pool Factory — verified BaseScan, Aerodrome GitHub.
    pub const AERODROME_FACTORY:    &str = "0x420DD381b31aEf6683db6B902084cB0FFECe40Da";

    // ── Pendle Finance ────────────────────────────────────────────────────
    /// Pendle Router V4 — same address cross-chain, verified on Base.
    pub const PENDLE_ROUTER: &str = "0x888888888889758F76e7103c6CbF23ABbF58F946";

    // ── Infrastructure ────────────────────────────────────────────────────
    /// Multicall3 — canonical address, same on all EVM chains.
    pub const MULTICALL3: &str = "0xcA11bde05977b3631167028862bE2a173976CA11";

    // ── Tokens (Base Mainnet) ─────────────────────────────────────────────
    /// WETH — Base protocol-native wrapped ETH.
    pub const WETH:   &str = "0x4200000000000000000000000000000000000006";
    /// USDC — Circle native USDC on Base (NOT USDbC). Verified BaseScan.
    pub const USDC:   &str = "0x833589fCD6eDb6E08f4c7C32D4f71b54bdA02913";
    /// cbETH — Coinbase Wrapped Staked ETH. Verified BaseScan.
    pub const CBETH:  &str = "0x2Ae3F1Ec7F1F5012CFEab0185bfc7aa3cf0DEc22";
    /// USDbC — Bridged USDC (legacy, largely deprecated in favour of native USDC).
    /// Kept for pair monitoring but TVL gating will filter it if depth < $200K.
    pub const USDBC:  &str = "0xd9aAEc86B65D86f6A7B5B1b0c42FFA531710b6CA";
    /// wstETH — Lido Wrapped Staked ETH on Base. Verified BaseScan.
    pub const WSTETH: &str = "0xc1CBa3fCea344f92D9239c08C0568f6F2F0ee452";
    /// cbBTC — Coinbase Wrapped BTC. Verified BaseScan.
    pub const CBBTC:  &str = "0xcbB7C0000aB88B473b1f5aFd9ef808440eed33Bf";
    /// DAI — Bridged DAI on Base. Low liquidity vs USDC — TVL gate will filter thin pools.
    pub const DAI:    &str = "0x50c5725949A6F0c72E6C4a641F24049A917DB0Cb";

    // ── Chainlink Price Feeds (Base Mainnet) ──────────────────────────────
    /// ETH/USD — verified data.chain.link/feeds/base/base/eth-usd → 0x7104...Bb70
    pub const CHAINLINK_ETH_USD:     &str = "0x71041dddad3595F9CEd3DcCFBe3D1F4b0a16Bb70";
    /// cbBTC/USD — verified data.chain.link/feeds/base/base/cbbtc-usd
    pub const CHAINLINK_CBBTC_USD:   &str = "0x07DA0E54543a844a80ABE69c8A12F22B3aA59f9D";
    /// cbETH/ETH — Chainlink rate feed on Base (data.chain.link/feeds/base/base/cbeth-eth).
    /// This is an ETH-denominated feed. To get cbETH/USD, multiply by CHAINLINK_ETH_USD:
    ///   cbeth_usd = cbeth_eth_price * eth_usd_price
    pub const CHAINLINK_CBETH_ETH:   &str = "0xd7818272B9e248357d13057AAb0B417aF31E817d";
    /// wstETH/ETH — rate feed on Base. Needed for composite wstETH/USD pricing.
    /// data.chain.link/feeds/base/base/wsteth-eth
    pub const CHAINLINK_WSTETH_ETH:  &str = "0xa669E5272E60f78299F4824495cE01a3923f4380";

    // ── Morpho Blue Market IDs (Base Mainnet) ─────────────────────────────
    //
    // ADDR-CRIT-02 FIX: All three prior market IDs were Ethereum mainnet IDs.
    // Using mainnet IDs on Base means idToMarketParams() always returns empty struct →
    // S3 (Liquidation) monitors zero Morpho positions → entirely blind on Morpho Base.
    //
    // Correct Base market IDs sourced from app.morpho.org/base/market/:
    //
    /// WETH collateral / USDC loan — highest TVL Morpho market on Base.
    /// Source: app.morpho.org/base/market/0x8793cf.../weth-usdc
    pub const MORPHO_MARKET_WETH_USDC: &str =
        "8793cf302b8ffd655ab97bd1c695dbd967807e8367a65cb2f4edaf1380ba1bda";
    /// USDC collateral / WETH loan — inverse direction.
    /// Source: app.morpho.org/base/market/0x3b3769.../usdc-weth
    pub const MORPHO_MARKET_USDC_WETH: &str =
        "3b3769cfca57be2eaed03fcc5299c25691b77781a1e124e7a8d520eb9a7eabb5";
    /// cbBTC collateral / USDC loan — largest cbBTC Morpho market on Base (86% LLTV).
    /// Source: app.morpho.org/base/market/0xf10437.../cbbtc-usdc
    pub const MORPHO_MARKET_CBBTC_USDC: &str =
        "f10437266b9dd52751bd6255e15cccd0cdf5c75b58c1a3e2621130c905cd8ed9";
    /// cbETH collateral / USDC loan.
    /// ⚠️  MUST VERIFY before enabling Phase 2+.
    /// Run: cast call 0xBBBBBbbBBb9cC5e90e3b3Af64bdAF62C37EEFFCb \
    ///   "idToMarketParams(bytes32)((address,address,address,address,uint256))" \
    ///   <candidate_id> --rpc-url $BASE_RPC
    /// MANDATORY BEFORE LIVE CAPITAL: MORPHO_MARKET_CBETH_USDC not yet verified.
    /// S3 Liquidation is blind to all cbETH/USDC Morpho positions until this is set.
    ///
    /// Fetch and verify:
    ///   1. Go to https://app.morpho.org/base — filter cbETH collateral + USDC loan
    ///   2. Copy the market ID (bytes32)
    ///   3. Verify on-chain:
    ///      cast call 0xBBBBBbbBBb9cC5e90e3b3Af64bdAF62C37EEFFCb \
    ///        "idToMarketParams(bytes32)((address,address,address,address,uint256))" \
    ///        <candidate_id> --rpc-url $BASE_RPC
    ///   4. If returned struct is non-zero, replace the zero string below.
    pub const MORPHO_MARKET_CBETH_USDC: &str =
        "0000000000000000000000000000000000000000000000000000000000000000";
    /// MANDATORY BEFORE LIVE CAPITAL: MORPHO_MARKET_WSTETH_USDC not yet verified.
    /// Same process as MORPHO_MARKET_CBETH_USDC above — filter wstETH collateral + USDC loan.
    pub const MORPHO_MARKET_WSTETH_USDC: &str =
        "0000000000000000000000000000000000000000000000000000000000000000";

    // ── Function selectors ────────────────────────────────────────────────
    pub const SEL_TOKEN0: [u8; 4] = [0x0d, 0xfe, 0x16, 0x81]; // token0()
    pub const SEL_TOKEN1: [u8; 4] = [0xd2, 0x10, 0x77, 0x25]; // token1()
    pub const SEL_FEE:    [u8; 4] = [0xdd, 0xca, 0x3f, 0x43]; // fee()

    // ── Per-asset helpers ─────────────────────────────────────────────────
    pub fn aave_liq_bonus(asset_lower_hex: &str) -> f64 {
        if asset_lower_hex.contains("420000") { return 0.05; }  // WETH
        if asset_lower_hex.contains("2ae3f1") { return 0.07; }  // cbETH
        if asset_lower_hex.contains("c1cba3") { return 0.07; }  // wstETH
        if asset_lower_hex.contains("cbb7c0") { return 0.10; }  // cbBTC
        0.075
    }

    pub fn token_decimals(asset_lower_hex: &str) -> u32 {
        if asset_lower_hex.contains("cbb7c0") { return 8; }    // cbBTC
        if asset_lower_hex.contains("833589") { return 6; }    // USDC
        if asset_lower_hex.contains("d9aaec") { return 6; }    // USDbC
        18
    }

    pub fn aave_ltv_liq_threshold(asset_lower_hex: &str) -> (f64, f64) {
        if asset_lower_hex.contains("420000") { return (0.80, 0.83); } // WETH
        if asset_lower_hex.contains("2ae3f1") { return (0.74, 0.79); } // cbETH
        if asset_lower_hex.contains("c1cba3") { return (0.75, 0.79); } // wstETH
        if asset_lower_hex.contains("833589") { return (0.77, 0.80); } // USDC
        if asset_lower_hex.contains("cbb7c0") { return (0.70, 0.75); } // cbBTC
        (0.80, 0.83)
    }
}
