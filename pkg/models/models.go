// Copyright (C) 2026 TESOBE GmbH
// SPDX-License-Identifier: AGPL-3.0-or-later

// Package models contains the shared data types used across the OBP Bank Node.
//
// The shapes mirror the OBP API where applicable (Transaction Request)
// so that the south-side REST surface is byte-identical to OBP for any field a bank
// already understands. Fields specific to the OBP API / Cardano are added as
// distinct top-level fields rather than nested under an extension block.
package models

import "time"

// Value is the OBP value object — currency + decimal amount expressed as a string.
type Value struct {
	Currency string `json:"currency"`
	Amount   string `json:"amount"`
}

// Routing identifies a bank or account in a particular scheme.
type Routing struct {
	Scheme  string `json:"scheme"`
	Address string `json:"address"`
}

// PaymentTo is the body of `to` in an Interface A1 request.
//
// The four `other*` fields match OBP SIMPLE Transaction Request naming exactly
// so banks can copy their existing OBP request bodies unchanged.
type PaymentTo struct {
	OtherBankRoutingScheme     string `json:"otherBankRoutingScheme"`
	OtherBankRoutingAddress    string `json:"otherBankRoutingAddress"`
	OtherAccountRoutingScheme  string `json:"otherAccountRoutingScheme"`
	OtherAccountRoutingAddress string `json:"otherAccountRoutingAddress"`
}

// PaymentRequest is the Interface A1 request body (Section 3.2).
type PaymentRequest struct {
	Value        Value     `json:"value"`
	Description  string    `json:"description"`
	To           PaymentTo `json:"to"`
	ChargePolicy string    `json:"charge_policy,omitempty"`
}

// TransactionRequest is the canonical internal representation of a payment as it
// moves through the OBP Bank Node's lifecycle. It is also returned to the bank in the
// Interface A1 response.
type TransactionRequest struct {
	TransactionRequestID string    `json:"transaction_request_id"`
	Type                 string    `json:"type"`
	From                FromRef    `json:"from"`
	To                  ToRef      `json:"to"`
	Value               Value      `json:"value"`
	Description         string     `json:"description"`
	Status              string     `json:"status"`
	PromiseID           *string    `json:"promise_id"`
	PromiseBlockchain   *string    `json:"promise_blockchain"`
	StartDate           time.Time  `json:"start_date"`
	EndDate             *time.Time `json:"end_date"`
	Challenge           *string    `json:"challenge"`
	ChargePolicy        string     `json:"charge_policy,omitempty"`

	// Internal-only — not serialised to the bank's response by default.
	NettingSnapshotID *string `json:"netting_snapshot_id,omitempty"`
	NettingBlockchain *string `json:"netting_blockchain,omitempty"`
	SettlementID      *string `json:"settlement_id,omitempty"`
	SettlementSystem  *string `json:"settlement_system,omitempty"`
	SettledAt         *time.Time `json:"settled_at,omitempty"`
}

// FromRef identifies the originating bank account.
type FromRef struct {
	BankID    string `json:"bank_id"`
	AccountID string `json:"account_id"`
}

// ToRef identifies the destination — at the OBP Bank Node's level of abstraction this is
// just the OBP API counterparty ID; the original routing is preserved in the
// Promise record.
type ToRef struct {
	CounterpartyID string `json:"counterparty_id"`
}

// Status values — see Section 8.
const (
	StatusInitiated         = "INITIATED"
	StatusSubmitted         = "SUBMITTED"
	StatusPromiseWritten    = "PROMISE_WRITTEN"
	StatusPendingNetting    = "PENDING_NETTING"
	StatusPendingSettlement = "PENDING_SETTLEMENT"
	StatusCompleted         = "COMPLETED"
	StatusException         = "EXCEPTION"
)

