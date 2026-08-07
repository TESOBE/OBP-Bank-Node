//! Interface C message router — dispatches an inbound RabbitMQ message by its
//! `MessageId` to a handler and produces the OBP inbound-envelope reply.
//!
//! This is pure of the AMQP transport: it takes `(message_id, correlation_id,
//! body)` and returns a [`ReplyEnvelope`], so the whole dispatch + handler
//! surface is unit-testable without a broker. The `lapin` consumer
//! ([`super::consumer`]) is a thin shell that feeds messages in and publishes
//! the replies out.

use std::sync::Arc;

use obp_blockchain::settlement::{
    PartyRef, SettlementBackend, SettlementInstruction as BcSettlement,
};
use obp_blockchain::{BlockchainError, PromiseRecord};
use tracing::{info, warn};

use super::types::*;
use crate::cbs::CbsClient;
use crate::evidence::{EvidenceStore, NewEvidence};
use crate::settlement_store::{self, Claim, NewClaim, SettlementRow, SettlementStore};

/// The value leg as the router sees it: the backend that moves funds, the
/// durable idempotency/finality store, and the depth at which a settlement
/// counts as final. Constructed together in `main` — a rail without the store
/// would re-pay on redelivery, so the pairing is not optional.
pub struct SettlementService {
    pub backend: Arc<dyn SettlementBackend>,
    pub store: SettlementStore,
    pub finality_depth: u32,
}

pub struct Router {
    pub bank_id: String,
    evidence: EvidenceStore,
    cbs: CbsClient,
    /// `None` when this node has no settlement rail configured (e.g. the mock
    /// blockchain backend).
    settlement: Option<SettlementService>,
}

impl Router {
    pub fn new(
        bank_id: impl Into<String>,
        evidence: EvidenceStore,
        cbs: CbsClient,
        settlement: Option<SettlementService>,
    ) -> Self {
        Self {
            bank_id: bank_id.into(),
            evidence,
            cbs,
            settlement,
        }
    }

    /// Dispatch one message. Always returns a reply envelope (never panics on a
    /// bad body) so the consumer can publish something back to `replyTo`.
    pub async fn handle(
        &self,
        message_id: &str,
        correlation_id: &str,
        body: &[u8],
    ) -> ReplyEnvelope {
        match message_id {
            message_id::CREDIT_NOTIFICATION => self.credit_notification(correlation_id, body).await,
            message_id::SETTLEMENT_INSTRUCTION => {
                self.settlement_instruction(correlation_id, body).await
            }
            message_id::NETTING_SNAPSHOT => self.netting_snapshot(correlation_id, body),
            message_id::STATUS_UPDATE => self.status_update(correlation_id, body),
            other => {
                warn!(message_id = %other, "Interface C: unrecognised MessageId");
                ReplyEnvelope::error(
                    correlation_id,
                    error_code::NOT_IMPLEMENTED,
                    format!("unrecognised MessageId: {other}"),
                )
            }
        }
    }

