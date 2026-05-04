// Copyright (C) 2026 TESOBE GmbH
// SPDX-License-Identifier: AGPL-3.0-or-later

package delivery

import (
	"bytes"
	"context"
	"encoding/json"
	"fmt"
	"io"
	"net/http"
	"time"

	"go.uber.org/zap"

	"github.com/OpenBankProject/OBP-Bank-Node/pkg/models"
)

// WebhookISO20022 — Section 3.3 Mode 2.
// Posts a JSON payload modelled on camt.054 with an OBP Bank Node extension block. We
// build the payload by hand; the OBP Bank Node does not depend on a full ISO 20022
// library.
type WebhookISO20022 struct {
	url    string
	secret string
	client *http.Client
	log    *zap.Logger
}

func NewWebhookISO20022(url, secret string, timeoutSec int, log *zap.Logger) *WebhookISO20022 {
	return &WebhookISO20022{
		url:    url,
		secret: secret,
		client: &http.Client{Timeout: time.Duration(timeoutSec) * time.Second},
		log:    log,
	}
}

func (w *WebhookISO20022) Name() string { return "webhook_iso20022" }

func (w *WebhookISO20022) Deliver(ctx context.Context, credit *models.CreditInstruction) (string, error) {
	payload := buildISO20022(credit)
	body, err := json.Marshal(payload)
	if err != nil {
		return "", err
	}
	req, err := http.NewRequestWithContext(ctx, http.MethodPost, w.url, bytes.NewReader(body))
	if err != nil {
		return "", err
	}
	req.Header.Set("Content-Type", "application/json")
	req.Header.Set("Authorization", "Bearer "+w.secret)

	resp, err := w.client.Do(req)
	if err != nil {
		return "", fmt.Errorf("iso20022 webhook POST: %w", err)
	}
	defer resp.Body.Close()

	respBody, _ := io.ReadAll(resp.Body)
	if resp.StatusCode != http.StatusOK {
		return "", fmt.Errorf("CBS webhook returned %d: %s", resp.StatusCode, string(respBody))
	}
	var ack cbsAck
	_ = json.Unmarshal(respBody, &ack)
	return ack.CBSReference, nil
}

// buildISO20022 produces a camt.054-style JSON shape (Section 3.3 Mode 2).
//
// ISO 20022 uses a fixed abbreviation vocabulary — the names below are dictated
// by the standard, not chosen by us. Glossary of the abbreviations used in this
// file (all are XML element names defined in the camt.054 schema):
//
//	BkToCstmrDbtCdtNtfctn  Bank-to-Customer Debit/Credit Notification — the camt.054 root
//	GrpHdr                 Group Header (envelope-level metadata)
//	MsgId                  Message Identifier (sender-assigned; we use the OBP Bank Node transaction_request_id)
//	CreDtTm                Creation Date/Time
//	Ntfctn                 Notification (the actual credit notice payload)
//	Acct                   Account (the beneficiary's account at this bank)
//	Id                     Identifier (re-used at every nesting level)
//	Othr                   Other — a non-standard ID scheme, used when the value isn't an IBAN/BBAN/etc.
//	SchmeNm                Scheme Name — labels which scheme the Othr.Id is in (OBP, BIC, MOBILE_PHONE, …)
//	Ntry                   Entry — one booking line against the account
//	Amt                    Amount; Ccy = Currency (ISO 4217)
//	CdtDbtInd              Credit/Debit Indicator — "CRDT" for credit, "DBIT" for debit
//	Sts                    Status — "BOOK" means booked (posted to the account)
//	BookgDt                Booking Date (when the bank posts it)
//	ValDt                  Value Date (when funds become available)
//	AddtlNtryInf           Additional Entry Information — free-text reference / description
//	RltdPties              Related Parties — the people / institutions involved in this entry
//	Dbtr                   Debtor — the party being debited (here: the originating bank)
//	Agt                    Agent — a financial institution acting on behalf of a party
//	FinInstnId             Financial Institution Identification — the bank's identifying details
//
// Reference: https://www.iso20022.org/iso-20022-message-definitions
func buildISO20022(credit *models.CreditInstruction) map[string]any {
	return map[string]any{
		"BkToCstmrDbtCdtNtfctn": map[string]any{
			// Envelope metadata — who sent this and when.
			"GrpHdr": map[string]any{
				"MsgId":   credit.TransactionRequestID,
				"CreDtTm": time.Now().UTC().Format(time.RFC3339),
			},
			// The notification body — one credit destined for one account.
			"Ntfctn": map[string]any{
				"Id": credit.TransactionRequestID,
				// Beneficiary account, expressed in the chosen routing scheme
				// (OBP, BIC+ACCOUNT_NUMBER, MOBILE_PHONE, …) via Othr.
				"Acct": map[string]any{
					"Id": map[string]any{
						"Othr": map[string]any{
							"Id":      credit.To.AccountRouting.Address,
							"SchmeNm": credit.To.AccountRouting.Scheme,
						},
					},
				},
				// The booking entry: amount, sides, dates, references.
				"Ntry": map[string]any{
					"Amt": map[string]any{
						"value": credit.Value.Amount,
						"Ccy":   credit.Value.Currency,
					},
					"CdtDbtInd":    "CRDT", // always a credit on Interface A2
					"Sts":          "BOOK", // booked — the OBP API has confirmed settlement
					"BookgDt":      credit.ValueDate,
					"ValDt":        credit.ValueDate,
					"AddtlNtryInf": credit.Description,
					// Originating bank identification — wrapped in Dbtr → Agt
					// → FinInstnId per the standard's institution-as-agent
					// pattern.
					"RltdPties": map[string]any{
						"Dbtr": map[string]any{
							"Agt": map[string]any{
								"FinInstnId": map[string]any{
									"Othr": map[string]any{
										"Id":      credit.From.BankRouting.Address,
										"SchmeNm": credit.From.BankRouting.Scheme,
									},
								},
							},
						},
					},
				},
			},
		},
		// Non-standard supplementary block — ISO 20022 allows arbitrary
		// extensions outside the schema-validated tree. We carry the
		// OBP Bank Node-specific identifiers (Cardano Promise hash, netting snapshot,
		// etc.) here so the bank's CBS can reconcile if it wants to.
		"obp_bank_node": map[string]any{
			"netting_snapshot_id": credit.NettingSnapshotID,
			"netting_blockchain":  credit.NettingBlockchain,
			"promise_id":          credit.PromiseID,
			"promise_blockchain":  credit.PromiseBlockchain,
		},
	}
}
