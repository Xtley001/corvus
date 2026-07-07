//!
//! Builder responses are now parsed — rejections logged by error code and reason.
//! record_inclusion called per receipt confirmation — feeds inclusion_rate metric.

use anyhow::Result;
use ethers::prelude::*;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use crate::{config::Config, shared::signing::Signer};

pub struct SubmissionPipeline {
    signer:            Arc<Signer>,
    provider:          Arc<Provider<Ipc>>,
    sequencer_rpc:     String,
    builder_endpoints: Vec<String>,
    client:            reqwest::Client,
    nonce:             Arc<AtomicU64>,
}

impl SubmissionPipeline {
    pub async fn new(cfg: &Config, signer: Arc<Signer>, provider: Arc<Provider<Ipc>>) -> Result<Self> {
        let client = reqwest::Client::builder()
            .tcp_nodelay(true)
            .timeout(std::time::Duration::from_secs(cfg.rpc_timeout_secs))
            .build()?;

        let wallet_addr = signer.wallet_address();
        let nonce = provider
            .get_transaction_count(wallet_addr, Some(BlockId::Number(BlockNumber::Pending)))
            .await?.as_u64();
        tracing::info!("Nonce bootstrap: wallet={:?} nonce={}", wallet_addr, nonce);

        Ok(Self {
            signer, provider,
            sequencer_rpc:     cfg.base_sequencer_rpc.clone(),
            builder_endpoints: cfg.builder_endpoints.clone(),
            client,
            nonce: Arc::new(AtomicU64::new(nonce)),
        })
    }

    pub async fn submit(&self, calldata: Bytes, gas_limit: u64) -> Result<H256> {
        self.submit_with_tip(calldata, gas_limit, 1.0).await
    }

    pub async fn submit_priority(&self, calldata: Bytes, gas_limit: u64) -> Result<H256> {
        self.submit_with_tip(calldata, gas_limit, 2.5).await
    }

    pub async fn submit_with_escalation(
        &self, calldata: Bytes, gas_limit: u64, max_blocks: u8,
    ) -> Result<H256> {
        let nonce = self.nonce.fetch_add(1, Ordering::SeqCst);
        let mut last = H256::zero();
        for attempt in 0..max_blocks {
            let (raw, hash) = self.signer.sign_tx_escalated(calldata.clone(), gas_limit, attempt, nonce).await?;
            self.broadcast_raw(&raw).await;
            last = hash;
            tokio::time::sleep(std::time::Duration::from_secs(2)).await;
            // AUDIT 2.11 FIX: check the RECEIPT (mined), not get_transaction (which also
            // returns pending txs) — otherwise we recorded inclusion for un-mined txs and
            // stopped escalating prematurely.
            if let Ok(Some(_)) = self.provider.get_transaction_receipt(hash).await {
                tracing::info!("Tx included after {} attempt(s): {:?}", attempt + 1, hash);
                crate::monitoring::metrics::record_inclusion();
                return Ok(hash);
            }
        }
        // FIX: function body was never closed — missing return, for-loop brace, and fn brace.
        // The compile error swallowed submit_jit_bundle into this broken function scope.
        Ok(last)
    }

    /// Submit JIT bundle. CRIT-NEW-03: target_raw must be full RLP signed bytes (not calldata).
    pub async fn submit_jit_bundle(
        &self, jit_calldata: Bytes, target_raw: Bytes, block: u64,
    ) -> Result<()> {
        let nonce = self.nonce.fetch_add(1, Ordering::SeqCst);
        let (signed_raw, _) = self.signer.sign_tx_with_nonce(jit_calldata, 500_000, 1.0, nonce).await?;

        let bundle = serde_json::json!({
            "jsonrpc": "2.0", "id": 1,
            "method": "eth_sendBundle",
            "params": [{
                "txs": [
                    format!("0x{}", hex::encode(&signed_raw)),
                    format!("0x{}", hex::encode(&*target_raw)),
                ],
                "blockNumber": format!("0x{:x}", block),
            }]
        });

        let body_bytes = serde_json::to_vec(&bundle)?;
        let body_hash  = ethers::utils::keccak256(&body_bytes);
        let sig        = self.signer.sign_message_hash(body_hash).await?;
        let fb_header  = format!("{:#x}:0x{}", self.signer.wallet_address(), hex::encode(sig.to_vec()));

        let futs: Vec<_> = self.builder_endpoints.iter().map(|ep| {
            let c   = self.client.clone();
            let b   = bundle.clone();
            let ep  = ep.clone();
            let hdr = fb_header.clone();
            async move {
                match c.post(&ep).header("X-Flashbots-Signature", &hdr).json(&b).send().await {
                    Ok(resp) => {
                        // parse builder response and log rejections
                        match resp.json::<serde_json::Value>().await {
                            Ok(body) => {
                                if let Some(err) = body.get("error") {
                                    tracing::warn!("Builder {} rejected bundle: {}", ep, err);
                                    crate::monitoring::metrics::record_sim_result("bundle", "rejected");
                                } else {
                                    tracing::debug!("Builder {} accepted bundle", ep);
                                    crate::monitoring::metrics::record_submission();
                                }
                            }
                            Err(e) => tracing::warn!("Builder {} non-JSON response: {}", ep, e),
                        }
                    }
                    Err(e) => tracing::warn!("Bundle submit to {}: {}", ep, e),
                }
            }
        }).collect();
        futures::future::join_all(futs).await;
        Ok(())
    }

    pub async fn resync_nonce(&self) -> Result<()> {
        let wallet_addr = self.signer.wallet_address();
        let chain_nonce = self.provider
            .get_transaction_count(wallet_addr, Some(BlockId::Number(BlockNumber::Pending)))
            .await?.as_u64();
        let local = self.nonce.load(Ordering::SeqCst);
        if chain_nonce > local {
            self.nonce.store(chain_nonce, Ordering::SeqCst);
            tracing::info!("Nonce resynced: {} → {}", local, chain_nonce);
        }
        Ok(())
    }

    async fn submit_with_tip(&self, calldata: Bytes, gas_limit: u64, tip_mult: f64) -> Result<H256> {
        let nonce = self.nonce.fetch_add(1, Ordering::SeqCst);
        let (raw, hash) = self.signer.sign_tx_with_nonce(calldata, gas_limit, tip_mult, nonce).await?;
        self.broadcast_raw(&raw).await;
        crate::monitoring::metrics::record_submission();
        Ok(hash)
    }

    async fn broadcast_raw(&self, raw: &Bytes) {
        let raw_hex = format!("0x{}", hex::encode(raw.as_ref()));
        let payload = serde_json::json!({
            "jsonrpc": "2.0", "id": 1,
            "method": "eth_sendRawTransaction",
            "params": [&raw_hex]
        });
        let all: Vec<String> = std::iter::once(self.sequencer_rpc.clone())
            .chain(self.builder_endpoints.iter().cloned())
            .collect();
        let futs: Vec<_> = all.iter().map(|ep| {
            let c = self.client.clone();
            let p = payload.clone();
            let ep = ep.clone();
            async move {
                match c.post(&ep).json(&p).send().await {
                    Ok(resp) => {
                        // parse response for error logging
                        if let Ok(body) = resp.json::<serde_json::Value>().await {
                            if let Some(err) = body.get("error") {
                                tracing::warn!("broadcast to {}: rpc error {}", ep, err);
                            }
                        }
                    }
                    Err(e) => tracing::debug!("broadcast to {}: {}", ep, e),
                }
            }
        }).collect();
        futures::future::join_all(futs).await;
    }
}
