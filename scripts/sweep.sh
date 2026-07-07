#!/usr/bin/env bash
# sweep.sh — Corvus profit sweep script
# LOW-07 fix: confirmation prompt before irreversible sweep operation.
set -euo pipefail

# ── Env validation ─────────────────────────────────────────────────────────────
: "${CORVUS_HOT_WALLET:?CORVUS_HOT_WALLET env var not set}"
: "${CORVUS_COLD_WALLET:?CORVUS_COLD_WALLET env var not set}"
: "${CORVUS_FLASH_EXECUTOR:?CORVUS_FLASH_EXECUTOR env var not set}"
: "${BASE_RPC_URL:?BASE_RPC_URL env var not set}"

TOKEN="${1:-}"
if [[ -z "$TOKEN" ]]; then
    echo "Usage: $0 <TOKEN_ADDRESS>"
    echo "Example: $0 0x833589fCD6eDb6E08f4c7C32D4f71b54bdA02913   # USDC"
    exit 1
fi

# ── Query balance ──────────────────────────────────────────────────────────────
echo "Querying balance of $TOKEN at $CORVUS_FLASH_EXECUTOR..."
BALANCE=$(cast call "$TOKEN" "balanceOf(address)(uint256)" "$CORVUS_FLASH_EXECUTOR" \
    --rpc-url "$BASE_RPC_URL" 2>/dev/null || echo "0")

if [[ "$BALANCE" == "0" || -z "$BALANCE" ]]; then
    echo "Balance is zero. Nothing to sweep."
    exit 0
fi

# ── LOW-07: Confirmation prompt — prevents fat-finger sweeps ──────────────────
echo ""
echo "┌─────────────────────────────────────────────────────────────┐"
echo "│  SWEEP CONFIRMATION                                         │"
echo "│                                                             │"
printf "│  Token:    %-48s│\n" "$TOKEN"
printf "│  Balance:  %-48s│\n" "$BALANCE (raw)"
printf "│  From:     %-48s│\n" "$CORVUS_FLASH_EXECUTOR"
printf "│  To:       %-48s│\n" "$CORVUS_COLD_WALLET"
echo "│                                                             │"
echo "│  ⚠  This action is IRREVERSIBLE on-chain.                  │"
echo "└─────────────────────────────────────────────────────────────┘"
echo ""
read -r -p "Confirm sweep? [y/N] " CONFIRM
if [[ "${CONFIRM,,}" != "y" ]]; then
    echo "Aborted. No transaction submitted."
    exit 0
fi

# ── Secondary confirmation for large balances (>$10K equivalent) ──────────────
if (( BALANCE > 10_000_000_000 )); then   # >$10K if USDC (6 dec)
    echo ""
    echo "⚠  Large balance detected. Type 'CONFIRM' in caps to proceed:"
    read -r CAPS_CONFIRM
    if [[ "$CAPS_CONFIRM" != "CONFIRM" ]]; then
        echo "Aborted."
        exit 0
    fi
fi

# ── Execute sweep via cast send ────────────────────────────────────────────────
echo "Submitting sweep transaction..."
TX_HASH=$(cast send "$CORVUS_FLASH_EXECUTOR" \
    "sweep(address,address)" "$TOKEN" "$CORVUS_COLD_WALLET" \
    --private-key "$CORVUS_HOT_PRIVATE_KEY" \
    --rpc-url "$BASE_RPC_URL" \
    --json 2>&1 | jq -r '.transactionHash // empty')

if [[ -z "$TX_HASH" ]]; then
    echo "ERROR: Transaction submission failed. Check RPC connection and wallet balance."
    exit 1
fi

echo ""
echo "✓  Sweep submitted: $TX_HASH"
echo "   Track: https://basescan.org/tx/$TX_HASH"

# ── Wait for confirmation ──────────────────────────────────────────────────────
echo "Waiting for confirmation..."
cast receipt "$TX_HASH" --rpc-url "$BASE_RPC_URL" --confirmations 3 > /dev/null 2>&1 \
    && echo "✓  Confirmed (3 blocks)" \
    || echo "⚠  Could not confirm — check basescan manually"
