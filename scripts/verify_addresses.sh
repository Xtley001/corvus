#!/usr/bin/env bash
# AUDIT 7: on-chain verification of every hardcoded address and market ID.
# The codebase's own comments admit prior address failures (QuoterV2 was wrong; all
# three Morpho market IDs were once ETHEREUM IDs). NOTHING here is trusted until this
# script prints ✓ for it against a synced Base node.
#
# Usage:  BASE_RPC_URL=http://127.0.0.1:8545 ./scripts/verify_addresses.sh
set -uo pipefail

RPC="${BASE_RPC_URL:?set BASE_RPC_URL (http://127.0.0.1:8545 for a local node)}"
command -v cast >/dev/null || { echo "cast (foundry) required"; exit 1; }

pass=0; fail=0
ok(){ echo "  ✓ $1"; pass=$((pass+1)); }
no(){ echo "  ✗ $1"; fail=$((fail+1)); }

has_code(){ # address -> nonzero codesize
  local sz; sz=$(cast codesize "$1" --rpc-url "$RPC" 2>/dev/null || echo 0)
  [[ "${sz:-0}" =~ ^[0-9]+$ && "$sz" -gt 0 ]]
}

echo "── Core contracts (must have code) ─────────────────────────────"
declare -A C=(
  [BalancerVault]=0xBA12222222228d8Ba445958a75a0704d566BF2C8
  [MorphoBlue]=0xBBBBBbbBBb9cC5e90e3b3Af64bdAF62C37EEFFCb
  [AaveAddressesProvider]=0xe20fCBdBfFC4Dd138cE8b2E6FBb6CB49777ad64D
  [UniV3Factory]=0x33128a8fC17869897dcE68Ed026d694621f6FDfD
  [UniV3Router02]=0x2626664c2603336E57B271c5C0b26F421741e481
  [UniV3QuoterV2]=0x3d4e44Eb1374240CE5F1B871ab261CD16335B76a
  [UniPosManager]=0x03a520b32C04BF3bEEf7BEb72E919cf822Ed34f1
  [AerodromeRouter]=0xcF77a3Ba9A5CA399B7c97c74d54e5b1Beb874E43
  [AerodromeFactory]=0x420DD381b31aEf6683db6B902084cB0FFECe40Da
  [Multicall3]=0xcA11bde05977b3631167028862bE2a173976CA11
)
for name in "${!C[@]}"; do
  if has_code "${C[$name]}"; then ok "$name ${C[$name]}"; else no "$name ${C[$name]} — NO CODE"; fi
done

echo "── Aave pool resolves from provider ────────────────────────────"
POOL=$(cast call 0xe20fCBdBfFC4Dd138cE8b2E6FBb6CB49777ad64D "getPool()(address)" --rpc-url "$RPC" 2>/dev/null)
EXPECT=0xA238Dd80C259a72e81d7e4664a9801593F98d1c5
if [[ "${POOL,,}" == "${EXPECT,,}" ]]; then ok "getPool() == $EXPECT"; else no "getPool()=$POOL != $EXPECT (update AAVE_V3_POOL_PROXY)"; fi

echo "── Aave flash premium (sim uses 5 bps) ─────────────────────────"
PREM=$(cast call "$POOL" "FLASHLOAN_PREMIUM_TOTAL()(uint128)" --rpc-url "$RPC" 2>/dev/null || echo "?")
echo "  ℹ FLASHLOAN_PREMIUM_TOTAL = $PREM (expect 5). If not 5, update the sim constant."

echo "── Tokens (must have code) ─────────────────────────────────────"
declare -A T=(
  [WETH]=0x4200000000000000000000000000000000000006
  [USDC]=0x833589fCD6eDb6E08f4c7C32D4f71b54bdA02913
  [cbETH]=0x2Ae3F1Ec7F1F5012CFEab0185bfc7aa3cf0DEc22
  [USDbC]=0xd9aAEc86B65D86f6A7B5B1b0c42FFA531710b6CA
  [wstETH]=0xc1CBa3fCea344f92D9239c08C0568f6F2F0ee452
  [cbBTC]=0xcbB7C0000aB88B473b1f5aFd9ef808440eed33Bf
  [DAI]=0x50c5725949A6F0c72E6C4a641F24049A917DB0Cb
)
for name in "${!T[@]}"; do
  if has_code "${T[$name]}"; then ok "$name ${T[$name]}"; else no "$name — NO CODE"; fi
done

echo "── Chainlink feeds (latestRoundData must return) ───────────────"
declare -A F=(
  [ETH_USD]=0x71041dddad3595F9CEd3DcCFBe3D1F4b0a16Bb70
  [cbBTC_USD]=0x07DA0E54543a844a80ABE69c8A12F22B3aA59f9D
  [cbETH_ETH]=0xd7818272B9e248357d13057AAb0B417aF31E817d
  [wstETH_ETH]=0xa669E5272E60f78299F4824495cE01a3923f4380
)
for name in "${!F[@]}"; do
  ANS=$(cast call "${F[$name]}" "latestRoundData()(uint80,int256,uint256,uint256,uint80)" --rpc-url "$RPC" 2>/dev/null | sed -n '2p')
  if [[ -n "$ANS" && "$ANS" != "0" ]]; then ok "$name answer=$ANS"; else no "$name — no answer (wrong//dead feed?)"; fi
done

echo "── Morpho market IDs (idToMarketParams must be non-zero) ────────"
MB=0xBBBBBbbBBb9cC5e90e3b3Af64bdAF62C37EEFFCb
declare -A M=(
  [WETH_USDC]=0x8793cf302b8ffd655ab97bd1c695dbd967807e8367a65cb2f4edaf1380ba1bda
  [USDC_WETH]=0x3b3769cfca57be2eaed03fcc5299c25691b77781a1e124e7a8d520eb9a7eabb5
  [cbBTC_USDC]=0xf10437266b9dd52751bd6255e15cccd0cdf5c75b58c1a3e2621130c905cd8ed9
  # AUDIT 7.2: these two are still all-zeros in addresses.rs — fill from app.morpho.org/base
  [cbETH_USDC]=0x0000000000000000000000000000000000000000000000000000000000000000
  [wstETH_USDC]=0x0000000000000000000000000000000000000000000000000000000000000000
)
for name in "${!M[@]}"; do
  id="${M[$name]}"
  if [[ "$id" =~ ^0x0+$ ]]; then no "$name — UNSET placeholder (S3/S5 blind to this market)"; continue; fi
  OUT=$(cast call "$MB" "idToMarketParams(bytes32)(address,address,address,address,uint256)" "$id" --rpc-url "$RPC" 2>/dev/null | head -1)
  if [[ -n "$OUT" && "$OUT" != "0x0000000000000000000000000000000000000000" ]]; then ok "$name loanToken=$OUT"; else no "$name — empty struct (wrong ID)"; fi
done

echo
echo "──────────────────────────────────────────────"
echo "PASS: $pass    FAIL/UNSET: $fail"
echo "Record the block number + date next to each ✓ in ADDRESS_AUDIT.md before funding."
cast block-number --rpc-url "$RPC" 2>/dev/null | sed 's/^/verified at block: /'
[[ "$fail" -eq 0 ]]