    /// `obp_credit_notification`: verify the commitment (if the evidence triplet
    /// is present), durably record the evidence, then deliver the credit to the
    /// bank's CBS. A commitment mismatch short-circuits — the customer is NOT
    /// credited on a notification we can't cryptographically tie to Bank A's
    /// on-chain promise.
    async fn credit_notification(&self, correlation_id: &str, body: &[u8]) -> ReplyEnvelope {
        let raw = String::from_utf8_lossy(body).into_owned();
        let cn: CreditNotification = match serde_json::from_slice(body) {
            Ok(v) => v,
            Err(e) => {
                warn!(error = %e, "credit_notification: malformed body");
                return ReplyEnvelope::error(
                    correlation_id,
                    error_code::BAD_MESSAGE,
                    format!("malformed credit_notification: {e}"),
                );
            }
        };

        // Verify only if all three evidence fields are present.
        let evidence_present = cn.promise_commitment.is_some()
            && cn.promise_salt.is_some()
            && cn.promise_preimage.is_some();
        let verified = match (
            &cn.promise_commitment,
            &cn.promise_salt,
            &cn.promise_preimage,
        ) {
            (Some(commitment), Some(salt), Some(preimage)) => {
                PromiseRecord::verify_v1(preimage.as_bytes(), salt.as_bytes(), commitment)
            }
            _ => false,
        };

        // Persist whatever arrived — durable possession is the point, even if
        // verification fails (the failed record is itself evidence of tampering).
        let originator_name = cn.originator.as_ref().map(|o| o.name.as_str());
        let store_result = self
            .evidence
            .upsert(NewEvidence {
                transaction_request_id: &cn.transaction_request_id,
                promise_commitment: cn.promise_commitment.as_deref().unwrap_or(""),
                promise_salt: cn.promise_salt.as_deref().unwrap_or(""),
                promise_preimage: cn.promise_preimage.as_deref().unwrap_or(""),
                promise_id: cn.promise_id.as_deref(),
                promise_blockchain: cn.promise_blockchain.as_deref(),
                verified,
                currency: Some(cn.value.currency.as_str()),
                amount: Some(cn.value.amount.as_str()),
                originator_name,
                raw_message: &raw,
            })
            .await;
        if let Err(e) = store_result {
            warn!(error = %e, "credit_notification: failed to persist evidence");
            return ReplyEnvelope::error(
                correlation_id,
                error_code::PLATFORM,
                "failed to persist evidence",
            );
        }

        if evidence_present && !verified {
            warn!(
                transaction_request_id = %cn.transaction_request_id,
                "credit_notification: commitment does NOT match salt+preimage — refusing to credit"
            );
            return ReplyEnvelope::error(
                correlation_id,
                error_code::COMMITMENT_MISMATCH,
                "promise commitment does not match the revealed salt and preimage",
            );
        }

        info!(
            bank_id = %self.bank_id,
            transaction_request_id = %cn.transaction_request_id,
            verified,
            evidence_present,
            "credit_notification: evidence stored; delivering to CBS"
        );

        // Deliver the credit to the bank's CBS (Interface A2), and record the
        // outcome on the evidence row — the delivery result is part of the
        // bank's audit trail (and read back over the south-side evidence API).
        // A failed *recording* is logged but does not fail the reply: the
        // delivery itself already happened (or failed) independently.
        match self.cbs.deliver_credit(&raw).await {
            Ok(ack) => {
                if let Err(e) = self
                    .evidence
                    .record_cbs_result(
                        &cn.transaction_request_id,
                        "DELIVERED",
                        ack.cbs_reference.as_deref(),
                    )
                    .await
                {
                    warn!(error = %e, "credit_notification: failed to record CBS result");
                }
                ReplyEnvelope::ok(
                    correlation_id,
                    serde_json::json!({
                        "transaction_request_id": cn.transaction_request_id,
                        "verified": verified,
                        "cbs_reference": ack.cbs_reference,
                    }),
                )
            }
            Err(e) => {
                warn!(error = %e, "credit_notification: CBS delivery failed");
                if let Err(se) = self
                    .evidence
                    .record_cbs_result(&cn.transaction_request_id, "FAILED", None)
                    .await
                {
                    warn!(error = %se, "credit_notification: failed to record CBS result");
                }
                ReplyEnvelope::error(
                    correlation_id,
                    error_code::CBS_DELIVERY_FAILED,
                    e.to_string(),
                )
            }
        }
    }

