//! Transaction signing.
//! NEW-CRIT-01 fix: sign_tx_with_nonce() accepts explicit nonce from SubmissionPipeline.
//! NEW-CRIT-03 fix: gas pricing deferred to GasOracle — no static constants.
//! MED-05 fix: returns properly signed RLP bytes (unchanged).

use anyhow::Result;
use ethers::prelude::*;
use ethers::types::transaction::eip2718::TypedTransaction;
use ethers::signers::{LocalWallet, Signer as EthersSigner};
use std::str::FromStr;
use std::sync::Arc;
use crate::shared::gas_oracle::GasOracle;

pub struct Signer {
    wallet:      LocalWallet,
    chain_id:    u64,
    executor:    Address,   // FlashExecutor contract — the `to` field of every tx
    wallet_addr: Address,   // hot wallet EOA — used for nonce bootstrap
    gas_oracle:  Arc<GasOracle>,
}

impl Signer {
    /// Create a signer.
    /// `executor_addr` = deployed FlashExecutor (tx destination).
    pub fn new(
        private_key:   &str,
        chain_id:      u64,
        executor_addr: &str,
        gas_oracle:    Arc<GasOracle>,
    ) -> Result<Self> {
        let wallet   = LocalWallet::from_str(private_key)?.with_chain_id(chain_id);
        let wallet_addr = wallet.address();
        let executor = executor_addr.parse::<Address>()
            .map_err(|e| anyhow::anyhow!("Invalid executor address '{}': {}", executor_addr, e))?;
        Ok(Self { wallet, chain_id, executor, wallet_addr, gas_oracle })
    }

    /// Sign with an explicit nonce provided by SubmissionPipeline.
    /// NEW-CRIT-01: this is the canonical signing path — nonce comes from
    /// the AtomicU64 counter in SubmissionPipeline, not from a stale zero.
    pub async fn sign_tx_with_nonce(
        &self,
        data:      Bytes,
        gas_limit: u64,
        tip_mult:  f64,
        nonce:     u64,
    ) -> Result<(Bytes, H256)> {
        // NEW-CRIT-03: query live base fee via GasOracle (cached per block)
        let (priority_wei, max_fee_wei) = self.gas_oracle.max_fee_wei(tip_mult);

        let tx = Eip1559TransactionRequest::new()
            .to(self.executor)
            .data(data)
            .gas(gas_limit)
            .chain_id(self.chain_id)
            .max_priority_fee_per_gas(priority_wei)
            .max_fee_per_gas(max_fee_wei)
            .nonce(nonce);

        let typed = TypedTransaction::Eip1559(tx);
        let sig   = self.wallet.sign_transaction(&typed).await?;
        let rlp   = typed.rlp_signed(&sig);
        let hash  = H256::from(ethers::utils::keccak256(&rlp));
        Ok((rlp, hash))
    }

    /// Sign with escalated tip for resubmission (PROFIT-09).
    /// Caller MUST pass the same nonce used for previous attempts of the same tx.
    pub async fn sign_tx_escalated(
        &self,
        data:      Bytes,
        gas_limit: u64,
        attempt:   u8,
        nonce:     u64,
    ) -> Result<(Bytes, H256)> {
        let multiplier = 1.0_f64 + (attempt as f64 * 0.5);
        self.sign_tx_with_nonce(data, gas_limit, multiplier, nonce).await
    }

    /// Sign a message hash (used for Flashbots bundle auth header).
    pub async fn sign_message_hash(&self, hash: [u8; 32]) -> Result<ethers::types::Signature> {
        Ok(self.wallet.sign_message(&hash).await?)
    }

    /// Return the FlashExecutor contract address (tx destination).
    pub fn executor_address(&self) -> Address { self.executor }

    /// Return the hot wallet EOA address (used for nonce bootstrap).
    pub fn wallet_address(&self) -> Address { self.wallet_addr }
}
