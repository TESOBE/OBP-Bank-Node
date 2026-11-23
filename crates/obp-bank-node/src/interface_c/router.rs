// Copyright (C) 2025-2026 TESOBE GmbH
// SPDX-License-Identifier: AGPL-3.0-or-later
// Licensed under the GNU Affero General Public License v3 or later: see the LICENSE and NOTICE files.

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

/// Another OBP host the node is on, as a hub needs it to carry a payment on: where to look the final bank up, and the node's bank and corridor accounts there.
pub struct ForwardTarget {
    /// The OBP host.
    pub obp_host: String,
    /// The node's bank there, which makes the second leg.
    pub bank_id: String,
    /// The single-currency shorthand corridor account there.
    pub account_id: String,
    /// Currency (upper case) to corridor account there. Empty means every currency sends from `account_id`.
    pub corridor_accounts: std::collections::HashMap<String, String>,
    /// That OBP host's OBP-API client, to list its banks.
    pub obp: Arc<crate::obp_client::ObpClient>,
}

impl ForwardTarget {
    /// The corridor account that sends `currency` on this OBP host, if there is one.
    fn corridor_account_for(&self, currency: &str) -> Option<&str> {
        // No per-currency accounts: the shorthand account sends everything.
        if self.corridor_accounts.is_empty() {
            // The shorthand account.
            Some(&self.account_id)
        } else {
            // The account for this currency, if any.
            self.corridor_accounts.get(currency.trim().to_uppercase().as_str()).map(String::as_str)
        }
    }
}

/// Every OBP host the node is on, filled in once all of them are set up. Each router uses the ones other than its own.
pub type ForwardTargets = Arc<std::sync::OnceLock<Vec<ForwardTarget>>>;

pub struct Router {
    /// The OBP host whose messages this router handles: the node has one router per OBP host it is on.
    pub obp_host: String,
    /// This node's bank on that OBP host.
    pub bank_id: String,
    /// This node's corridor account — the institutional originator of return
    /// promises.
    account_id: String,
    evidence: EvidenceStore,
    cbs: CbsClient,
    /// The node's own outbound pipeline: a CBS-rejected credit is answered
    /// with a RETURN promise inserted here, which the dispatcher then drives
    /// exactly like an A1.1-initiated payment.
    outbox: crate::outbox::OutboxStore,
    /// `None` when this node has no settlement rail configured (e.g. the mock
    /// blockchain backend).
    settlement: Option<SettlementService>,
    /// The node's OBP hosts, for carrying a payment on to a bank on another of them. Empty unless set with `with_forward_targets`.
    forward_targets: ForwardTargets,
}

impl Router {
    pub fn new(
        obp_host: impl Into<String>,
        bank_id: impl Into<String>,
        account_id: impl Into<String>,
        evidence: EvidenceStore,
        cbs: CbsClient,
        outbox: crate::outbox::OutboxStore,
        settlement: Option<SettlementService>,
    ) -> Self {
        Self {
            // The OBP host the messages come from.
            obp_host: obp_host.into(),
            bank_id: bank_id.into(),
            account_id: account_id.into(),
            evidence,
            cbs,
            outbox,
            settlement,
            // No other OBP hosts until main sets them.
            forward_targets: Arc::new(std::sync::OnceLock::new()),
        }
    }