    /// `obp_settlement_instruction`: this (debtor) node settles the net on its
    /// rail. Idempotent by design — OBP-API redelivers from its transactional
    /// outbox, and redelivery doubles as status polling:
    ///
    /// 1. Claim a durable row by `idempotency_key` *before* paying. A
    ///    redelivered/concurrent instruction gets the recorded state back
    ///    (`SETTLING`/`SUBMITTED`/`FINAL`/`ERROR`), never a second payment.
    /// 2. On a fresh claim, call [`SettlementBackend::settle`] (real value
    ///    transfer) and record `SUBMITTED` + the tx.
    /// 3. The reply's `data.status` is explicit: `SUBMITTED` means broadcast,
    ///    **not** final — the finality watcher promotes the row to `FINAL` at
    ///    the configured depth, and OBP-API sees that on its next redelivery.
    async fn settlement_instruction(&self, correlation_id: &str, body: &[u8]) -> ReplyEnvelope {
        let si: SettlementInstruction = match serde_json::from_slice(body) {
            Ok(v) => v,
            Err(e) => {
                return ReplyEnvelope::error(
                    correlation_id,
                    error_code::BAD_MESSAGE,
                    format!("malformed settlement_instruction: {e}"),
                )
            }
        };

        let svc = match &self.settlement {
            Some(s) => s,
            None => {
                warn!(bank_id = %self.bank_id, "settlement_instruction received but no settlement backend is configured");
                return ReplyEnvelope::error(
                    correlation_id,
                    error_code::SETTLEMENT_NOT_CONFIGURED,
                    "no settlement backend configured on this node",
                );
            }
        };

        // Required fields. The idempotency key is mandatory for real money —
        // without it a redelivered instruction is indistinguishable from a new
        // obligation.
        let amount = match si.amount.as_deref() {
            Some(a) => a,
            None => {
                return ReplyEnvelope::error(
                    correlation_id,
                    error_code::BAD_MESSAGE,
                    "settlement_instruction: amount is required",
                )
            }
        };
        let net_amount_minor = match parse_minor_units(amount, 2) {
            Ok(v) => v,
            Err(e) => {
                return ReplyEnvelope::error(
                    correlation_id,
                    error_code::BAD_MESSAGE,
                    format!("settlement_instruction: {e}"),
                )
            }
        };
        let creditor_address = match si.creditor_address.clone() {
            Some(a) => a,
            None => {
                return ReplyEnvelope::error(
                    correlation_id,
                    error_code::BAD_MESSAGE,
                    "settlement_instruction: creditor_address is required",
                )
            }
        };
        let idempotency_key = match si
            .idempotency_key
            .clone()
            .or_else(|| si.settlement_id.clone())
            .filter(|k| !k.trim().is_empty())
        {
            Some(k) => k,
            None => {
                return ReplyEnvelope::error(
                    correlation_id,
                    error_code::BAD_MESSAGE,
                    "settlement_instruction: idempotency_key (or settlement_id) is required",
                )
            }
        };

        // Claim before pay. Whoever inserts the row owns the attempt.
        let claim = svc
            .store
            .claim(NewClaim {
                idempotency_key: &idempotency_key,
                settlement_id: si.settlement_id.as_deref(),
                snapshot_id: si.snapshot_id.as_deref(),
                currency: si.currency.as_deref().unwrap_or_default(),
                net_amount_minor,
                creditor_address: &creditor_address,
            })
            .await;
        match claim {
            Ok(Claim::Claimed) => { /* fresh attempt below */ }
            Ok(Claim::Existing(row)) => {
                let retry = row.status == settlement_store::status::ERROR && row.retryable;
                if retry {
                    // Provably-never-on-chain failure: reopen and try again.
                    if let Err(e) = svc.store.reclaim_for_retry(&idempotency_key).await {
                        warn!(error = %e, "settlement store: reclaim failed");
                        return ReplyEnvelope::error(
                            correlation_id,
                            error_code::PLATFORM,
                            "settlement store unavailable",
                        );
                    }
                } else {
                    return reply_for_existing(correlation_id, &row, svc.finality_depth);
                }
            }
            Err(e) => {
                warn!(error = %e, "settlement store: claim failed");
                return ReplyEnvelope::error(
                    correlation_id,
                    error_code::PLATFORM,
                    "settlement store unavailable",
                );
            }
        }

        // This node is the debtor: pay out of the backend's own account.
        let instruction = BcSettlement {
            snapshot_id: si
                .snapshot_id
                .clone()
                .or_else(|| si.settlement_id.clone())
                .unwrap_or_default(),
            debtor: PartyRef {
                bank_id: self.bank_id.clone(),
                account: svc.backend.settles_from().to_string(),
            },
            creditor: PartyRef {
                bank_id: si.creditor_bank_id.clone().unwrap_or_default(),
                account: creditor_address,
            },
            currency: si.currency.clone().unwrap_or_default(),
            net_amount_minor,
            idempotency_key: idempotency_key.clone(),
        };

        info!(
            bank_id = %self.bank_id,
            settlement_id = ?si.settlement_id,
            system = svc.backend.system(),
            net_amount_minor,
            "settlement_instruction: settling on chain"
        );

        match svc.backend.settle(&instruction).await {
            Ok(outcome) => {
                if let Err(e) = svc
                    .store
                    .mark_submitted(
                        &idempotency_key,
                        &outcome.tx.tx_id,
                        &outcome.tx.chain,
                        &outcome.asset,
                        &outcome.asset_amount,
                    )
                    .await
                {
                    // The transfer is on chain but the store write failed: the
                    // row stays SETTLING, which redelivery surfaces as
                    // in-flight rather than re-paying. Loud log for ops.
                    tracing::error!(error = %e, tx_id = %outcome.tx.tx_id, "settlement store: mark_submitted failed AFTER broadcast");
                }
                ReplyEnvelope::ok(
                    correlation_id,
                    serde_json::json!({
                        "settlement_id": si.settlement_id,
                        "status": settlement_store::status::SUBMITTED,
                        "tx_id": outcome.tx.tx_id,
                        "blockchain": outcome.tx.chain,
                        "asset": outcome.asset,
                        "asset_amount": outcome.asset_amount,
                        "depth": 0,
                        "finality_depth": svc.finality_depth,
                    }),
                )
            }
            Err(e) => {
                // `Rejected` provably never reached the chain (guard/validation
                // failures, or the node refusing the tx) → retryable on
                // redelivery. Transport/internal errors are ambiguous — the tx
                // may be on chain — so they stick until reconciled.
                let retryable = matches!(e, BlockchainError::Rejected(_));
                if let Err(se) = svc
                    .store
                    .mark_error(&idempotency_key, &e.to_string(), retryable)
                    .await
                {
                    warn!(error = %se, "settlement store: mark_error failed");
                }
                warn!(error = %e, settlement_id = ?si.settlement_id, retryable, "settlement failed");
                ReplyEnvelope::error(correlation_id, error_code::SETTLEMENT_FAILED, e.to_string())
            }
        }
    }

