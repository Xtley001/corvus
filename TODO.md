# Deployment TODO — Bare Metal

Everything to do to take Corvus from "compiles" to "safely earning" on a self-hosted Base node. Work top to bottom; do not skip a phase. Nothing here should be rushed — the cost of one silent revert loop or an unverified market ID dwarfs the time these steps take.

Legend: `[ ]` todo · `[~]` in progress · `[x]` done. Fill in the values in **‹angle brackets›** as you go.

---

## Phase 0 — Host & OS

- [ ] Provision the box: 8+ cores, 32 GB RAM, 2 TB NVMe, 1 Gbps (Ubuntu 22.04 LTS).
- [ ] Create a non-root `corvus` user; disable password SSH, key-only login.
- [ ] Firewall: allow SSH only; **never** expose `9090` (metrics) or the node RPC publicly.
- [ ] Set up unattended-security-updates and a swap/pagefile sized for build peaks.
- [ ] Time sync (`chrony`/`systemd-timesyncd`) — block timestamps matter.

## Phase 1 — Toolchain

- [ ] `rustup` stable 1.77+ (`rustup update stable`).
- [ ] Foundry (`foundryup`).
- [ ] Docker 24+ (monitoring stack only).
- [ ] Build the release binary for this exact CPU: `cd bot && RUSTFLAGS="-C target-cpu=native" cargo build --release --locked` (host-only flag — intentionally not committed, since it makes the binary non-portable).

## Phase 2 — Base node (op-geth + op-node)

- [ ] Follow [`NODE_SETUP.md`](./NODE_SETUP.md); sync fully (`eth.syncing == false`).
- [ ] **Move the IPC socket off `/tmp`** → e.g. `/var/run/base/geth.ipc`; update `ipc_path` in `bot/config/default.toml` (or `CORVUS_IPC_PATH`). If you keep it on `/tmp`, the systemd unit must stay `PrivateTmp=false`.
- [ ] Confirm the node exposes the `eth` + `txpool` namespaces on IPC and that `eth_getRawTransactionByHash` works (needed if S4 is ever enabled).
- [ ] Note: Base's public mempool is thin (single sequencer), so mempool-derived signal is sparse — treat it as bonus, not a core dependency. The reliable edge here is fast confirmed-block reaction + fast sequencer submission.

## Phase 3 — Build & static checks

- [ ] `cd bot && cargo build --release --locked` → exit 0.
- [ ] `cargo clippy --release --locked -- -D warnings` → clean.
- [ ] `cargo test --release --lib` → green.
- [ ] `cd contracts && forge build` → compiles (not verified on the audit host — do this here).
- [ ] Confirm the CI badge is green on GitHub Actions.

## Phase 4 — Contract tests (the release gate's core)

- [ ] `forge test --fork-url $BASE_RPC_URL -vv` passes.
- [ ] **Write and pass the three end-to-end fork tests** (proves the audit fixes actually land value):
  - [ ] Cross-DEX arb round-trip increases the executor's profit-token balance (proves the `amountIn=MAX` fix).
  - [ ] Liquidation collateral swap lands in the executor, not the router (proves the recipient fix).
  - [ ] Rate-arb open+close reconciles balances (only relevant once S5 opening is built).

## Phase 5 — Address & market-ID verification

- [ ] `BASE_RPC_URL=http://127.0.0.1:8545 ./scripts/verify_addresses.sh` → all ✓.
- [ ] Fill the two zero Morpho market IDs (`MORPHO_MARKET_CBETH_USDC`, `MORPHO_MARKET_WSTETH_USDC`) in `bot/src/shared/addresses.rs` from app.morpho.org/base; re-run the script until they pass.
- [ ] Confirm `FLASHLOAN_PREMIUM_TOTAL()` == 5 bps; if not, update the sim constant in `simulation.rs`.
- [ ] Record the verification **block number + date** next to each address (recreate a short `ADDRESS_AUDIT.md` or note it in the deploy log).

## Phase 6 — Wallets & keys

- [ ] Create the **owner** as a Gnosis Safe (or hardware wallet). This holds whitelist/sweep/rotation power and is immutable.
- [ ] Create a dedicated **executor** hot EOA (server-side, minimal ETH).
- [ ] Create a separate **cold wallet** (sweep destination).
- [ ] Keep the **deployer** key on a separate machine — it only pays deploy gas and gets no on-chain rights.
- [ ] Never store the deployer/owner key on the running bot host.

## Phase 7 — Deploy the contract

- [ ] Fill `bot/.env` from `bot/.env.example`: `OWNER_ADDRESS` (Safe), `EXECUTOR_ADDRESS`, `COLD_WALLET_ADDRESS`, `DEPLOYER_PRIVATE_KEY`, `BASESCAN_API_KEY`.
- [ ] `./scripts/deploy.sh` → note the deployed `FlashExecutor` address.
- [ ] Verify on BaseScan; confirm `owner()` == your Safe, `coldWallet()` correct, `AAVE_V3_POOL()` resolved.
- [ ] Re-run `verify_addresses.sh` against the **deployed** contract's whitelists.
- [ ] Set `CORVUS_FLASH_EXECUTOR_ADDRESS` to the deployed address.

