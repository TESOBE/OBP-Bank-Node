// Copyright (C) 2026 TESOBE GmbH
// SPDX-License-Identifier: AGPL-3.0-or-later

// Package cardano writes the five Open Corridor record types to the Cardano blockchain via
// the Blockfrost API (Interface D).
//
// Cardano metadata transactions are how Open Corridor anchors the audit trail. Building
// them properly requires fee calculation, UTxO selection and signing with the
// bank's key — out of scope for the v0.1 skeleton. The interface here matches
// spec Section 16.5; the StubWriter logs the record and returns a fake tx hash
// so the rest of the system can be exercised end-to-end.
package cardano

import (
	"context"
	"crypto/sha256"
	"encoding/hex"
	"encoding/json"

	"go.uber.org/zap"

	"github.com/OpenBankProject/OBP-Bank-Node/pkg/models"
)

type Writer interface {
	WritePromise(ctx context.Context, p *models.PromiseRecord) (txHash string, err error)
	WriteSettlementReference(ctx context.Context, r *models.SettlementReference) (txHash string, err error)
	WriteException(ctx context.Context, e *models.ExceptionRecord) (txHash string, err error)
}

type StubWriter struct {
	log *zap.Logger
}

func NewStubWriter(log *zap.Logger) *StubWriter { return &StubWriter{log: log} }

func (s *StubWriter) WritePromise(ctx context.Context, p *models.PromiseRecord) (string, error) {
	hash := contentHash(p)
	s.log.Info("cardano: stub WritePromise",
		zap.String("transaction_request_id", p.TransactionRequestID),
		zap.String("tx_hash", hash))
	return hash, nil
}

func (s *StubWriter) WriteSettlementReference(ctx context.Context, r *models.SettlementReference) (string, error) {
	hash := contentHash(r)
	s.log.Info("cardano: stub WriteSettlementReference",
		zap.String("transaction_request_id", r.TransactionRequestID),
		zap.String("tx_hash", hash))
	return hash, nil
}

func (s *StubWriter) WriteException(ctx context.Context, e *models.ExceptionRecord) (string, error) {
	hash := contentHash(e)
	s.log.Warn("cardano: stub WriteException",
		zap.String("transaction_request_id", e.TransactionRequestID),
		zap.String("code", e.Code),
		zap.String("tx_hash", hash))
	return hash, nil
}

// contentHash gives the stub a deterministic tx-hash-shaped value derived from
// the record contents — useful in tests because the same record always produces
// the same hash.
func contentHash(v interface{}) string {
	b, _ := json.Marshal(v)
	sum := sha256.Sum256(b)
	return hex.EncodeToString(sum[:])
}
