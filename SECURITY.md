# Security

The security model for the `FlashExecutor` contract and the Corvus engine. Report vulnerabilities privately (see [Disclosure](#disclosure)); never open a public issue.

---

## Access control

Three principals with strictly separated permissions.

| Principal | Controls | Where it should live |
|---|---|---|
| `owner` | Whitelists, executor rotation, `sweep` to any address, pause | Gnosis Safe / hardware wallet |
| `executor` | Execute strategies, `emergencySweep` to `coldWallet` | Hot server EOA |
| `coldWallet` | Immutable sweep destination | Separate cold wallet |

`owner`, `coldWallet`, and `AAVE_V3_POOL` are immutable. As of the audit remediation, `owner` is an explicit **constructor parameter** (`FlashExecutor(_owner, _executor, _coldWallet)`) — deploy from a throwaway key and set `owner` to a multisig from block 0. The deployer EOA receives no on-chain rights.

---

## Key capabilities

### Compromised `executor` key

- Can call `execute()` with any whitelisted protocol/router, and `emergencySweep(token)` (sends to `coldWallet` only).
- Cannot change `owner`, whitelists, or `coldWallet`; cannot propose a new executor; cannot sweep to an arbitrary address.
- **Response:** rotate via `proposeExecutor()` → `acceptExecutor()` (24-hour timelock). Funds remain sweepable only to `coldWallet`.

### Compromised `owner` key

- Can edit `allowedProtocols` / `allowedRouters`, propose a new executor (24-hour timelock), `sweep(token, to)` to any address, and `setPaused`.
- This is the critical path — use a hardware wallet or multisig, and monitor `ExecutorProposed` and `Sweep` events on-chain.

---

## Flash-loan callback security

Every callback is gated at two levels: a caller check and the `_inFlashLoan` guard set by `execute()`.

| Callback | Caller check |
|---|---|
| `receiveFlashLoan` | `msg.sender == BALANCER_VAULT` |
| `onMorphoFlashLoan` | `msg.sender == MORPHO_BLUE` |
| `executeOperation` | `msg.sender == AAVE_V3_POOL && initiator == address(this)` |

Swap calldata is constructed **on-chain** from live balances (recipient always `address(this)`), and router allowances are reset to `0` after each external call — no standing approvals persist on a contract that holds funds.

---

## Known accepted risks

| Risk | Severity | Notes |
|---|---|---|
| Executor key compromise | Medium | Sweeps only to `coldWallet`; rotate via 24h timelock |
| 24-hour executor timelock | Low-Medium | Consider longer for large exposure |
| Contract not upgradeable | Design | Bugs require redeploy; unwind positions before migration |
| `receive()` accepts ETH | Low | No logic depends on balance; `sweepETH` exists for recovery |
| Morpho market IDs unverified (cbETH/USDC, wstETH/USDC) | Operational | S3/S5 blind to those markets until IDs are set (zero-ID is safe) |
| Not externally audited or fork-tested | High | Complete the release gate in [`TODO.md`](./TODO.md#release-gate) before funding |

---

## Disclosure

Report security issues privately to the repository maintainer. Do not open a public GitHub issue or disclose the issue before a fix is deployed.