// CreditInstruction is the Interface A2 payload — a credit arriving from the
// OBP API that needs to be delivered to the bank's CBS in one of four modes.
type CreditInstruction struct {
	TransactionRequestID string         `json:"transaction_request_id"`
	NettingSnapshotID    string         `json:"netting_snapshot_id"`
	NettingBlockchain    string         `json:"netting_blockchain"`
	Type                 string         `json:"type"`
	From                 CreditFrom     `json:"from"`
	To                   CreditTo       `json:"to"`
	Value                Value          `json:"value"`
	Description          string         `json:"description"`
	ValueDate            string         `json:"value_date"`
	ChargePolicy         string         `json:"charge_policy"`
	PromiseID            string         `json:"promise_id"`
	PromiseBlockchain    string         `json:"promise_blockchain"`
}

type CreditFrom struct {
	BankID      string  `json:"bank_id"`
	BankRouting Routing `json:"bank_routing"`
}

type CreditTo struct {
	BankID         string  `json:"bank_id"`
	AccountID      string  `json:"account_id"`
	AccountRouting Routing `json:"account_routing"`
}

// PromiseRecord is the on-chain Record 1 — written by the OBP Bank Node after a payment is
// accepted by the OBP API.
type PromiseRecord struct {
	TransactionRequestID string    `json:"transaction_request_id"`
	From                 FromRef   `json:"from"`
	BeneficiaryRouting   PaymentTo `json:"beneficiary_routing"`
	Value                Value     `json:"value"`
	DescriptionHash      string    `json:"description_hash"`
	Timestamp            time.Time `json:"timestamp"`
}

// SettlementReference is on-chain Record 5.
type SettlementReference struct {
	TransactionRequestID string    `json:"transaction_request_id"`
	NettingSnapshotID    string    `json:"netting_snapshot_id"`
	SettlementID         string    `json:"settlement_id"`
	SettlementSystem     string    `json:"settlement_system"`
	Timestamp            time.Time `json:"timestamp"`
}

// ExceptionRecord is on-chain Record 4.
type ExceptionRecord struct {
	TransactionRequestID string    `json:"transaction_request_id"`
	Code                 string    `json:"code"`
	Detail               string    `json:"detail"`
	Timestamp            time.Time `json:"timestamp"`
}

// NettingSnapshot — the body of an `obp.netting.snapshot` message on
// Interface C. Published by the OBP API when a netting cycle closes.
type NettingSnapshot struct {
	NettingSnapshotID string    `json:"netting_snapshot_id"`
	NettingBlockchain string    `json:"netting_blockchain"`
	Currencies        []string  `json:"currencies,omitempty"`
	PublishedAt       time.Time `json:"published_at"`
}

// SettlementInstruction — the body of an `obp.settlement.instruction`
// message on Interface C. Tells the bank to settle a netting snapshot via
// a particular settlement system (Cardano, CHAPS, NIBSS, …).
//
// Value carries the currency-and-amount in whatever the settlement system
// uses — `"ADA"` for Cardano bearer settlement, an ISO 4217 code for fiat
// rails like CHAPS or NIBSS. DestinationAddress is whatever the settlement
// system needs to identify the receiver (Cardano wallet address, SWIFT BIC,
// NIBSS bank code, etc.).
type SettlementInstruction struct {
	NettingSnapshotID  string    `json:"netting_snapshot_id"`
	SettlementSystem   string    `json:"settlement_system"`
	Value              Value     `json:"value"`
	DestinationAddress string    `json:"destination_address,omitempty"`
	InstructedAt       time.Time `json:"instructed_at"`
}

// StatusUpdate — the body of an `obp.status.update` message on Interface C.
type StatusUpdate struct {
	TransactionRequestID string    `json:"transaction_request_id"`
	Status               string    `json:"status"`
	UpdatedAt            time.Time `json:"updated_at"`
}

// APIError is the canonical error body returned by the south-side REST surface.
// Codes prefixed `OBP-` mirror OBP's own error codes; codes prefixed `OBP Bank Node-` are
// OBP Bank Node-specific (Section 3.2 error table).
type APIError struct {
	Error       string `json:"error"`
	Message     string `json:"message"`
	OBPEndpoint string `json:"obp_endpoint,omitempty"`
}