    /// `obp_netting_snapshot`: record for reconciliation. Stored as a log line
    /// for now; a snapshot store is future work.
    fn netting_snapshot(&self, correlation_id: &str, body: &[u8]) -> ReplyEnvelope {
        match serde_json::from_slice::<serde_json::Value>(body) {
            Ok(v) => {
                let snapshot_id = v.get("snapshot_id").and_then(|s| s.as_str()).unwrap_or("?");
                info!(bank_id = %self.bank_id, %snapshot_id, "netting_snapshot received");
                ReplyEnvelope::ok(correlation_id, serde_json::json!({ "recorded": true }))
            }
            Err(e) => ReplyEnvelope::error(
                correlation_id,
                error_code::BAD_MESSAGE,
                format!("malformed netting_snapshot: {e}"),
            ),
        }
    }

    /// `obp_status_update`: a TR's status changed. Logged for now; reconciling it
    /// onto the local outbox lifecycle is future work.
    fn status_update(&self, correlation_id: &str, body: &[u8]) -> ReplyEnvelope {
        match serde_json::from_slice::<StatusUpdate>(body) {
            Ok(su) => {
                info!(
                    bank_id = %self.bank_id,
                    transaction_request_id = %su.transaction_request_id,
                    status = %su.status,
                    "status_update received"
                );
                ReplyEnvelope::ok(correlation_id, serde_json::json!({ "recorded": true }))
            }
            Err(e) => ReplyEnvelope::error(
                correlation_id,
                error_code::BAD_MESSAGE,
                format!("malformed status_update: {e}"),
            ),
        }
    }
}

/// The reply for a redelivered settlement instruction: the recorded state,
/// never a new payment. `SETTLING`/`SUBMITTED`/`FINAL` are ok-replies with an
/// explicit `status` (this is how OBP-API polls finality); a sticky `ERROR`
/// repeats the recorded failure.
fn reply_for_existing(
    correlation_id: &str,
    row: &SettlementRow,
    finality_depth: u32,
) -> ReplyEnvelope {
    match row.status.as_str() {
        settlement_store::status::ERROR => ReplyEnvelope::error(
            correlation_id,
            error_code::SETTLEMENT_FAILED,
            row.error_reason
                .clone()
                .unwrap_or_else(|| "settlement failed".into()),
        ),
        status => ReplyEnvelope::ok(
            correlation_id,
            serde_json::json!({
                "settlement_id": row.settlement_id,
                "status": status,
                "tx_id": row.tx_id,
                "blockchain": row.blockchain,
                "asset": row.asset,
                "asset_amount": row.asset_amount,
                "depth": row.last_depth,
                "finality_depth": finality_depth,
            }),
        ),
    }
}

