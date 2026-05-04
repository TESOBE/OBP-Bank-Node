// Copyright (C) 2026 TESOBE GmbH
// SPDX-License-Identifier: AGPL-3.0-or-later

// Package outbox provides a SQLite-backed durable store for transaction
// requests, credit instructions, and queued outbound messages.
//
// Section 11 of the spec mandates that:
//   - every payment instruction received on Interface A1 is persisted before
//     ack, so a 202 to the bank is honoured even if the OBP API is down
//   - the outbox survives container restarts (it lives on a Docker volume)
//   - entries are retained for 90 days
//
// We intentionally keep the schema minimal — one table per concern, one row per
// instance. Replay logic and 90-day retention are wired in main.go.
package outbox

import (
	"context"
	"database/sql"
	"encoding/json"
	"fmt"
	"time"

	_ "modernc.org/sqlite"

	"github.com/OpenBankProject/OBP-Bank-Node/pkg/models"
)

type Outbox struct {
	db *sql.DB
}

const schema = `
CREATE TABLE IF NOT EXISTS transaction_requests (
    transaction_request_id TEXT PRIMARY KEY,
    body                   TEXT NOT NULL,
    status                 TEXT NOT NULL,
    submitted_to_platform  INTEGER NOT NULL DEFAULT 0,
    promise_written        INTEGER NOT NULL DEFAULT 0,
    created_at             TIMESTAMP NOT NULL,
    updated_at             TIMESTAMP NOT NULL
);
CREATE INDEX IF NOT EXISTS idx_tr_status ON transaction_requests(status);

CREATE TABLE IF NOT EXISTS credit_instructions (
    transaction_request_id TEXT PRIMARY KEY,
    body                   TEXT NOT NULL,
    delivery_status        TEXT NOT NULL,    -- PENDING / DELIVERED / FAILED
    attempts               INTEGER NOT NULL DEFAULT 0,
    last_attempt_at        TIMESTAMP,
    next_attempt_at        TIMESTAMP,
    cbs_reference          TEXT,
    created_at             TIMESTAMP NOT NULL,
    updated_at             TIMESTAMP NOT NULL
);

CREATE TABLE IF NOT EXISTS routing_cache (
    cache_key       TEXT PRIMARY KEY,
    counterparty_id TEXT NOT NULL,
    bank_id         TEXT NOT NULL,
    account_id      TEXT NOT NULL,
    cached_at       TIMESTAMP NOT NULL
);
`

func Open(path string) (*Outbox, error) {
	db, err := sql.Open("sqlite", path)
	if err != nil {
		return nil, fmt.Errorf("open sqlite: %w", err)
	}
	if _, err := db.Exec(schema); err != nil {
		_ = db.Close()
		return nil, fmt.Errorf("apply schema: %w", err)
	}
	return &Outbox{db: db}, nil
}

func (o *Outbox) Close() error { return o.db.Close() }

// SaveTransactionRequest persists a TR before the bank receives a 202. If the
// process dies between this call and submission to the OBP API the replay
// loop picks it up on next startup.
func (o *Outbox) SaveTransactionRequest(ctx context.Context, tr *models.TransactionRequest) error {
	body, err := json.Marshal(tr)
	if err != nil {
		return err
	}
	now := time.Now().UTC()
	_, err = o.db.ExecContext(ctx,
		`INSERT INTO transaction_requests (transaction_request_id, body, status, created_at, updated_at)
		 VALUES (?, ?, ?, ?, ?)
		 ON CONFLICT(transaction_request_id) DO UPDATE SET body=excluded.body, status=excluded.status, updated_at=excluded.updated_at`,
		tr.TransactionRequestID, string(body), tr.Status, now, now)
	return err
}

func (o *Outbox) GetTransactionRequest(ctx context.Context, transactionRequestID string) (*models.TransactionRequest, error) {
	var body string
	err := o.db.QueryRowContext(ctx,
		`SELECT body FROM transaction_requests WHERE transaction_request_id = ?`, transactionRequestID).Scan(&body)
	if err == sql.ErrNoRows {
		return nil, nil
	}
	if err != nil {
		return nil, err
	}
	var tr models.TransactionRequest
	if err := json.Unmarshal([]byte(body), &tr); err != nil {
		return nil, err
	}
	return &tr, nil
}

func (o *Outbox) ListTransactionRequests(ctx context.Context, limit int) ([]*models.TransactionRequest, error) {
	rows, err := o.db.QueryContext(ctx,
		`SELECT body FROM transaction_requests ORDER BY created_at DESC LIMIT ?`, limit)
	if err != nil {
		return nil, err
	}
	defer rows.Close()
	var out []*models.TransactionRequest
	for rows.Next() {
		var body string
		if err := rows.Scan(&body); err != nil {
			return nil, err
		}
		var tr models.TransactionRequest
		if err := json.Unmarshal([]byte(body), &tr); err != nil {
			return nil, err
		}
		out = append(out, &tr)
	}
	return out, rows.Err()
}

// UpdateStatus changes only the status / settlement fields without rewriting the
// whole row. Callers that need to mutate other fields should re-Save.
func (o *Outbox) UpdateStatus(ctx context.Context, transactionRequestID, status string) error {
	tr, err := o.GetTransactionRequest(ctx, transactionRequestID)
	if err != nil {
		return err
	}
	if tr == nil {
		return fmt.Errorf("transaction request %s not found", transactionRequestID)
	}
	tr.Status = status
	return o.SaveTransactionRequest(ctx, tr)
}

// SaveCredit stores an inbound credit instruction in PENDING state, ready for the
// delivery loop to pick up. Idempotent on transaction_request_id.
func (o *Outbox) SaveCredit(ctx context.Context, c *models.CreditInstruction) error {
	body, err := json.Marshal(c)
	if err != nil {
		return err
	}
	now := time.Now().UTC()
	_, err = o.db.ExecContext(ctx,
		`INSERT INTO credit_instructions (transaction_request_id, body, delivery_status, created_at, updated_at)
		 VALUES (?, ?, 'PENDING', ?, ?)
		 ON CONFLICT(transaction_request_id) DO NOTHING`,
		c.TransactionRequestID, string(body), now, now)
	return err
}

func (o *Outbox) MarkCreditDelivered(ctx context.Context, transactionRequestID, cbsReference string) error {
	now := time.Now().UTC()
	_, err := o.db.ExecContext(ctx,
		`UPDATE credit_instructions SET delivery_status='DELIVERED', cbs_reference=?, updated_at=? WHERE transaction_request_id=?`,
		cbsReference, now, transactionRequestID)
	return err
}