    /// Give the router the node's OBP hosts, so it can carry a payment on to a bank on another of them.
    pub fn with_forward_targets(mut self, forward_targets: ForwardTargets) -> Self {
        // Keep the shared list.
        self.forward_targets = forward_targets;
        self
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
            message_id::SETTLEMENT_ADVICE => self.settlement_advice(correlation_id, body).await,
            message_id::SETTLEMENT_INSTRUCTION => {
                self.settlement_instruction(correlation_id, body).await
            }
            message_id::NETTING_SNAPSHOT => self.netting_snapshot(correlation_id, body),
            message_id::STATUS_UPDATE => self.status_update(correlation_id, body).await,
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
        let beneficiary = cn.beneficiary.as_ref();
        let store_result = self
            .evidence
            .upsert(NewEvidence {
                transaction_request_id: &cn.transaction_request_id,
                // The OBP host the credit notification came from.
                obp_host: &self.obp_host,
                // This node's bank on it.
                bank_id: &self.bank_id,
                promise_commitment: cn.promise_commitment.as_deref().unwrap_or(""),
                promise_salt: cn.promise_salt.as_deref().unwrap_or(""),
                promise_preimage: cn.promise_preimage.as_deref().unwrap_or(""),
                promise_id: cn.promise_id.as_deref(),
                promise_blockchain: cn.promise_blockchain.as_deref(),
                verified,
                currency: Some(cn.value.currency.as_str()),
                amount: Some(cn.value.amount.as_str()),
                originator_name,
                beneficiary_name: beneficiary.map(|b| b.name.as_str()),
                beneficiary_account_routing_scheme: beneficiary
                    .map(|b| b.account_routing.scheme.as_str()),
                beneficiary_account_routing_address: beneficiary
                    .map(|b| b.account_routing.address.as_str()),
                return_of_transaction_request_id: cn.return_of.as_deref(),
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

        // A verified promise whose instruction names a final bank: this bank is the hub, so it carries the payment on instead of crediting a customer.
        if verified {
            // The final bank, if the instruction names one.
            if let Some(final_bank_routing) = final_bank_routing_of(&cn) {
                // Make the second leg (or return the first).
                return self.forward(correlation_id, &cn, final_bank_routing).await;
            }
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
            // A 4xx is the CBS itself refusing the credit (unknown account,
            // name mismatch, …) — permanent. The answer is a RETURN promise in
            // the opposite direction: the original promise settles normally
            // and the return nets against it, so the originator is repaid
            // through the same corridor machinery. One hop only — a refused
            // RETURN is never returned again; it parks STICKY for an operator
            // via CBS_REJECTED, as does a credit whose preimage is missing
            // (no return beneficiary derivable).
            // 5xx and transport errors stay FAILED: transient, retried.
            Err(crate::cbs::CbsError::Rejected { status, body }) if status < 500 => {
                warn!(status, %body, "credit_notification: CBS rejected the credit");
                if let Err(se) = self
                    .evidence
                    .record_cbs_result(&cn.transaction_request_id, "REJECTED", None)
                    .await
                {
                    warn!(error = %se, "credit_notification: failed to record CBS result");
                }
                if cn.return_of.is_some() {
                    return ReplyEnvelope::error(
                        correlation_id,
                        error_code::CBS_REJECTED,
                        format!(
                            "CBS rejected a RETURN credit ({status}): {body} — not returning a return"
                        ),
                    );
                }
                match self.initiate_return(&cn, &body).await {
                    Ok(return_id) => {
                        if let Err(e) = self
                            .evidence
                            .record_return(&cn.transaction_request_id, &return_id)
                            .await
                        {
                            warn!(error = %e, "credit_notification: failed to record return linkage");
                        }
                        info!(
                            transaction_request_id = %cn.transaction_request_id,
                            return_transaction_request_id = %return_id,
                            "credit_notification: CBS rejected — return promise initiated"
                        );
                        ReplyEnvelope::ok(
                            correlation_id,
                            serde_json::json!({
                                "transaction_request_id": cn.transaction_request_id,
                                "verified": verified,
                                "cbs_reference": null,
                                "cbs_status": "REJECTED",
                                "return_transaction_request_id": return_id,
                            }),
                        )
                    }
                    Err(reason) => ReplyEnvelope::error(
                        correlation_id,
                        error_code::CBS_REJECTED,
                        format!("CBS rejected the credit ({status}): {body}; no return initiated: {reason}"),
                    ),
                }
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

    /// Carry a credit on as the second leg of a payment through this bank, the hub: find the final bank on another of the node's OBP hosts, and put a payment to it in the outbox there. The dispatcher then makes it like any payment. Safe to repeat: a redelivered credit finds the second leg it already made.
    /// When no other OBP host has the final bank, or the node has no corridor account there for the currency, the first leg is returned to the originator, as for a credit the CBS refuses.
    /// This first version carries the payment on once the first leg's promise has arrived: the first leg does not wait for the second (no conditional promise yet), so the hub carries the risk between the two.
    async fn forward(
        &self,
        correlation_id: &str,
        cn: &CreditNotification,
        final_bank_routing: (String, String),
    ) -> ReplyEnvelope {
        // The final bank's routing scheme and address, for example BIC and DEMCZZ22.
        let (scheme, address) = final_bank_routing;
        // The node's other OBP hosts.
        let targets: Vec<&ForwardTarget> = self
            .forward_targets
            .get()
            // Every OBP host the node is on, or none yet.
            .map(|targets| targets.as_slice())
            .unwrap_or_default()
            .iter()
            // Not the one the credit came from.
            .filter(|target| target.obp_host != self.obp_host)
            .collect();
        // Where the final bank was found: the OBP host and its bank_id there.
        let mut found: Option<(&ForwardTarget, String)> = None;
        // OBP hosts whose banks could not be listed.
        let mut lookup_errors: Vec<String> = Vec::new();
        // Look on each other OBP host in turn.
        for target in targets {
            // That OBP host's Open Corridor bank with the routing, if any.
            match find_participant_bank(&target.obp, &scheme, &address).await {
                // Found it: stop looking.
                Ok(Some(bank_id)) => {
                    // Keep where it was found.
                    found = Some((target, bank_id));
                    break;
                }
                // Not on this OBP host.
                Ok(None) => {}
                // This OBP host could not be asked.
                Err(e) => lookup_errors.push(format!("OBP host {}: {e}", target.obp_host)),
            }
        }
        // The final bank was not found anywhere.
        let Some((target, final_bank_id)) = found else {
            // An OBP host that could not be asked might have it: try again later rather than return the money.
            if !lookup_errors.is_empty() {
                // The reason, for the log and the reply.
                let reason = format!("could not look up {scheme} {address}: {}", lookup_errors.join("; "));
                warn!(transaction_request_id = %cn.transaction_request_id, %reason, "credit_notification: cannot carry the payment on yet");
                // Transient: OBP-API delivers the credit again.
                return ReplyEnvelope::error(correlation_id, error_code::CBS_DELIVERY_FAILED, reason);
            }
            // No route: return the first leg.
            return self
                .refuse_forward(correlation_id, cn, &format!("no route to the final bank {scheme} {address} from this hub"))
                .await;
        };
        // The corridor account that sends this currency on the final bank's OBP host.
        let Some(account_id) = target.corridor_account_for(&cn.value.currency) else {
            // Without one the hub cannot pay there: return the first leg.
            return self
                .refuse_forward(
                    correlation_id,
                    cn,
                    &format!("this hub has no corridor account for {} on OBP host {}", cn.value.currency, target.obp_host),
                )
                .await;
        };
        // The second leg, built from the first leg's instruction.
        let request = match self.second_leg(cn, &scheme, &address) {
            // Built.
            Ok(request) => request,
            // The first leg's instruction is missing something the second leg needs: return the first leg.
            Err(reason) => return self.refuse_forward(correlation_id, cn, &reason).await,
        };
        // The second leg as the outbox stores it.
        let payload = match serde_json::to_string(&request) {
            // Serialized.
            Ok(payload) => payload,
            // Cannot happen for these types; reported as a platform error.
            Err(e) => {
                return ReplyEnvelope::error(correlation_id, error_code::PLATFORM, format!("serializing the second leg: {e}"))
            }
        };
        // One second leg per first leg, however often the credit is delivered.
        let idempotency_key = format!("forward:{}:{}", self.obp_host, cn.transaction_request_id);
        // A new id for the second leg.
        let new_id = uuid::Uuid::new_v4().to_string();
        // A fresh salt for its promise commitment.
        let salt = uuid::Uuid::new_v4().simple().to_string();
        // Put it in the outbox on the final bank's OBP host.
        let inserted = self
            .outbox
            .insert_with_idempotency_key(
                crate::outbox::NewEntry {
                    transaction_request_id: &new_id,
                    // The OBP host the final bank is on.
                    obp_host: &target.obp_host,
                    // The hub's bank there.
                    bank_id: &target.bank_id,
                    // Its corridor account for the currency.
                    account_id,
                    request_payload: &payload,
                    commitment_salt: &salt,
                },
                &idempotency_key,
            )
            .await;
        // The second leg's id: the new one, or the one an earlier delivery made.
        let second_leg_id = match inserted {
            // Made now.
            Ok(()) => new_id,
            // Made by an earlier delivery of this credit: use that one.
            Err(crate::outbox::OutboxError::DuplicateIdempotencyKey) => match self.outbox.get_by_idempotency_key(&idempotency_key).await {
                // The earlier second leg.
                Ok(Some(existing)) => existing.transaction_request_id,
                // Should not happen; retried.
                _ => {
                    return ReplyEnvelope::error(correlation_id, error_code::PLATFORM, "could not read back the second leg")
                }
            },
            // The outbox could not be written; retried.
            Err(e) => {
                return ReplyEnvelope::error(correlation_id, error_code::PLATFORM, format!("persisting the second leg: {e}"))
            }
        };
        // Note on the credit's evidence that it was carried on, and where.
        if let Err(e) = self.evidence.record_forward(&cn.transaction_request_id, &second_leg_id, &target.obp_host).await {
            warn!(error = %e, "credit_notification: failed to record the second leg");
        }
        // Not delivered to the CBS: carried on.
        if let Err(e) = self.evidence.record_cbs_result(&cn.transaction_request_id, "FORWARDED", None).await {
            warn!(error = %e, "credit_notification: failed to record the forward");
        }
        info!(
            transaction_request_id = %cn.transaction_request_id,
            second_leg_transaction_request_id = %second_leg_id,
            obp_host = %target.obp_host,
            final_bank_id = %final_bank_id,
            "credit_notification: carried on to the final bank as a second leg"
        );
        // Tell OBP-API the credit was handled.
        ReplyEnvelope::ok(
            correlation_id,
            serde_json::json!({
                "transaction_request_id": cn.transaction_request_id,
                "verified": true,
                "cbs_reference": null,
                "cbs_status": "FORWARDED",
                "forwarded_as_transaction_request_id": second_leg_id,
                "forwarded_on_obp_host": target.obp_host,
            }),
        )
    }

    /// The second leg of a payment through this hub: the same value, beneficiary, originator and charge policy as the first leg's instruction, paid to the final bank, and naming the first leg.
    fn second_leg(
        &self,
        cn: &CreditNotification,
        scheme: &str,
        address: &str,
    ) -> Result<crate::rest::types::InitiateRequest, String> {
        use crate::rest::types as a1;
        // The first leg's instruction, from the promise preimage (verified before this is called).
        let preimage: serde_json::Value = cn
            .promise_preimage
            .as_deref()
            // Parse it.
            .map(serde_json::from_str)
            // No preimage.
            .ok_or("no preimage on the credit")?
            // Not JSON.
            .map_err(|e| format!("preimage is not JSON: {e}"))?;
        // The instruction the first leg's originating bank sent.
        let instruction = &preimage["instruction"];
        // The final beneficiary: the first leg's `to`.
        let mut to: a1::BeneficiaryRouting = serde_json::from_value(instruction["to"].clone())
            // Say what was wrong.
            .map_err(|e| format!("the first leg's beneficiary cannot be read: {e}"))?;
        // Paid to the final bank, not to this hub.
        to.other_bank_routing_scheme = scheme.to_string();
        to.other_bank_routing_address = address.to_string();
        // The Travel Rule originator travels on unchanged.
        let originator: a1::Originator = serde_json::from_value(instruction["originator"].clone())
            // Say what was wrong.
            .map_err(|e| format!("the first leg's originator cannot be read: {e}"))?;
        // The first leg's description, kept short.
        let first_description = instruction["description"].as_str().unwrap_or_default();
        Ok(a1::InitiateRequest {
            // The same value: this version adds no hub fee and no currency conversion.
            value: a1::MoneyValue {
                currency: cn.value.currency.clone(),
                amount: cn.value.amount.clone(),
            },
            // Says which payment this carries on.
            description: format!("Carried on from {} (OBP host {}): {}", cn.transaction_request_id, self.obp_host, first_description)
                .chars()
                .take(250)
                .collect(),
            to,
            originator,
            // The first leg's charge policy.
            charge_policy: instruction["charge_policy"].as_str().unwrap_or("SHARED").to_string(),
            // Not a return.
            return_of: None,
            // The final bank is reached: no further hub.
            final_bank_routing: None,
            // The first leg, which this one carries on.
            previous_leg: Some(a1::PreviousLeg {
                obp_host: self.obp_host.clone(),
                transaction_request_id: cn.transaction_request_id.clone(),
                promise_commitment: cn.promise_commitment.clone().unwrap_or_default(),
            }),
        })
    }

    /// The first leg cannot be carried on: return it to the originator, as for a credit the CBS refuses, and record why.
    async fn refuse_forward(&self, correlation_id: &str, cn: &CreditNotification, reason: &str) -> ReplyEnvelope {
        warn!(transaction_request_id = %cn.transaction_request_id, %reason, "credit_notification: cannot carry the payment on");
        // Not delivered, not carried on.
        if let Err(e) = self.evidence.record_cbs_result(&cn.transaction_request_id, "NOT_FORWARDED", None).await {
            warn!(error = %e, "credit_notification: failed to record the refusal");
        }
        // Return the money to the originator.
        match self.initiate_return(cn, reason).await {
            // The return is on its way.
            Ok(return_id) => {
                // Link the return to the credit.
                if let Err(e) = self.evidence.record_return(&cn.transaction_request_id, &return_id).await {
                    warn!(error = %e, "credit_notification: failed to record return linkage");
                }
                ReplyEnvelope::ok(
                    correlation_id,
                    serde_json::json!({
                        "transaction_request_id": cn.transaction_request_id,
                        "verified": true,
                        "cbs_reference": null,
                        "cbs_status": "NOT_FORWARDED",
                        "return_transaction_request_id": return_id,
                    }),
                )
            }
            // No return possible either: parked for an operator.
            Err(return_reason) => ReplyEnvelope::error(
                correlation_id,
                error_code::CBS_REJECTED,
                format!("{reason}; no return initiated: {return_reason}"),
            ),
        }
    }

    /// Build and enqueue the RETURN promise for a CBS-rejected credit: same
    /// amount and currency back to the ORIGINAL ORIGINATOR, whose identity and
    /// account routing come out of the promise preimage (the canonical A1.1
    /// instruction Bank A hashed). Enters this node's normal outbound pipeline
    /// — outbox, dispatcher, OBP submit, on-chain promise — like any payment.
    async fn initiate_return(
        &self,
        cn: &CreditNotification,
        reason: &str,
    ) -> Result<String, String> {
        use crate::rest::types as a1;

        let preimage = cn
            .promise_preimage
            .as_deref()
            .ok_or("no preimage on the credit — cannot derive the return beneficiary")?;
        let preimage: serde_json::Value = serde_json::from_str(preimage)
            .map_err(|e| format!("preimage is not JSON: {e}"))?;
        let originating_bank_id = preimage["originating_bank_id"]
            .as_str()
            .filter(|s| !s.trim().is_empty())
            .ok_or("preimage has no originating_bank_id")?;
        let orig = &preimage["instruction"]["originator"];
        let orig_name = orig["name"].as_str().unwrap_or_default();
        let orig_scheme = orig["account_routing"]["scheme"].as_str().unwrap_or_default();
        let orig_address = orig["account_routing"]["address"].as_str().unwrap_or_default();
        if orig_scheme.trim().is_empty() || orig_address.trim().is_empty() {
            return Err("preimage originator has no account routing".into());
        }

        let reason = reason.chars().take(120).collect::<String>();
        let request = a1::InitiateRequest {
            value: a1::MoneyValue {
                currency: cn.value.currency.clone(),
                amount: cn.value.amount.clone(),
            },
            description: format!("Return of {}: {}", cn.transaction_request_id, reason),
            to: a1::BeneficiaryRouting {
                name: orig_name.to_string(),
                description: format!("Return to originator of {}", cn.transaction_request_id),
                other_bank_routing_scheme: "OBP".into(),
                other_bank_routing_address: originating_bank_id.to_string(),
                other_account_routing_scheme: orig_scheme.to_string(),
                other_account_routing_address: orig_address.to_string(),
                other_account_secondary_routing_scheme: String::new(),
                other_account_secondary_routing_address: String::new(),
                other_branch_routing_scheme: String::new(),
                other_branch_routing_address: String::new(),
            },
            // The bank itself is the Travel Rule originator of a return — the
            // rejected credit never reached a customer.
            originator: a1::Originator {
                name: format!("Bank {} (return)", self.bank_id),
                address: format!("Settlement account {} at {}", self.account_id, self.bank_id),
                account_routing: a1::AccountRouting {
                    scheme: "OBP".into(),
                    address: self.account_id.clone(),
                },
                source: None,
            },
            charge_policy: "SHARED".into(),
            return_of: Some(cn.transaction_request_id.clone()),
            // A return goes straight back to the originating bank.
            final_bank_routing: None,
            // Not a second leg.
            previous_leg: None,
        };
        let payload =
            serde_json::to_string(&request).map_err(|e| format!("serializing return: {e}"))?;

        let return_id = uuid::Uuid::new_v4().to_string();
        let salt = uuid::Uuid::new_v4().simple().to_string();
        self.outbox
            .insert(crate::outbox::NewEntry {
                transaction_request_id: &return_id,
                // The return is made on the OBP host the credit came from.
                obp_host: &self.obp_host,
                bank_id: &self.bank_id,
                account_id: &self.account_id,
                request_payload: &payload,
                commitment_salt: &salt,
            })
            .await
            .map_err(|e| format!("persisting return to outbox: {e}"))?;
        Ok(return_id)
    }

    /// `obp_settlement_advice`: a netted settle covered promises of this node's
    /// corridor. The covered list is the pair's FULL list (both directions), so
    /// stamp both sides of this node's records: the credits it already paid out
    /// (evidence store) and its own outbound promises (outbox) — ids belonging
    /// to the counterparty's records match nothing and are ignored. This is how
    /// a node that did not trigger the settle learns of the coverage. Idempotent:
    /// only unstamped rows are touched, so an at-least-once redelivery preserves
    /// the original settled_at.
    async fn settlement_advice(&self, correlation_id: &str, body: &[u8]) -> ReplyEnvelope {
        let advice: SettlementAdvice = match serde_json::from_slice(body) {
            Ok(v) => v,
            Err(e) => {
                warn!(error = %e, "settlement_advice: malformed body");
                return ReplyEnvelope::error(
                    correlation_id,
                    error_code::BAD_MESSAGE,
                    format!("malformed settlement_advice: {e}"),
                );
            }
        };
        // As creditor, keep the settlement so its payment can be checked when
        // it arrives (incoming.rs); the advice itself comes before the payment.
        if advice.creditor_bank_id.as_deref() == Some(self.bank_id.as_str()) {
            if let Some(svc) = &self.settlement {
                if let Err(e) = svc
                    .store
                    .record_incoming(
                        // The OBP host the advice came from.
                        &self.obp_host,
                        // This node's bank on it: the creditor.
                        &self.bank_id,
                        &advice.settlement_id,
                        advice.debtor_bank_id.as_deref(),
                        advice.currency.as_deref(),
                        advice.net_amount.as_deref(),
                        &advice.covered_transaction_request_ids,
                    )
                    .await
                {
                    warn!(error = %e, "settlement_advice: failed to record the incoming settlement");
                }
            }
        }
        let credits = self
            .evidence
            .mark_settled(&advice.covered_transaction_request_ids, &advice.settlement_id)
            .await;
        let promises = self
            .outbox
            // Only the promises made on this router's OBP host.
            .mark_settled(&self.obp_host, &advice.covered_transaction_request_ids, &advice.settlement_id)
            .await;
        match (credits, promises) {
            (Ok(credits_stamped), Ok(promises_stamped)) => {
                info!(
                    bank_id = %self.bank_id,
                    settlement_id = %advice.settlement_id,
                    covered = advice.covered_transaction_request_ids.len(),
                    credits_stamped,
                    promises_stamped,
                    "settlement_advice: credits and promises stamped settled"
                );
                ReplyEnvelope::ok(
                    correlation_id,
                    serde_json::json!({
                        "settlement_id": advice.settlement_id,
                        "acknowledged": true,
                    }),
                )
            }
            (credits, promises) => {
                if let Err(e) = &credits {
                    warn!(error = %e, "settlement_advice: failed to stamp credits");
                }
                if let Err(e) = &promises {
                    warn!(error = %e, "settlement_advice: failed to stamp promises");
                }
                ReplyEnvelope::error(
                    correlation_id,
                    error_code::PLATFORM,
                    "failed to stamp settled rows",
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
                // The OBP host the instruction came from.
                obp_host: &self.obp_host,
                // This node's bank on it: the debtor.
                bank_id: &self.bank_id,
                settlement_id: si.settlement_id.as_deref(),
                snapshot_id: si.snapshot_id.as_deref(),
                currency: si.currency.as_deref().unwrap_or_default(),
                net_amount_minor,
                creditor_address: &creditor_address,
                purpose: si.purpose.as_deref(),
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
                        outcome.fx.as_ref(),
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
                ReplyEnvelope::error(correlation_id, failure_code(&e.to_string()), e.to_string())
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

    /// `obp_status_update`: a TR's status changed. Stored on the matching
    /// outbox row as `obp_status` (by OBP-API's id, else by the node's own)
    /// so A1.2 shows it; the node's own lifecycle is not touched. The reply
    /// says whether a row matched, so an update for an unknown id is visible
    /// on the OBP-API side.
    async fn status_update(&self, correlation_id: &str, body: &[u8]) -> ReplyEnvelope {
        match serde_json::from_slice::<StatusUpdate>(body) {
            Ok(su) => {
                let matched = match self
                    .outbox
                    .record_obp_status(&su.transaction_request_id, &su.status)
                    .await
                {
                    Ok(m) => m,
                    Err(e) => {
                        return ReplyEnvelope::error(
                            correlation_id,
                            error_code::PLATFORM,
                            format!("recording status_update: {e}"),
                        )
                    }
                };
                info!(
                    bank_id = %self.bank_id,
                    transaction_request_id = %su.transaction_request_id,
                    status = %su.status,
                    matched,
                    "status_update received"
                );
                ReplyEnvelope::ok(correlation_id, serde_json::json!({ "recorded": true, "matched": matched }))
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
/// The error code for a failed settlement, from its message (also the stored
/// reason, so a redelivery answers with the same code).
fn failure_code(reason: &str) -> &'static str {
    if reason.starts_with(obp_blockchain::BELOW_MINIMUM_PREFIX) {
        error_code::SETTLEMENT_BELOW_MINIMUM
    } else {
        error_code::SETTLEMENT_FAILED
    }
}

fn reply_for_existing(
    correlation_id: &str,
    row: &SettlementRow,
    finality_depth: u32,
) -> ReplyEnvelope {
    match row.status.as_str() {
        settlement_store::status::ERROR => {
            let reason = row.error_reason.clone().unwrap_or_else(|| "settlement failed".into());
            ReplyEnvelope::error(correlation_id, failure_code(&reason), reason)
        }
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

/// The final bank a promise's instruction names (`final_bank_routing`), as scheme and address, when this bank is the hub that should carry it on. None for an ordinary payment, a return, or a credit without a readable preimage.
fn final_bank_routing_of(cn: &CreditNotification) -> Option<(String, String)> {
    // A return goes back the way it came, never on.
    if cn.return_of.is_some() {
        return None;
    }
    // The instruction the originating bank hashed.
    let preimage: serde_json::Value = serde_json::from_str(cn.promise_preimage.as_deref()?).ok()?;
    // Its final bank, if any.
    let routing = &preimage["instruction"]["final_bank_routing"];
    // The scheme, for example BIC.
    let scheme = routing["scheme"].as_str()?.trim();
    // The address, for example DEMCZZ22.
    let address = routing["address"].as_str()?.trim();
    // Both are needed.
    if scheme.is_empty() || address.is_empty() {
        return None;
    }
    Some((scheme.to_string(), address.to_string()))
}

/// The bank_id of the Open Corridor participant on an OBP host whose bank routing matches (scheme and address compared without case), from that OBP-API's getBanks. None when no participant there has it.
async fn find_participant_bank(
    obp: &crate::obp_client::ObpClient,
    scheme: &str,
    address: &str,
) -> Result<Option<String>, String> {
    // The OBP host's banks.
    let banks = obp.get_banks().await.map_err(|e| e.to_string())?;
    // Each bank in the list.
    for bank in banks["banks"].as_array().cloned().unwrap_or_default() {
        // Does one of its routings match?
        let routing_matches = bank["bank_routings"].as_array().cloned().unwrap_or_default().iter().any(|routing| {
            // Same scheme.
            routing["scheme"].as_str().is_some_and(|s| s.eq_ignore_ascii_case(scheme))
                // Same address.
                && routing["address"].as_str().is_some_and(|a| a.trim().eq_ignore_ascii_case(address))
        });
        // Is it an Open Corridor participant there?
        let participant = bank["attributes"].as_array().cloned().unwrap_or_default().iter().any(|attribute| {
            // The participant mark the setup page sets.
            attribute["name"].as_str() == Some("OPEN_CORRIDOR_PARTICIPANT")
                // Set to true.
                && attribute["value"].as_str().is_some_and(|v| v.eq_ignore_ascii_case("true"))
        });
        // Both: this is the final bank.
        if routing_matches && participant {
            return Ok(bank["bank_id"].as_str().map(str::to_string));
        }
    }
    // No participant on this OBP host has the routing.
    Ok(None)
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
        let outbox = crate::outbox::OutboxStore::connect_in_memory()
            .await
            .unwrap();
        (
            Router::new("test-host-a:8080", "ke.01.kcs", "settlement-ke", evidence, cbs, outbox, service),
            store,
        )
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
            "beneficiary": {
                "name": "Bea Beneficiary",
                "account_routing": { "scheme": "OBP", "address": "acct-77" },
            },
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
    async fn settlement_advice_records_an_incoming_settlement_only_as_creditor() {
        let settlement = Arc::new(MockSettlement {
            from: "addr_test1me".into(),
            calls: Arc::new(AtomicUsize::new(0)),
            last_creditor: std::sync::Mutex::new(None),
            last_minor: std::sync::Mutex::new(None),
        });
        let (r, store) = router_with("http://127.0.0.1:1/credit", Some(settlement)).await;
        let store = store.unwrap();
        let advice = |id: &str, creditor: &str| {
            serde_json::json!({
                "settlement_id": id, "currency": "KES", "net_amount": "1530.00",
                "debtor_bank_id": "rt.bank.a", "creditor_bank_id": creditor,
                "covered_transaction_request_ids": ["tr-1", "tr-2"],
            })
            .to_string()
        };
        // This router's bank is ke.01.kcs: creditor of one, debtor side of the other.
        for (id, creditor) in [("in-1", "ke.01.kcs"), ("out-1", "rt.bank.b")] {
            let reply = r
                .handle(message_id::SETTLEMENT_ADVICE, "c", advice(id, creditor).as_bytes())
                .await;
            assert!(reply.is_ok(), "{:?}", reply.status);
        }
        // Redelivered: still one row, progress kept.
        r.handle(message_id::SETTLEMENT_ADVICE, "c", advice("in-1", "ke.01.kcs").as_bytes())
            .await;

        // Listed under the router's OBP host and bank.
        let rows = store.list_incoming("test-host-a:8080", "ke.01.kcs", 10).await.unwrap();
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].settlement_id, "in-1");
        assert_eq!(rows[0].status, crate::settlement_store::incoming_status::NETTED);
        assert_eq!(rows[0].net_amount.as_deref(), Some("1530.00"));
        assert_eq!(rows[0].debtor_bank_id.as_deref(), Some("rt.bank.a"));
    }

    #[tokio::test]
    async fn settlement_advice_stamps_credits_and_own_promises_settled() {
        let r = router_with_cbs("http://127.0.0.1:1/credit").await;
        for id in ["tr-s1", "tr-s2"] {
            r.evidence
                .upsert(crate::evidence::NewEvidence {
                    transaction_request_id: id,
                    // The OBP host the notification came from.
                    obp_host: "test-host-a:8080",
                    // The node's bank there.
                    bank_id: "ke.01.kcs",
                    promise_commitment: "c",
                    promise_salt: "s",
                    promise_preimage: "p",
                    promise_id: None,
                    promise_blockchain: None,
                    verified: true,
                    currency: Some("KES"),
                    amount: Some("3.00"),
                    originator_name: None,
                    beneficiary_name: None,
                    beneficiary_account_routing_scheme: None,
                    beneficiary_account_routing_address: None,
                    return_of_transaction_request_id: None,
                    raw_message: "{}",
                })
                .await
                .unwrap();
        }

        // Two of this node's own outbound promises: one covered by the settle
        // (matched via its OBP TR id), one outside it.
        for (local_id, obp_id) in [("local-1", "tr-mine"), ("local-2", "tr-other")] {
            r.outbox
                .insert(crate::outbox::NewEntry {
                    transaction_request_id: local_id,
                    // The OBP host the row belongs to: the router's own.
                    obp_host: "test-host-a:8080",
                    bank_id: "ke.01.kcs",
                    account_id: "settlement-ke",
                    request_payload: "{}",
                    commitment_salt: "salt",
                })
                .await
                .unwrap();
            r.outbox.mark_submitted(local_id, Some(obp_id)).await.unwrap();
        }

        // The covered list is the pair's FULL list: this node's received
        // credits, its own outbound promise, and a counterparty id that
        // matches nothing here.
        let body = serde_json::json!({
            "settlement_id": "settle-77",
            "currency": "KES",
            "net_amount": "0.00",
            "debtor_bank_id": "rt.bank.a",
            "creditor_bank_id": "rt.bank.b",
            "covered_transaction_request_ids": ["tr-s1", "tr-s2", "tr-mine", "tr-theirs"],
            "idempotency_key": "settle-77",
        })
        .to_string();
        let reply = r
            .handle(message_id::SETTLEMENT_ADVICE, "corr-adv", body.as_bytes())
            .await;
        assert!(reply.is_ok(), "expected ok, got {:?}", reply.status);
        assert_eq!(reply.data["settlement_id"], "settle-77");
        assert_eq!(reply.data["acknowledged"], true);

        let rec = r.evidence.get("tr-s1").await.unwrap().unwrap();
        assert_eq!(rec.settlement_id.as_deref(), Some("settle-77"));
        assert!(rec.settled_at.is_some());

        // The node's own covered promise is stamped; the uncovered one is not.
        let mine = r.outbox.get("local-1").await.unwrap().unwrap();
        assert_eq!(mine.settlement_id.as_deref(), Some("settle-77"));
        assert!(mine.settled_at.is_some());
        let other = r.outbox.get("local-2").await.unwrap().unwrap();
        assert!(other.settlement_id.is_none());

        // Redelivery stays ok and does not re-stamp.
        let reply = r
            .handle(message_id::SETTLEMENT_ADVICE, "corr-adv-2", body.as_bytes())
            .await;
        assert!(reply.is_ok());

        // Malformed body is refused, never panics.
        let reply = r
            .handle(message_id::SETTLEMENT_ADVICE, "corr-bad", b"not json")
            .await;
        assert!(!reply.is_ok());
        assert_eq!(reply.status.error_code, error_code::BAD_MESSAGE);
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

        // Evidence persisted, marked verified, CBS outcome recorded — and the
        // beneficiary (whom the CBS credited) surfaced for display.
        let rec = r.evidence.get(&id).await.unwrap().unwrap();
        assert!(rec.verified);
        assert_eq!(rec.promise_id.as_deref(), Some("cardano-tx-abc"));
        assert_eq!(rec.cbs_status.as_deref(), Some("DELIVERED"));
        assert_eq!(rec.cbs_reference.as_deref(), Some("CBS-1"));
        assert_eq!(rec.beneficiary_name.as_deref(), Some("Bea Beneficiary"));
        assert_eq!(
            rec.beneficiary_account_routing_scheme.as_deref(),
            Some("OBP")
        );
        assert_eq!(
            rec.beneficiary_account_routing_address.as_deref(),
            Some("acct-77")
        );
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

    /// Spawn a CBS stub answering every credit with the given status + body.
    async fn cbs_stub_fixed(status: axum::http::StatusCode, body: &'static str) -> String {
        let router = AxumRouter::new().route("/credit", post(move || async move { (status, body) }));
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr: SocketAddr = listener.local_addr().unwrap();
        tokio::spawn(async move { axum::serve(listener, router).await.unwrap() });
        format!("http://{addr}/credit")
    }

    #[tokio::test]
    async fn cbs_4xx_is_a_permanent_rejection() {
        let url = cbs_stub_fixed(
            axum::http::StatusCode::UNPROCESSABLE_ENTITY,
            r#"{"status":"REJECTED","error":"unknown beneficiary account"}"#,
        )
        .await;
        let r = router_with_cbs(&url).await;
        let (body, id) = credit_with_valid_commitment();
        let reply = r
            .handle(message_id::CREDIT_NOTIFICATION, "corr-7", body.as_bytes())
            .await;
        // Distinct error code: OBP-API parks the outbox row STICKY on it
        // instead of redelivering a credit the CBS will refuse again.
        assert_eq!(reply.status.error_code, error_code::CBS_REJECTED);
        assert!(reply
            .status
            .backend_messages
            .iter()
            .any(|m| m.contains("422")));
        let rec = r.evidence.get(&id).await.unwrap().unwrap();
        assert_eq!(rec.cbs_status.as_deref(), Some("REJECTED"));
        assert!(rec.cbs_reference.is_none());
    }

    /// A credit whose preimage is the full canonical instruction — enough to
    /// derive the return beneficiary. `return_of` marks it as itself a return.
    fn credit_returnable(return_of: Option<&str>) -> (String, String) {
        let instruction = serde_json::json!({
            "value": {"currency": "KES", "amount": "250.00"},
            "description": "Original payment",
            "to": {
                "name": "Bea Beneficiary", "description": "",
                "other_bank_routing_scheme": "OBP", "other_bank_routing_address": "ke.01.kcs",
                "other_account_routing_scheme": "OBP", "other_account_routing_address": "no-such-acct",
                "other_account_secondary_routing_scheme": "", "other_account_secondary_routing_address": "",
                "other_branch_routing_scheme": "", "other_branch_routing_address": ""
            },
            "originator": {
                "name": "Alice Sender", "address": "1 Sender Street",
                "account_routing": {"scheme": "IBAN", "address": "KE93 0000 1234"}
            },
            "charge_policy": "SHARED",
        });
        let preimage = serde_json::json!({
            "transaction_request_id": "tr-orig-9",
            "originating_bank_id": "rt.bank.a",
            "originating_account_id": "settlement-a",
            "instruction": instruction,
        })
        .to_string();
        let salt = "00112233445566778899aabbccddeeff";
        let commitment = PromiseRecord::compute_commitment(preimage.as_bytes(), salt.as_bytes());
        let mut body = serde_json::json!({
            "transaction_request_id": "tr-orig-9",
            "value": {"currency": "KES", "amount": "250.00"},
            "originator": {"name": "Alice Sender"},
            "beneficiary": {
                "name": "Bea Beneficiary",
                "account_routing": {"scheme": "OBP", "address": "no-such-acct"},
            },
            "promise_id": "tx-9", "promise_blockchain": "cardano",
            "promise_commitment": commitment, "promise_salt": salt, "promise_preimage": preimage,
        });
        if let Some(r) = return_of {
            body["return_of"] = serde_json::json!(r);
        }
        (body.to_string(), "tr-orig-9".into())
    }

    #[tokio::test]
    async fn cbs_rejection_initiates_a_return_promise() {
        let url = cbs_stub_fixed(
            axum::http::StatusCode::UNPROCESSABLE_ENTITY,
            r#"{"status":"REJECTED","error":"unknown beneficiary account"}"#,
        )
        .await;
        let r = router_with_cbs(&url).await;
        let (body, id) = credit_returnable(None);
        let reply = r
            .handle(message_id::CREDIT_NOTIFICATION, "corr-9", body.as_bytes())
            .await;
        // The rejection is answered with a RETURN, so the message itself is
        // handled — success reply carrying the return's id.
        assert!(reply.is_ok(), "expected ok, got {:?}", reply.status);
        assert_eq!(reply.data["cbs_status"], "REJECTED");
        let return_id = reply.data["return_transaction_request_id"]
            .as_str()
            .unwrap()
            .to_string();

        // The return entered this node's outbound pipeline: same value, back
        // to the ORIGINAL ORIGINATOR's routing, linked via return_of.
        let rows = r.outbox.list("test-host-a:8080", "ke.01.kcs", 10).await.unwrap();
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].transaction_request_id, return_id);
        assert_eq!(rows[0].bank_id, "ke.01.kcs");
        assert_eq!(rows[0].account_id, "settlement-ke");
        let payload: serde_json::Value = serde_json::from_str(&rows[0].request_payload).unwrap();
        assert_eq!(payload["return_of"], id);
        assert_eq!(payload["value"]["amount"], "250.00");
        assert_eq!(payload["value"]["currency"], "KES");
        assert_eq!(payload["to"]["name"], "Alice Sender");
        assert_eq!(payload["to"]["other_bank_routing_scheme"], "OBP");
        assert_eq!(payload["to"]["other_bank_routing_address"], "rt.bank.a");
        assert_eq!(payload["to"]["other_account_routing_scheme"], "IBAN");
        assert_eq!(payload["to"]["other_account_routing_address"], "KE93 0000 1234");
        assert_eq!(payload["originator"]["account_routing"]["address"], "settlement-ke");

        // Rejection + linkage recorded on the evidence row.
        let rec = r.evidence.get(&id).await.unwrap().unwrap();
        assert_eq!(rec.cbs_status.as_deref(), Some("REJECTED"));
        assert_eq!(
            rec.returned_by_transaction_request_id.as_deref(),
            Some(return_id.as_str())
        );
    }

    /// A credit for a payment through this bank as the hub: `to` names this bank, with the final beneficiary's account, and `final_bank_routing` names the final bank by BIC.
    fn credit_through_hub() -> (String, String) {
        // The instruction the originating bank hashed.
        let instruction = serde_json::json!({
            "value": {"currency": "KES", "amount": "300.00"},
            "description": "School fees",
            "to": {
                "name": "Cara Customer", "description": "",
                "other_bank_routing_scheme": "BIC", "other_bank_routing_address": "DEMBZZ22",
                "other_account_routing_scheme": "ACCOUNT_NUMBER", "other_account_routing_address": "c-123",
                "other_account_secondary_routing_scheme": "", "other_account_secondary_routing_address": "",
                "other_branch_routing_scheme": "", "other_branch_routing_address": ""
            },
            "originator": {
                "name": "Alice Sender", "address": "1 Sender Street",
                "account_routing": {"scheme": "IBAN", "address": "KE93 0000 1234"}
            },
            "charge_policy": "SHARED",
            "final_bank_routing": {"scheme": "BIC", "address": "DEMCZZ22"},
        });
        // The preimage around it.
        let preimage = serde_json::json!({
            "transaction_request_id": "tr-leg-1",
            "originating_bank_id": "rt.bank.a",
            "originating_account_id": "settlement-a",
            "instruction": instruction,
        })
        .to_string();
        // A valid commitment over it.
        let salt = "00112233445566778899aabbccddeeff";
        let commitment = PromiseRecord::compute_commitment(preimage.as_bytes(), salt.as_bytes());
        // The credit notification.
        let body = serde_json::json!({
            "transaction_request_id": "tr-leg-1",
            "value": {"currency": "KES", "amount": "300.00"},
            "originator": {"name": "Alice Sender"},
            "beneficiary": {"name": "Cara Customer", "account_routing": {"scheme": "ACCOUNT_NUMBER", "address": "c-123"}},
            "promise_id": "tx-leg-1", "promise_blockchain": "cardano",
            "promise_commitment": commitment, "promise_salt": salt, "promise_preimage": preimage,
        });
        (body.to_string(), "tr-leg-1".into())
    }

    /// A stub OBP-API answering getBanks with the given banks body, or with a 500 when `banks` is None.
    async fn obp_banks_stub(banks: Option<serde_json::Value>) -> String {
        // The one route the forward uses.
        let router = AxumRouter::new().route(
            "/obp/v6.0.0/banks",
            axum::routing::get(move || {
                // Each request gets its own copy.
                let banks = banks.clone();
                async move {
                    match banks {
                        // The banks, as OBP-API sends them.
                        Some(banks) => (axum::http::StatusCode::OK, Json(banks)),
                        // OBP-API failing.
                        None => (axum::http::StatusCode::INTERNAL_SERVER_ERROR, Json(serde_json::json!({"code": 500, "message": "OBP-50000: Unknown Error."}))),
                    }
                }
            }),
        );
        // Serve it on a free port.
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr: SocketAddr = listener.local_addr().unwrap();
        tokio::spawn(async move { axum::serve(listener, router).await.unwrap() });
        format!("http://{addr}")
    }

    /// A router on test-host-a:8080 whose node is also on test-host-b:8081, whose OBP-API is `obp_b_url`.
    async fn hub_router(obp_b_url: &str) -> Router {
        // The CBS is never called when a credit is carried on.
        let router = router_with_cbs("http://127.0.0.1:1/credit").await;
        // Both OBP hosts the node is on.
        let targets = vec![
            ForwardTarget {
                obp_host: "test-host-a:8080".into(),
                bank_id: "ke.01.kcs".into(),
                account_id: "settlement-ke".into(),
                corridor_accounts: Default::default(),
                obp: Arc::new(crate::obp_client::ObpClient::new("http://127.0.0.1:1", crate::obp_client::ObpAuth::None).unwrap()),
            },
            ForwardTarget {
                obp_host: "test-host-b:8081".into(),
                bank_id: "ke.01.kcs".into(),
                account_id: "settlement-ke".into(),
                corridor_accounts: [("KES".to_string(), "settlement-ke-b".to_string())].into_iter().collect(),
                obp: Arc::new(crate::obp_client::ObpClient::new(obp_b_url, crate::obp_client::ObpAuth::None).unwrap()),
            },
        ];
        // The shared list, filled in.
        let forward_targets: ForwardTargets = Arc::new(std::sync::OnceLock::new());
        let _ = forward_targets.set(targets);
        router.with_forward_targets(forward_targets)
    }

    /// Bank C on OBP host B: an Open Corridor participant with BIC DEMCZZ22.
    fn banks_with_bank_c() -> serde_json::Value {
        serde_json::json!({"banks": [{
            "bank_id": "rt.bank.c4",
            "bank_routings": [{"scheme": "BIC", "address": "DEMCZZ22"}],
            "attributes": [{"name": "OPEN_CORRIDOR_PARTICIPANT", "value": "true"}]
        }]})
    }

    #[tokio::test]
    async fn hub_carries_a_payment_on_to_the_final_bank() {
        // OBP host B has Bank C.
        let r = hub_router(&obp_banks_stub(Some(banks_with_bank_c())).await).await;
        // The first leg arrives on OBP host A.
        let (body, id) = credit_through_hub();
        let reply = r.handle(message_id::CREDIT_NOTIFICATION, "corr-hub-1", body.as_bytes()).await;
        // Handled, as carried on.
        assert!(reply.is_ok(), "expected ok, got {:?}", reply.status);
        assert_eq!(reply.data["cbs_status"], "FORWARDED");
        assert_eq!(reply.data["forwarded_on_obp_host"], "test-host-b:8081");
        let second_leg_id = reply.data["forwarded_as_transaction_request_id"].as_str().unwrap().to_string();
        // Nothing was made on OBP host A.
        assert!(r.outbox.list("test-host-a:8080", "ke.01.kcs", 10).await.unwrap().is_empty());
        // The second leg is on OBP host B, from the hub's KES corridor account there.
        let rows = r.outbox.list("test-host-b:8081", "ke.01.kcs", 10).await.unwrap();
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].transaction_request_id, second_leg_id);
        assert_eq!(rows[0].account_id, "settlement-ke-b");
        // Paid to the final bank, to the same beneficiary, with the same value and originator, naming the first leg.
        let payload: serde_json::Value = serde_json::from_str(&rows[0].request_payload).unwrap();
        assert_eq!(payload["to"]["other_bank_routing_scheme"], "BIC");
        assert_eq!(payload["to"]["other_bank_routing_address"], "DEMCZZ22");
        assert_eq!(payload["to"]["other_account_routing_address"], "c-123");
        assert_eq!(payload["to"]["name"], "Cara Customer");
        assert_eq!(payload["value"]["amount"], "300.00");
        assert_eq!(payload["originator"]["name"], "Alice Sender");
        assert_eq!(payload["previous_leg"]["obp_host"], "test-host-a:8080");
        assert_eq!(payload["previous_leg"]["transaction_request_id"], id);
        assert!(payload.get("final_bank_routing").is_none(), "the second leg goes no further");
        // The evidence says where it went.
        let rec = r.evidence.get(&id).await.unwrap().unwrap();
        assert_eq!(rec.cbs_status.as_deref(), Some("FORWARDED"));
        assert_eq!(rec.forwarded_as_transaction_request_id.as_deref(), Some(second_leg_id.as_str()));
        assert_eq!(rec.forwarded_on_obp_host.as_deref(), Some("test-host-b:8081"));
        // A redelivery makes no second second leg.
        let again = r.handle(message_id::CREDIT_NOTIFICATION, "corr-hub-2", body.as_bytes()).await;
        assert_eq!(again.data["forwarded_as_transaction_request_id"], second_leg_id);
        assert_eq!(r.outbox.list("test-host-b:8081", "ke.01.kcs", 10).await.unwrap().len(), 1);
    }

    #[tokio::test]
    async fn hub_returns_a_payment_it_has_no_route_for() {
        // OBP host B does not have the final bank.
        let r = hub_router(&obp_banks_stub(Some(serde_json::json!({"banks": []}))).await).await;
        let (body, id) = credit_through_hub();
        let reply = r.handle(message_id::CREDIT_NOTIFICATION, "corr-hub-3", body.as_bytes()).await;
        // Handled, with a return.
        assert!(reply.is_ok(), "expected ok, got {:?}", reply.status);
        assert_eq!(reply.data["cbs_status"], "NOT_FORWARDED");
        // The return is on OBP host A, back to the originator.
        let rows = r.outbox.list("test-host-a:8080", "ke.01.kcs", 10).await.unwrap();
        assert_eq!(rows.len(), 1);
        let payload: serde_json::Value = serde_json::from_str(&rows[0].request_payload).unwrap();
        assert_eq!(payload["return_of"], id);
        // Nothing on OBP host B.
        assert!(r.outbox.list("test-host-b:8081", "ke.01.kcs", 10).await.unwrap().is_empty());
        let rec = r.evidence.get(&id).await.unwrap().unwrap();
        assert_eq!(rec.cbs_status.as_deref(), Some("NOT_FORWARDED"));
    }

    #[tokio::test]
    async fn hub_retries_when_it_cannot_look_the_final_bank_up() {
        // OBP host B's OBP-API fails.
        let r = hub_router(&obp_banks_stub(None).await).await;
        let (body, _id) = credit_through_hub();
        let reply = r.handle(message_id::CREDIT_NOTIFICATION, "corr-hub-4", body.as_bytes()).await;
        // Transient: OBP-API delivers it again later; no money moves yet.
        assert_eq!(reply.status.error_code, error_code::CBS_DELIVERY_FAILED);
        assert!(r.outbox.list("test-host-a:8080", "ke.01.kcs", 10).await.unwrap().is_empty());
        assert!(r.outbox.list("test-host-b:8081", "ke.01.kcs", 10).await.unwrap().is_empty());
    }

    #[tokio::test]
    async fn rejected_return_is_never_returned_again() {
        let url = cbs_stub_fixed(
            axum::http::StatusCode::UNPROCESSABLE_ENTITY,
            r#"{"status":"REJECTED","error":"originator account closed"}"#,
        )
        .await;
        let r = router_with_cbs(&url).await;
        let (body, id) = credit_returnable(Some("tr-original-0"));
        let reply = r
            .handle(message_id::CREDIT_NOTIFICATION, "corr-10", body.as_bytes())
            .await;
        // One hop: the refused RETURN parks STICKY at OBP-API, no ping-pong.
        assert_eq!(reply.status.error_code, error_code::CBS_REJECTED);
        assert!(
            r.outbox.list("test-host-a:8080", "ke.01.kcs", 10).await.unwrap().is_empty(),
            "must not create a return-of-return"
        );
        let rec = r.evidence.get(&id).await.unwrap().unwrap();
        assert_eq!(rec.cbs_status.as_deref(), Some("REJECTED"));
        assert_eq!(
            rec.return_of_transaction_request_id.as_deref(),
            Some("tr-original-0")
        );
        assert!(rec.returned_by_transaction_request_id.is_none());
    }

    #[tokio::test]
    async fn cbs_5xx_stays_a_transient_failure() {
        let url = cbs_stub_fixed(axum::http::StatusCode::SERVICE_UNAVAILABLE, "cbs down").await;
        let r = router_with_cbs(&url).await;
        let (body, id) = credit_with_valid_commitment();
        let reply = r
            .handle(message_id::CREDIT_NOTIFICATION, "corr-8", body.as_bytes())
            .await;
        assert_eq!(reply.status.error_code, error_code::CBS_DELIVERY_FAILED);
        let rec = r.evidence.get(&id).await.unwrap().unwrap();
        assert_eq!(rec.cbs_status.as_deref(), Some("FAILED"));
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
        assert_eq!(u.data["matched"], false, "no outbox row carries tr-1");
    }

    #[tokio::test]
    async fn status_update_is_stored_on_the_matching_outbox_row() {
        let r = router_with_cbs("http://127.0.0.1:1/credit").await;
        r.outbox
            .insert(crate::outbox::NewEntry {
                transaction_request_id: "local-1",
                // The OBP host the row belongs to: the router's own.
                obp_host: "test-host-a:8080",
                bank_id: "ke.01.kcs",
                account_id: "acct",
                request_payload: "{}",
                commitment_salt: "s",
            })
            .await
            .unwrap();
        r.outbox.mark_submitted("local-1", Some("obp-1")).await.unwrap();
        let u = r
            .handle(
                message_id::STATUS_UPDATE,
                "c",
                br#"{"transaction_request_id":"obp-1","status":"COMPLETED"}"#,
            )
            .await;
        assert!(u.is_ok());
        assert_eq!(u.data["matched"], true);
        let row = r.outbox.get("local-1").await.unwrap().unwrap();
        assert_eq!(row.obp_status.as_deref(), Some("COMPLETED"));
        assert_eq!(row.status, crate::outbox::status::SUBMITTED);
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

    #[tokio::test]
    async fn below_minimum_is_a_distinct_final_failure() {
        let calls = Arc::new(AtomicUsize::new(0));
        let backend = Arc::new(FlakyImpl {
            error: || {
                obp_blockchain::BlockchainError::BelowMinimum(
                    "net KES 20.00 is 940_000 lovelace; the minimum output is 1_000_000 lovelace (KES 21.22)".into(),
                )
            },
            fail_times: 1,
            calls: calls.clone(),
        });
        let (r, _) = router_with("http://127.0.0.1:1/credit", Some(backend)).await;
        let body = settlement_body("idem-min");

        let first = r
            .handle(message_id::SETTLEMENT_INSTRUCTION, "c1", body.as_bytes())
            .await;
        assert_eq!(first.status.error_code, error_code::SETTLEMENT_BELOW_MINIMUM);

        // Retrying cannot help (the amount is fixed), so redelivery repeats the
        // same answer and never calls the chain again.
        let second = r
            .handle(message_id::SETTLEMENT_INSTRUCTION, "c2", body.as_bytes())
            .await;
        assert_eq!(second.status.error_code, error_code::SETTLEMENT_BELOW_MINIMUM);
        assert_eq!(calls.load(Ordering::SeqCst), 1, "no second attempt");
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
