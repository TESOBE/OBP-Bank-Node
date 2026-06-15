//! Interface C message router — dispatches an inbound RabbitMQ message by its
//! `MessageId` to a handler and produces the OBP inbound-envelope reply.
//!
//! This is pure of the AMQP transport: it takes `(message_id, correlation_id,
//! body)` and returns a [`ReplyEnvelope`], so the whole dispatch + handler
//! surface is unit-testable without a broker. The `lapin` consumer
//! ([`super::consumer`]) is a thin shell that feeds messages in and publishes
//! the replies out.

use std::sync::Arc;

use obp_blockchain::settlement::{PartyRef, SettlementBackend, SettlementInstruction as BcSettlement};
use obp_blockchain::PromiseRecord;
use tracing::{info, warn};

use super::types::*;
use crate::cbs::CbsClient;
use crate::evidence::{EvidenceStore, NewEvidence};

pub struct Router {
    pub bank_id: String,
    evidence: EvidenceStore,
    cbs: CbsClient,
    /// The value-leg backend that actually moves funds when a settlement
    /// instruction arrives. `None` when this node has no settlement rail
    /// configured (e.g. the mock blockchain backend).
    settlement: Option<Arc<dyn SettlementBackend>>,
}

impl Router {
    pub fn new(
        bank_id: impl Into<String>,
        evidence: EvidenceStore,
        cbs: CbsClient,
        settlement: Option<Arc<dyn SettlementBackend>>,
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
    pub async fn handle(&self, message_id: &str, correlation_id: &str, body: &[u8]) -> ReplyEnvelope {
        match message_id {
            message_id::CREDIT_NOTIFICATION => self.credit_notification(correlation_id, body).await,
            message_id::SETTLEMENT_INSTRUCTION => self.settlement_instruction(correlation_id, body).await,
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
        let verified = match (&cn.promise_commitment, &cn.promise_salt, &cn.promise_preimage) {
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

        // Deliver the credit to the bank's CBS (Interface A2).
        match self.cbs.deliver_credit(&raw).await {
            Ok(ack) => ReplyEnvelope::ok(
                correlation_id,
                serde_json::json!({
                    "transaction_request_id": cn.transaction_request_id,
                    "verified": verified,
                    "cbs_reference": ack.cbs_reference,
                }),
            ),
            Err(e) => {
                warn!(error = %e, "credit_notification: CBS delivery failed");
                ReplyEnvelope::error(correlation_id, error_code::CBS_DELIVERY_FAILED, e.to_string())
            }
        }
    }

    /// `obp_settlement_instruction`: this (debtor) node settles the net on its
    /// rail. Maps the instruction to the settlement backend's shape — this node
    /// is the debtor, so `debtor.account` is the backend's own payout account —
    /// and calls [`SettlementBackend::settle`], which builds, signs, and submits
    /// the real value transfer (ADA on Cardano).
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

        let backend = match &self.settlement {
            Some(b) => b,
            None => {
                warn!(bank_id = %self.bank_id, "settlement_instruction received but no settlement backend is configured");
                return ReplyEnvelope::error(
                    correlation_id,
                    error_code::SETTLEMENT_NOT_CONFIGURED,
                    "no settlement backend configured on this node",
                );
            }
        };

        // Required value fields.
        let amount = match si.amount.as_deref() {
            Some(a) => a,
            None => return ReplyEnvelope::error(correlation_id, error_code::BAD_MESSAGE, "settlement_instruction: amount is required"),
        };
        let net_amount_minor = match parse_minor_units(amount, 2) {
            Ok(v) => v,
            Err(e) => return ReplyEnvelope::error(correlation_id, error_code::BAD_MESSAGE, format!("settlement_instruction: {e}")),
        };
        let creditor_address = match si.creditor_address.clone() {
            Some(a) => a,
            None => return ReplyEnvelope::error(correlation_id, error_code::BAD_MESSAGE, "settlement_instruction: creditor_address is required"),
        };

        // This node is the debtor: pay out of the backend's own account.
        let instruction = BcSettlement {
            snapshot_id: si.snapshot_id.clone().or_else(|| si.settlement_id.clone()).unwrap_or_default(),
            debtor: PartyRef {
                bank_id: self.bank_id.clone(),
                account: backend.settles_from().to_string(),
            },
            creditor: PartyRef {
                bank_id: si.creditor_bank_id.clone().unwrap_or_default(),
                account: creditor_address,
            },
            currency: si.currency.clone().unwrap_or_default(),
            net_amount_minor,
            idempotency_key: si
                .idempotency_key
                .clone()
                .or_else(|| si.settlement_id.clone())
                .unwrap_or_default(),
        };

        info!(
            bank_id = %self.bank_id,
            settlement_id = ?si.settlement_id,
            system = backend.system(),
            net_amount_minor,
            "settlement_instruction: settling on chain"
        );

        match backend.settle(&instruction).await {
            Ok(outcome) => ReplyEnvelope::ok(
                correlation_id,
                serde_json::json!({
                    "settlement_id": si.settlement_id,
                    "tx_id": outcome.tx.tx_id,
                    "blockchain": outcome.tx.chain,
                    "asset": outcome.asset,
                    "asset_amount": outcome.asset_amount,
                }),
            ),
            Err(e) => {
                warn!(error = %e, settlement_id = ?si.settlement_id, "settlement failed");
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

/// Parse a decimal major-unit amount (e.g. `"25000.00"`) into integer minor
/// units at `decimals` places. PoC assumes `decimals = 2` (KES-like currencies);
/// a per-currency exponent is future work. Truncates beyond `decimals`.
fn parse_minor_units(amount: &str, decimals: u32) -> Result<u128, String> {
    let amount = amount.trim();
    let (int_part, frac_part) = amount.split_once('.').unwrap_or((amount, ""));
    if int_part.is_empty() && frac_part.is_empty() {
        return Err("empty amount".into());
    }
    let int: u128 = if int_part.is_empty() {
        0
    } else {
        int_part.parse().map_err(|_| format!("invalid amount: {amount:?}"))?
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
        frac.parse().map_err(|_| format!("invalid amount fraction: {amount:?}"))?
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
    use std::sync::Arc;
    use std::sync::atomic::{AtomicUsize, Ordering};

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

    async fn router_with_cbs(cbs_url: &str) -> Router {
        router_with(cbs_url, None).await
    }

    async fn router_with(cbs_url: &str, settlement: Option<Arc<dyn SettlementBackend>>) -> Router {
        let evidence = EvidenceStore::connect_in_memory().await.unwrap();
        let cbs = CbsClient::new(cbs_url, None, 5).unwrap();
        Router::new("ke.01.kcs", evidence, cbs, settlement)
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

        let reply = r.handle(message_id::CREDIT_NOTIFICATION, "corr-2", body.as_bytes()).await;
        assert!(reply.is_ok(), "expected ok, got {:?}", reply.status);
        assert_eq!(reply.data["verified"], true);
        assert_eq!(reply.data["cbs_reference"], "CBS-1");
        assert_eq!(counter.load(Ordering::SeqCst), 1, "CBS delivered once");

        // Evidence persisted and marked verified.
        let rec = r.evidence.get(&id).await.unwrap().unwrap();
        assert!(rec.verified);
        assert_eq!(rec.promise_id.as_deref(), Some("cardano-tx-abc"));
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

        let reply = r.handle(message_id::CREDIT_NOTIFICATION, "corr-3", body.as_bytes()).await;
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

        let reply = r.handle(message_id::CREDIT_NOTIFICATION, "corr-4", body.as_bytes()).await;
        assert!(reply.is_ok());
        assert_eq!(reply.data["verified"], false);
        assert_eq!(counter.load(Ordering::SeqCst), 1);
    }

    #[tokio::test]
    async fn malformed_body_returns_bad_message() {
        let r = router_with_cbs("http://127.0.0.1:1/credit").await;
        let reply = r.handle(message_id::CREDIT_NOTIFICATION, "corr-5", b"{not json").await;
        assert_eq!(reply.status.error_code, error_code::BAD_MESSAGE);
    }

    #[tokio::test]
    async fn cbs_down_returns_delivery_failed() {
        let r = router_with_cbs("http://127.0.0.1:1/credit").await;
        let (body, _) = credit_with_valid_commitment();
        let reply = r.handle(message_id::CREDIT_NOTIFICATION, "corr-6", body.as_bytes()).await;
        assert_eq!(reply.status.error_code, error_code::CBS_DELIVERY_FAILED);
    }

    #[tokio::test]
    async fn netting_snapshot_and_status_update_are_acknowledged() {
        let r = router_with_cbs("http://127.0.0.1:1/credit").await;
        let n = r.handle(message_id::NETTING_SNAPSHOT, "c", br#"{"snapshot_id":"snap1"}"#).await;
        assert!(n.is_ok());
        let u = r
            .handle(message_id::STATUS_UPDATE, "c", br#"{"transaction_request_id":"tr-1","status":"COMPLETED"}"#)
            .await;
        assert!(u.is_ok());
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
        let r = router_with("http://127.0.0.1:1/credit", Some(settlement.clone())).await;

        let body = serde_json::json!({
            "settlement_id": "s1",
            "currency": "KES",
            "amount": "25000.00",
            "creditor_address": "addr_test1creditor",
            "idempotency_key": "idem-1",
        })
        .to_string();
        let reply = r.handle(message_id::SETTLEMENT_INSTRUCTION, "c", body.as_bytes()).await;

        assert!(reply.is_ok(), "expected ok, got {:?}", reply.status);
        assert_eq!(reply.data["tx_id"], "settle-tx-1");
        assert_eq!(reply.data["asset"], "ADA");
        assert_eq!(calls.load(Ordering::SeqCst), 1, "settle called once");
        // The debtor account was filled from the backend, and minor units parsed.
        assert_eq!(settlement.last_creditor.lock().unwrap().as_deref(), Some("addr_test1creditor"));
        assert_eq!(*settlement.last_minor.lock().unwrap(), Some(2_500_000)); // 25000.00 → cents
    }

    #[tokio::test]
    async fn settlement_instruction_without_backend_is_not_configured() {
        let r = router_with_cbs("http://127.0.0.1:1/credit").await; // settlement = None
        let body = br#"{"settlement_id":"s1","currency":"KES","amount":"10.00","creditor_address":"x"}"#;
        let reply = r.handle(message_id::SETTLEMENT_INSTRUCTION, "c", body).await;
        assert_eq!(reply.status.error_code, error_code::SETTLEMENT_NOT_CONFIGURED);
    }

    #[tokio::test]
    async fn settlement_failure_maps_to_settlement_failed() {
        // A backend that rejects (e.g. not the debtor) → SETTLEMENT_FAILED.
        struct Failing;
        #[async_trait::async_trait]
        impl SettlementBackend for Failing {
            fn system(&self) -> &str {
                "failing"
            }
            fn settles_from(&self) -> &str {
                "addr_test1me"
            }
            async fn settle(
                &self,
                _i: &BcSettlement,
            ) -> obp_blockchain::Result<obp_blockchain::settlement::SettlementOutcome> {
                Err(obp_blockchain::BlockchainError::Rejected("nope".into()))
            }
            async fn confirm(
                &self,
                _tx: &obp_blockchain::TxReference,
            ) -> obp_blockchain::Result<obp_blockchain::ConfirmationStatus> {
                Ok(obp_blockchain::ConfirmationStatus::Pending)
            }
        }
        let r = router_with("http://127.0.0.1:1/credit", Some(Arc::new(Failing))).await;
        let body = br#"{"settlement_id":"s1","currency":"KES","amount":"10.00","creditor_address":"x"}"#;
        let reply = r.handle(message_id::SETTLEMENT_INSTRUCTION, "c", body).await;
        assert_eq!(reply.status.error_code, error_code::SETTLEMENT_FAILED);
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
