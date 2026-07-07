#!/usr/bin/env bash
# Corvus v1.0 — contract deployment script
# Usage: ./scripts/deploy.sh
#
# Required environment variables (set in your shell or sourced .env):
#   DEPLOYER_PRIVATE_KEY    — deployer wallet private key (uint256)
#   EXECUTOR_ADDRESS        — hot wallet EOA for flash loan submission
#   COLD_WALLET_ADDRESS     — immutable sweep destination (baked into contract)
#   BASESCAN_API_KEY        — for BaseScan source verification

set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
REPO_ROOT="$(dirname "$SCRIPT_DIR")"
CONTRACTS_DIR="$REPO_ROOT/contracts"

echo "╔══════════════════════════════════════╗"
echo "║    Corvus v1.0 Contract Deploy       ║"
echo "║    Base Mainnet (Chain ID: 8453)     ║"
echo "╚══════════════════════════════════════╝"

# Validate required env vars
: "${DEPLOYER_PRIVATE_KEY:?Must set DEPLOYER_PRIVATE_KEY}"
: "${EXECUTOR_ADDRESS:?Must set EXECUTOR_ADDRESS}"
: "${COLD_WALLET_ADDRESS:?Must set COLD_WALLET_ADDRESS}"
: "${BASESCAN_API_KEY:?Must set BASESCAN_API_KEY}"

# Create position store directory for Phase 2+ persistence
echo "Ensuring position store directory exists..."
sudo mkdir -p /var/lib/corvus
sudo chown "$(id -u):$(id -g)" /var/lib/corvus
echo "  /var/lib/corvus ready"

# Verify node is synced
if ! /usr/local/bin/op-geth attach /tmp/base-geth.ipc --exec "eth.syncing" 2>/dev/null | grep -q "false"; then
    echo "ERROR: op-geth not synced or not running at /tmp/base-geth.ipc"
    echo "       Start op-geth + op-node and wait for full sync"
    exit 1
fi

BLOCK=$(/usr/local/bin/op-geth attach /tmp/base-geth.ipc --exec "eth.blockNumber" 2>/dev/null)
echo "Node synced at block: $BLOCK"

# Build and test
cd "$CONTRACTS_DIR"
echo "Building contracts..."
forge build --silent

echo "Running fork tests (requires BASE_RPC_URL)..."
BASE_RPC_URL="${BASE_RPC_URL:-http://127.0.0.1:8545}" \
  forge test --fork-url "${BASE_RPC_URL:-http://127.0.0.1:8545}" -vv
echo "All tests passed."

# Deploy
echo "Deploying FlashExecutor..."
DEPLOY_OUTPUT=$(forge script script/Deploy.s.sol \
  --rpc-url http://127.0.0.1:8545 \
  --broadcast \
  --verify \
  --etherscan-api-key "$BASESCAN_API_KEY" \
  -vvvv 2>&1)

echo "$DEPLOY_OUTPUT"

# Extract deployed addresses
EXECUTOR_ADDR=$(echo "$DEPLOY_OUTPUT" | grep "FlashExecutor:" | awk '{print $2}')
AAVE_POOL=$(echo "$DEPLOY_OUTPUT" | grep "Aave V3 Pool (resolved):" | awk '{print $5}')

if [[ -z "$EXECUTOR_ADDR" ]]; then
    echo "ERROR: Could not extract deployed FlashExecutor address from output"
    exit 1
fi

echo ""
echo "═══════════════════════════════════════════════"
echo "DEPLOYMENT SUCCESSFUL"
echo "FlashExecutor:          $EXECUTOR_ADDR"
echo "Aave V3 Pool (resolved):$AAVE_POOL"
echo "Cold Wallet:            $COLD_WALLET_ADDRESS"
echo "═══════════════════════════════════════════════"
echo ""
echo "Next steps:"
echo "  1. Add to your .env:"
echo "     CORVUS_FLASH_EXECUTOR_ADDRESS=$EXECUTOR_ADDR"
echo ""
echo "  2. Verify Aave pool resolved correctly:"
echo "     cast call $EXECUTOR_ADDR 'AAVE_V3_POOL()(address)' --rpc-url http://127.0.0.1:8545"
echo ""
echo "  3. Verify Morpho market IDs before enabling Phase 2+:"
echo "     See addresses.rs — MORPHO_MARKET_CBETH_USDC and MORPHO_MARKET_WSTETH_USDC"
echo "     require on-chain verification at app.morpho.org/base"