/// Parse a decimal major-unit amount (e.g. `"25000.00"`) into integer minor
/// units at `decimals` places. Assumes `decimals = 2` (KES-like currencies) —
/// a documented limitation; per-currency exponent is on the list. Truncates
/// beyond `decimals`.
fn parse_minor_units(amount: &str, decimals: u32) -> Result<u128, String> {
    let amount = amount.trim();
    let (int_part, frac_part) = amount.split_once('.').unwrap_or((amount, ""));
    if int_part.is_empty() && frac_part.is_empty() {
        return Err("empty amount".into());
    }
    let int: u128 = if int_part.is_empty() {
        0
    } else {
        int_part
            .parse()
            .map_err(|_| format!("invalid amount: {amount:?}"))?
    };
    let width = decimals as usize;
    let mut frac = frac_part.to_string();
    frac.truncate(width);
    while frac.len() < width {
        frac.push('0');
    }
    let frac_val: u128 = if frac.is_empty() {
        0
    } else {
        frac.parse()
            .map_err(|_| format!("invalid amount fraction: {amount:?}"))?
    };
    int.checked_mul(10u128.pow(decimals))
        .and_then(|v| v.checked_add(frac_val))
        .ok_or_else(|| "amount overflow".into())
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::{routing::post, Json, Router as AxumRouter};
    use std::net::SocketAddr;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::sync::Arc;

    async fn cbs_stub_counting(counter: Arc<AtomicUsize>) -> String {
        let router = AxumRouter::new().route(
            "/credit",
            post(move || {
                let counter = counter.clone();
                async move {
                    counter.fetch_add(1, Ordering::SeqCst);
                    Json(serde_json::json!({ "status": "ACCEPTED", "cbs_reference": "CBS-1" }))
                }
            }),
        );
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr: SocketAddr = listener.local_addr().unwrap();
        tokio::spawn(async move { axum::serve(listener, router).await.unwrap() });
        format!("http://{addr}/credit")
    }

    /// Finality depth used across router tests.
    const TEST_FINALITY_DEPTH: u32 = 5;

    async fn router_with_cbs(cbs_url: &str) -> Router {
        router_with(cbs_url, None).await.0
    }

    /// Returns the router plus a handle on the settlement store so tests can
    /// inspect/advance settlement rows.
    async fn router_with(
        cbs_url: &str,
        settlement: Option<Arc<dyn SettlementBackend>>,
    ) -> (Router, Option<SettlementStore>) {
        let evidence = EvidenceStore::connect_in_memory().await.unwrap();
        let cbs = CbsClient::new(cbs_url, None, 5).unwrap();
        let (service, store) = match settlement {
            Some(backend) => {
                let store = SettlementStore::connect_in_memory().await.unwrap();
                (
                    Some(SettlementService {
                        backend,
                        store: store.clone(),
                        finality_depth: TEST_FINALITY_DEPTH,
                    }),
                    Some(store),
                )
            }
            None => (None, None),
        };
        (Router::new("ke.01.kcs", evidence, cbs, service), store)
    }

    /// A settlement backend test double: records calls and returns a canned tx.
    struct MockSettlement {
        from: String,
        calls: Arc<AtomicUsize>,
        last_creditor: std::sync::Mutex<Option<String>>,
        last_minor: std::sync::Mutex<Option<u128>>,
    }

    #[async_trait::async_trait]
    impl SettlementBackend for MockSettlement {
        fn system(&self) -> &str {
            "mock-settlement"
        }
        fn settles_from(&self) -> &str {
            &self.from
        }
        async fn settle(
            &self,
            instruction: &BcSettlement,
        ) -> obp_blockchain::Result<obp_blockchain::settlement::SettlementOutcome> {
            self.calls.fetch_add(1, Ordering::SeqCst);
            *self.last_creditor.lock().unwrap() = Some(instruction.creditor.account.clone());
            *self.last_minor.lock().unwrap() = Some(instruction.net_amount_minor);
            Ok(obp_blockchain::settlement::SettlementOutcome {
                tx: obp_blockchain::TxReference {
                    chain: "cardano".into(),
                    tx_id: "settle-tx-1".into(),
                    submitted_at: chrono::Utc::now(),
                },
                asset: "ADA".into(),
                asset_amount: "141163".into(),
                fx: None,
            })
        }
        async fn confirm(
            &self,
            _tx: &obp_blockchain::TxReference,
        ) -> obp_blockchain::Result<obp_blockchain::ConfirmationStatus> {
            Ok(obp_blockchain::ConfirmationStatus::Confirmed { depth: 1 })
        }
    }

    /// Build a credit_notification whose commitment genuinely matches its
    /// salt+preimage, using the same scheme the writer uses.
    fn credit_with_valid_commitment() -> (String, String) {
        let preimage = r#"{"transaction_request_id":"tr-1","amount":"1500.00"}"#;
        let salt = "00112233445566778899aabbccddeeff";
        let commitment = PromiseRecord::compute_commitment(preimage.as_bytes(), salt.as_bytes());
        let body = serde_json::json!({
            "transaction_request_id": "tr-1",
            "value": { "currency": "KES", "amount": "1500.00" },
            "originator": { "name": "Acme Coffee Ltd" },
            "promise_id": "cardano-tx-abc",
            "promise_blockchain": "cardano",
            "promise_commitment": commitment,
            "promise_salt": salt,
            "promise_preimage": preimage,
        });
        (body.to_string(), "tr-1".into())
    }

    #[tokio::test]
    async fn unknown_message_id_returns_not_implemented() {
        let r = router_with_cbs("http://127.0.0.1:1/credit").await;
        let reply = r.handle("obp_no_such_thing", "corr-1", b"{}").await;
        assert!(!reply.is_ok());
        assert_eq!(reply.status.error_code, error_code::NOT_IMPLEMENTED);
        assert_eq!(reply.inbound_adapter_call_context.correlation_id, "corr-1");
    }

    #[tokio::test]
    async fn valid_credit_notification_verifies_stores_and_delivers() {
        let counter = Arc::new(AtomicUsize::new(0));
        let cbs_url = cbs_stub_counting(counter.clone()).await;
        let r = router_with_cbs(&cbs_url).await;
        let (body, id) = credit_with_valid_commitment();

        let reply = r
            .handle(message_id::CREDIT_NOTIFICATION, "corr-2", body.as_bytes())
            .await;
        assert!(reply.is_ok(), "expected ok, got {:?}", reply.status);
        assert_eq!(reply.data["verified"], true);
        assert_eq!(reply.data["cbs_reference"], "CBS-1");
        assert_eq!(counter.load(Ordering::SeqCst), 1, "CBS delivered once");

        // Evidence persisted, marked verified, CBS outcome recorded.
        let rec = r.evidence.get(&id).await.unwrap().unwrap();
        assert!(rec.verified);
        assert_eq!(rec.promise_id.as_deref(), Some("cardano-tx-abc"));
        assert_eq!(rec.cbs_status.as_deref(), Some("DELIVERED"));
        assert_eq!(rec.cbs_reference.as_deref(), Some("CBS-1"));
    }

    #[tokio::test]
    async fn tampered_commitment_is_rejected_and_not_credited() {
        // CBS at a refused port: if it were called the reply would be a delivery
        // error, not a mismatch — so reaching MISMATCH proves we didn't credit.
        let r = router_with_cbs("http://127.0.0.1:1/credit").await;
        let preimage = r#"{"amount":"1500.00"}"#;
        let body = serde_json::json!({
            "transaction_request_id": "tr-bad",
            "value": { "currency": "KES", "amount": "1500.00" },
            "promise_commitment": "deadbeef",   // does not match
            "promise_salt": "abcd",
            "promise_preimage": preimage,
        })
        .to_string();

        let reply = r
            .handle(message_id::CREDIT_NOTIFICATION, "corr-3", body.as_bytes())
            .await;
        assert_eq!(reply.status.error_code, error_code::COMMITMENT_MISMATCH);
        // Still recorded (as evidence of the bad notification), marked unverified.
        let rec = r.evidence.get("tr-bad").await.unwrap().unwrap();
        assert!(!rec.verified);
    }

    #[tokio::test]
    async fn credit_without_evidence_fields_still_delivers() {
        // Backward-compatible: a notification lacking the evidence triplet is
        // stored unverified and still delivered.
        let counter = Arc::new(AtomicUsize::new(0));
        let cbs_url = cbs_stub_counting(counter.clone()).await;
        let r = router_with_cbs(&cbs_url).await;
        let body = serde_json::json!({
            "transaction_request_id": "tr-plain",
            "value": { "currency": "KES", "amount": "1500.00" },
        })
        .to_string();

        let reply = r
            .handle(message_id::CREDIT_NOTIFICATION, "corr-4", body.as_bytes())
            .await;
        assert!(reply.is_ok());
        assert_eq!(reply.data["verified"], false);
        assert_eq!(counter.load(Ordering::SeqCst), 1);
    }

    #[tokio::test]
    async fn malformed_body_returns_bad_message() {
        let r = router_with_cbs("http://127.0.0.1:1/credit").await;
        let reply = r
            .handle(message_id::CREDIT_NOTIFICATION, "corr-5", b"{not json")
            .await;
        assert_eq!(reply.status.error_code, error_code::BAD_MESSAGE);
    }

    #[tokio::test]
    async fn cbs_down_returns_delivery_failed() {
        let r = router_with_cbs("http://127.0.0.1:1/credit").await;
        let (body, id) = credit_with_valid_commitment();
        let reply = r
            .handle(message_id::CREDIT_NOTIFICATION, "corr-6", body.as_bytes())
            .await;
        assert_eq!(reply.status.error_code, error_code::CBS_DELIVERY_FAILED);
        // The failed delivery attempt is recorded on the evidence row.
        let rec = r.evidence.get(&id).await.unwrap().unwrap();
        assert_eq!(rec.cbs_status.as_deref(), Some("FAILED"));
        assert!(rec.cbs_reference.is_none());
    }

    #[tokio::test]
    async fn netting_snapshot_and_status_update_are_acknowledged() {
        let r = router_with_cbs("http://127.0.0.1:1/credit").await;
        let n = r
            .handle(
                message_id::NETTING_SNAPSHOT,
                "c",
                br#"{"snapshot_id":"snap1"}"#,
            )
            .await;
        assert!(n.is_ok());
        let u = r
            .handle(
                message_id::STATUS_UPDATE,
                "c",
                br#"{"transaction_request_id":"tr-1","status":"COMPLETED"}"#,
            )
            .await;
        assert!(u.is_ok());
    }

    fn settlement_body(idem: &str) -> String {
        serde_json::json!({
            "settlement_id": "s1",
            "currency": "KES",
            "amount": "25000.00",
            "creditor_address": "addr_test1creditor",
            "idempotency_key": idem,
        })
        .to_string()
    }

    #[tokio::test]
    async fn settlement_instruction_triggers_the_backend() {
        let calls = Arc::new(AtomicUsize::new(0));
        let settlement = Arc::new(MockSettlement {
            from: "addr_test1me".into(),
            calls: calls.clone(),
            last_creditor: std::sync::Mutex::new(None),
            last_minor: std::sync::Mutex::new(None),
        });
        let (r, store) = router_with("http://127.0.0.1:1/credit", Some(settlement.clone())).await;

        let body = settlement_body("idem-1");
        let reply = r
            .handle(message_id::SETTLEMENT_INSTRUCTION, "c", body.as_bytes())
            .await;

        assert!(reply.is_ok(), "expected ok, got {:?}", reply.status);
        assert_eq!(reply.data["tx_id"], "settle-tx-1");
        assert_eq!(reply.data["asset"], "ADA");
        // Broadcast is not settlement: the reply is explicit about it.
        assert_eq!(reply.data["status"], settlement_store::status::SUBMITTED);
        assert_eq!(reply.data["finality_depth"], TEST_FINALITY_DEPTH);
        assert_eq!(calls.load(Ordering::SeqCst), 1, "settle called once");
        // The debtor account was filled from the backend, and minor units parsed.
        assert_eq!(
            settlement.last_creditor.lock().unwrap().as_deref(),
            Some("addr_test1creditor")
        );
        assert_eq!(*settlement.last_minor.lock().unwrap(), Some(2_500_000)); // 25000.00 → cents
                                                                             // Durably recorded for the finality watcher.
        let row = store.unwrap().get("idem-1").await.unwrap().unwrap();
        assert_eq!(row.status, settlement_store::status::SUBMITTED);
        assert_eq!(row.tx_id.as_deref(), Some("settle-tx-1"));
    }

    #[tokio::test]
    async fn redelivered_instruction_reports_state_and_never_pays_twice() {
        let calls = Arc::new(AtomicUsize::new(0));
        let settlement = Arc::new(MockSettlement {
            from: "addr_test1me".into(),
            calls: calls.clone(),
            last_creditor: std::sync::Mutex::new(None),
            last_minor: std::sync::Mutex::new(None),
        });
        let (r, store) = router_with("http://127.0.0.1:1/credit", Some(settlement)).await;
        let body = settlement_body("idem-1");

        let first = r
            .handle(message_id::SETTLEMENT_INSTRUCTION, "c1", body.as_bytes())
            .await;
        assert_eq!(first.data["status"], settlement_store::status::SUBMITTED);

        // Redelivery (OBP-API outbox retry / status poll): recorded state back,
        // no second payment.
        let second = r
            .handle(message_id::SETTLEMENT_INSTRUCTION, "c2", body.as_bytes())
            .await;
        assert!(second.is_ok());
        assert_eq!(second.data["status"], settlement_store::status::SUBMITTED);
        assert_eq!(second.data["tx_id"], "settle-tx-1");
        assert_eq!(
            calls.load(Ordering::SeqCst),
            1,
            "settle must run exactly once"
        );

        // Once the watcher finalizes, redelivery reports FINAL with depth.
        let store = store.unwrap();
        store.record_depth("idem-1", 3).await.unwrap();
        store
            .mark_final("idem-1", TEST_FINALITY_DEPTH)
            .await
            .unwrap();
        let third = r
            .handle(message_id::SETTLEMENT_INSTRUCTION, "c3", body.as_bytes())
            .await;
        assert_eq!(third.data["status"], settlement_store::status::FINAL);
        assert_eq!(third.data["depth"], TEST_FINALITY_DEPTH);
        assert_eq!(calls.load(Ordering::SeqCst), 1);
    }

    #[tokio::test]
    async fn settlement_without_any_idempotency_key_is_bad_message() {
        let calls = Arc::new(AtomicUsize::new(0));
        let settlement = Arc::new(MockSettlement {
            from: "addr_test1me".into(),
            calls: calls.clone(),
            last_creditor: std::sync::Mutex::new(None),
            last_minor: std::sync::Mutex::new(None),
        });
        let (r, _) = router_with("http://127.0.0.1:1/credit", Some(settlement)).await;
        // No idempotency_key AND no settlement_id → refuse; a redelivered
        // instruction would be indistinguishable from a new obligation.
        let body = br#"{"currency":"KES","amount":"10.00","creditor_address":"x"}"#;
        let reply = r
            .handle(message_id::SETTLEMENT_INSTRUCTION, "c", body)
            .await;
        assert_eq!(reply.status.error_code, error_code::BAD_MESSAGE);
        assert_eq!(calls.load(Ordering::SeqCst), 0);
    }

    #[tokio::test]
    async fn settlement_instruction_without_backend_is_not_configured() {
        let r = router_with_cbs("http://127.0.0.1:1/credit").await; // settlement = None
        let body =
            br#"{"settlement_id":"s1","currency":"KES","amount":"10.00","creditor_address":"x"}"#;
        let reply = r
            .handle(message_id::SETTLEMENT_INSTRUCTION, "c", body)
            .await;
        assert_eq!(
            reply.status.error_code,
            error_code::SETTLEMENT_NOT_CONFIGURED
        );
    }

    /// A backend that fails with the scripted error `fail_times` times, then
    /// succeeds.
    struct FlakyImpl {
        error: fn() -> obp_blockchain::BlockchainError,
        fail_times: usize,
        calls: Arc<AtomicUsize>,
    }

    #[async_trait::async_trait]
    impl SettlementBackend for FlakyImpl {
        fn system(&self) -> &str {
            "flaky"
        }
        fn settles_from(&self) -> &str {
            "addr_test1me"
        }
        async fn settle(
            &self,
            _i: &BcSettlement,
        ) -> obp_blockchain::Result<obp_blockchain::settlement::SettlementOutcome> {
            let n = self.calls.fetch_add(1, Ordering::SeqCst);
            if n < self.fail_times {
                return Err((self.error)());
            }
            Ok(obp_blockchain::settlement::SettlementOutcome {
                tx: obp_blockchain::TxReference {
                    chain: "cardano".into(),
                    tx_id: "settle-tx-retry".into(),
                    submitted_at: chrono::Utc::now(),
                },
                asset: "ADA".into(),
                asset_amount: "1".into(),
                fx: None,
            })
        }
        async fn confirm(
            &self,
            _tx: &obp_blockchain::TxReference,
        ) -> obp_blockchain::Result<obp_blockchain::ConfirmationStatus> {
            Ok(obp_blockchain::ConfirmationStatus::Pending)
        }
    }

    #[tokio::test]
    async fn rejected_failure_is_retryable_on_redelivery() {
        let calls = Arc::new(AtomicUsize::new(0));
        let backend = Arc::new(FlakyImpl {
            error: || obp_blockchain::BlockchainError::Rejected("fx unavailable".into()),
            fail_times: 1,
            calls: calls.clone(),
        });
        let (r, _) = router_with("http://127.0.0.1:1/credit", Some(backend)).await;
        let body = settlement_body("idem-r");

        let first = r
            .handle(message_id::SETTLEMENT_INSTRUCTION, "c1", body.as_bytes())
            .await;
        assert_eq!(first.status.error_code, error_code::SETTLEMENT_FAILED);

        // `Rejected` provably never reached the chain → redelivery retries.
        let second = r
            .handle(message_id::SETTLEMENT_INSTRUCTION, "c2", body.as_bytes())
            .await;
        assert!(
            second.is_ok(),
            "retryable failure must reopen: {:?}",
            second.status
        );
        assert_eq!(second.data["status"], settlement_store::status::SUBMITTED);
        assert_eq!(calls.load(Ordering::SeqCst), 2);
    }

    #[tokio::test]
    async fn transport_failure_is_sticky_no_automatic_repay() {
        let calls = Arc::new(AtomicUsize::new(0));
        let backend = Arc::new(FlakyImpl {
            error: || obp_blockchain::BlockchainError::Transport("socket died mid-submit".into()),
            fail_times: 1,
            calls: calls.clone(),
        });
        let (r, _) = router_with("http://127.0.0.1:1/credit", Some(backend)).await;
        let body = settlement_body("idem-t");

        let first = r
            .handle(message_id::SETTLEMENT_INSTRUCTION, "c1", body.as_bytes())
            .await;
        assert_eq!(first.status.error_code, error_code::SETTLEMENT_FAILED);

        // Transport is ambiguous — the transfer may be on chain. Redelivery
        // must repeat the recorded error, not pay again.
        let second = r
            .handle(message_id::SETTLEMENT_INSTRUCTION, "c2", body.as_bytes())
            .await;
        assert_eq!(second.status.error_code, error_code::SETTLEMENT_FAILED);
        assert_eq!(
            calls.load(Ordering::SeqCst),
            1,
            "no automatic second attempt"
        );
    }

    #[test]
    fn parse_minor_units_handles_decimals() {
        assert_eq!(parse_minor_units("25000.00", 2).unwrap(), 2_500_000);
        assert_eq!(parse_minor_units("25000", 2).unwrap(), 2_500_000);
        assert_eq!(parse_minor_units("0.05", 2).unwrap(), 5);
        assert_eq!(parse_minor_units("10.5", 2).unwrap(), 1050);
        assert_eq!(parse_minor_units("1.999", 2).unwrap(), 199); // truncates to cents
        assert!(parse_minor_units("abc", 2).is_err());
        assert!(parse_minor_units("-5.00", 2).is_err());
    }
}