## Phase 8 — Runtime config

- [ ] Review `bot/config/default.toml`. Keep **`jit_enabled=false`** and **`rate_arb_open_enabled=false`** (S4/S5-open ship disabled).
- [ ] Keep `builder_endpoints = []` (Base sequencer only; no L1 bundles).
- [ ] Set conservative gates for the canary: raise `min_profit_usd`, tight `pool_min_liquidity_usd`.
- [ ] Put all secrets in `/etc/corvus/corvus.env` (referenced by the systemd unit), mode `600`, owned by `corvus`.

## Phase 9 — Process supervision

- [ ] Install the unit: `sudo cp deploy/corvus.service /etc/systemd/system/`.
- [ ] `sudo systemctl daemon-reload && sudo systemctl enable corvus` (don't start yet).
- [ ] Confirm `StateDirectory=corvus` gives `/var/lib/corvus` for position persistence.
- [ ] Verify `Restart=on-failure` and `TimeoutStopSec=15` (lets the panic hook flush positions).

## Phase 10 — Monitoring & alerts

- [ ] `cd monitoring && GRAFANA_PASSWORD=‹pw› docker compose up -d`; confirm the Corvus dashboard loads.
- [ ] Confirm the metrics server binds `127.0.0.1` only.
- [ ] Set `CORVUS_TELEGRAM_BOT_TOKEN` + `CORVUS_TELEGRAM_CHAT_ID`; **send a test for all 6 alert conditions** (circuit breaker, low HF, low gas, panic, emergency unwind, sim divergence).
- [ ] Fund the executor gas reserve (~0.1 ETH); confirm the low-gas alert fires below 1 ETH.

## Phase 11 — Shadow mode (do NOT skip)

- [ ] Run for **≥1 week logging would-be trades but submitting nothing** (add a submit kill-switch or point `base_sequencer_rpc` at a black hole while you observe).
- [ ] Compare simulated profit vs. what would have happened on-chain; confirm the sim isn't systematically off.
- [ ] Watch circuit-breaker counters, IPC latency, and pool-registry health.

## Phase 12 — Canary → scale

- [ ] Enable **S1 only** with small size and tight profit floor; real capital, minimal exposure.
- [ ] Watch inclusion rate and realized vs. simulated profit for a week.
- [ ] Add S2, then S3, one at a time, each with its own observation window.
- [ ] Scale notionals gradually only after each strategy is net-positive on-chain.

## Phase 13 — Ongoing operations

- [ ] Sweep profit from the contract to the cold wallet on a schedule (`scripts/sweep.sh`) — don't let balances accumulate on-chain.
- [ ] Monitor `ExecutorProposed` / `Sweep` events on-chain for the owner key.
- [ ] Rotate the executor key periodically via `proposeExecutor()` → `acceptExecutor()` (24h timelock).
- [ ] Re-run `verify_addresses.sh` after any Base protocol upgrade or governance change.
- [ ] Keep `Cargo.lock` pinned; run `cargo audit` in CI; apply security updates.

---

## Deferred feature work (not required to run S1–S3, but on the roadmap)

- [ ] **S5 Morpho carry** — the open/close calldata only speaks Aave's ABI. Build Morpho-Blue-shaped `supply/borrow/repay/withdraw` calldata + fork tests before flipping `rate_arb_open_enabled`.
- [ ] **S4 Flash JIT rebuild** — fix Aerodrome/UniV3 pool-type handling and the heuristic profit/liquidity math; only enable if Base mempool signal proves worthwhile in shadow mode.
- [ ] **Live Aave flash premium** — read `FLASHLOAN_PREMIUM_TOTAL()` each block instead of the hardcoded 5 bps constant in `simulation.rs`.
- [ ] **alloy/revm migration** — `ethers-rs` is deprecated and unmaintained; migrate to `alloy` + current `revm` before scaling capital further.

---

## Release gate

Do not fund with meaningful capital until **all** of these are true:

- [ ] `cargo build --release --locked` and `cargo clippy -- -D warnings` are clean; unit tests pass.
- [ ] `forge build` compiles and the three end-to-end fork tests (Phase 4) pass on a Base fork.
- [ ] Every address and Morpho market ID is verified on-chain (Phase 5), recorded with block + date.
- [ ] `owner` is a multisig/hardware wallet; the deployer key is not on the bot host.
- [ ] All 6 Telegram alert conditions have been test-fired.
- [ ] A full shadow-mode week (Phase 11) shows simulated profit tracking on-chain reality.
- [ ] The S1-only canary (Phase 12) has run net-positive on small capital for a week.
