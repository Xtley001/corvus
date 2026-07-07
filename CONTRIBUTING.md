# Contributing

Guidelines for working on Corvus. This is financial infrastructure that signs transactions with a hot key and moves real capital — correctness and review discipline matter more than velocity.

---

## Development setup

```bash
# Toolchain
rustup update stable            # Rust 1.77+
curl -L https://foundry.paradigm.xyz | bash && foundryup

# Build
cd bot && cargo build --release --locked
cd ../contracts && forge build
```

A synced local `op-geth` node (IPC) is required to run the bot and the fork tests. See [`NODE_SETUP.md`](./NODE_SETUP.md).

---

## Checks that must pass

Every change must be green on all of these before it is merged — the CI in [`.github/workflows/ci.yml`](./.github/workflows/ci.yml) enforces them:

| Check | Command |
|---|---|
| Build (pinned deps) | `cargo build --release --locked` |
| Lint (warnings are errors) | `cargo clippy --release --locked -- -D warnings` |
| Unit tests | `cargo test --release --locked --bins` |
| Contract build | `forge build` (run `forge install` first — see below) |
| Contract fork tests | `forge test --fork-url "$BASE_RPC_URL" -vv` |

Contracts need their libraries installed once (not vendored in the repo):

```bash
cd contracts
forge install foundry-rs/forge-std OpenZeppelin/openzeppelin-contracts@v4.9.6
```

CI runs the `bot` build + unit tests as the hard gate. Contract build/fork tests and `cargo audit` are run locally (they need external libs / a Base node). For a **bare-metal release binary**, build with `RUSTFLAGS="-C target-cpu=native"` on the deploy host — do not commit that flag (it makes the binary non-portable).

Never commit with a failing build. The three missing-constant errors caught in the audit existed only because nothing had ever been compiled — CI now makes that impossible, so keep it green.

---

## Change rules

- **`Cargo.lock` is committed** — update it deliberately, never let it drift. Do not add new `ethers-rs` usage; new code should be `alloy`-ready (see migration note in [`TODO.md`](./TODO.md)).
- **Addresses and market IDs** live only in `bot/src/shared/addresses.rs`. Any new one must pass `scripts/verify_addresses.sh` against a live node, recorded with block number + date.
- **Contract changes** require a matching Foundry fork test that asserts the executor's token balances move as intended. A contract PR without an end-to-end test is not reviewable.
- **New external calls** in `FlashExecutor` must gate on `allowedRouters` / `allowedProtocols` and reset approvals to zero after the call.
- **Strategy money-path changes** (sizing, calldata, profit gates) must state, in the PR description, how the change was verified on a fork — not just that it compiles.
- **Secrets** never enter the repo. `.env` is gitignored; double-check `git status` before every push.

---

## Commits and pull requests

- Write imperative, specific commit subjects (`Fix liquidation swap recipient`, not `updates`).
- Keep a PR focused on one concern; describe the failure it fixes and how you verified the fix.
- Reference the strategy (S1–S5) or contract path touched.

---

## Security

Do not open a public issue for a vulnerability. Follow the private disclosure process in [`SECURITY.md`](./SECURITY.md). Do not add exploit-enabling code paths (unrestricted external calls, bypassed profit gates, disabled whitelists) even behind a flag.
