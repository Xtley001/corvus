# Corvus

Flash-loan MEV system for Base Mainnet — cross-DEX arbitrage, triangular arbitrage, and protocol liquidations executed atomically via flash loans.

[![CI](https://img.shields.io/github/actions/workflow/status/Xtley001/corvus/ci.yml?branch=main)](https://github.com/Xtley001/corvus/actions)
[![Rust](https://img.shields.io/badge/rust-stable-orange.svg)](https://www.rust-lang.org)
[![Solidity](https://img.shields.io/badge/solidity-0.8.24-363636.svg)](https://soliditylang.org)
[![License: MIT](https://img.shields.io/badge/license-MIT-blue.svg)](./LICENSE)

Corvus watches Base lending markets and DEX pools, borrows capital from Balancer, Morpho, or Aave, and exploits price dislocations and undercollateralised positions inside a single atomic transaction. If any leg is unprofitable the transaction reverts, so no trading capital sits at risk between trades. The Rust engine runs against a local `op-geth` node over IPC; the `FlashExecutor` contract enforces a profit gate, protocol/router whitelists, and a hardcoded cold-wallet sweep.


---

## Architecture

```
corvus/
├── bot/                    # Rust engine
│   └── src/
│       ├── main.rs         # block loop, strategy dispatch, circuit breakers
│       ├── shared/         # addresses, pool discovery, price feed/oracle,
│       │                   #   simulation, flash-loan router, submission, mempool
│       └── strategies/     # S1 cross-dex, S2 tri-arb, S3 liquidation,
│                           #   S4 JIT (disabled), S5 rate-arb
├── contracts/              # FlashExecutor (Foundry)
├── monitoring/             # Prometheus + Grafana stack
├── scripts/                # deploy.sh, sweep.sh, verify_addresses.sh
└── deploy/                 # corvus.service (systemd)
```

---

## Prerequisites

| Dependency | Version | Notes |
|---|---|---|
| Rust | stable 1.77+ | `rustup update stable` |
| Foundry | latest | `foundryup` |
| op-geth + op-node | latest tag | local IPC node — see [`NODE_SETUP.md`](./NODE_SETUP.md) |
| Docker | 24+ | monitoring stack only |

Hardware for a self-hosted node: 8+ cores, 32 GB RAM, 2 TB NVMe, 1 Gbps. See [`NODE_SETUP.md`](./NODE_SETUP.md).

---

## Build

```bash
# Rust engine
cd bot && cargo build --release --locked

# Contracts
cd ../contracts && forge build
```

---

## Configuration

All tunables live in [`bot/config/default.toml`](./bot/config/default.toml); override any value with a `CORVUS_`-prefixed env var. Secrets go in `bot/.env` (never commit it) — see [`bot/.env.example`](./bot/.env.example).

| Key | Purpose |
|---|---|
| `min_profit_usd` | Minimum net profit to execute any trade |
| `min_arb_spread_bps` | Spread floor for arb scans (dynamically raised at high gas) |
| `liquidation_hf_threshold` | Health factor below which positions are liquidated |
| `jit_enabled` | S4 Flash JIT — **default `false`** (rebuild + fork-test first) |
| `rate_arb_open_enabled` | S5 opening — **default `false`** (needs Morpho carry path) |
| `builder_endpoints` | **Empty** — Base has one sequencer; no L1 bundles |

---

## Deployment

Set `OWNER_ADDRESS` to a Gnosis Safe or hardware wallet — it holds all privileged rights and is immutable after deploy. The deployer key only pays gas and gets no on-chain rights.

```bash
cp bot/.env.example bot/.env   # fill in OWNER_ADDRESS, EXECUTOR_ADDRESS, COLD_WALLET_ADDRESS, ...
./scripts/deploy.sh
```

---

## Verification

Verify every hardcoded address and Morpho market ID against a synced node **before** funding:

```bash
BASE_RPC_URL=http://127.0.0.1:8545 ./scripts/verify_addresses.sh
```

## Testing

```bash
cd bot && cargo test --release --lib          # unit tests
cd contracts && forge test --fork-url "$BASE_RPC_URL" -vv   # fork tests (needs a Base RPC)
```

---

## Monitoring

```bash
cd monitoring && GRAFANA_PASSWORD=<pw> docker compose up -d
```

Prometheus on `:9091`, Grafana on `:3000`. The metrics server binds to `127.0.0.1` only.

---

## Strategies

| ID | Strategy | State |
|---|---|---|
| S1 | Cross-DEX arbitrage (Aerodrome ↔ UniV3) | Active |
| S2 | Triangular arbitrage (3-hop) | Active |
| S3 | Protocol liquidation (Aave V3 + Morpho Blue) | Active |
| S4 | Flash JIT liquidity | Disabled (`jit_enabled=false`) |
| S5 | Cross-protocol rate arbitrage | Unwind-only (`rate_arb_open_enabled=false`) |

---

## Documentation

| Doc | Contents |
|---|---|
| [`TODO.md`](./TODO.md) | Bare-metal deployment runbook (complete before funding) |
| [`SECURITY.md`](./SECURITY.md) | Access-control and key-capability model |
| [`NODE_SETUP.md`](./NODE_SETUP.md) | op-geth + op-node install |
| [`CONTRIBUTING.md`](./CONTRIBUTING.md) | Dev setup, CI gate, change rules |

---

## Security

The `FlashExecutor` enforces a profit gate, protocol/router whitelists, a two-step timelocked executor rotation, and an `emergencySweep` restricted to the hardcoded cold wallet. Report vulnerabilities per [`SECURITY.md`](./SECURITY.md) — do not open a public issue. This code has **not** been externally audited or fork-tested; review it yourself before production use.

## Contributing

See [`CONTRIBUTING.md`](./CONTRIBUTING.md) for dev setup, the CI gate, and change rules.

## License

Released under the MIT License.
