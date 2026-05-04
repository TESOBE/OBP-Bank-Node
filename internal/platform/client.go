// Copyright (C) 2026 TESOBE GmbH
// SPDX-License-Identifier: AGPL-3.0-or-later

// Package platform is the Interface B client — outbound to the OBP API's
// OBP REST API.
//
// The interface here matches Section 16.5 of the spec verbatim. The default
// implementation is a stub that logs and returns plausible responses so the
// rest of the OBP Bank Node can run end-to-end without requiring a live OBP API — a
// real OAuth2 + http-backed implementation slots in here without touching any
// callers.
package platform

import (
	"context"
	"sync"
	"time"

	"github.com/google/uuid"
	"go.uber.org/zap"

	"github.com/OpenBankProject/OBP-Bank-Node/pkg/models"
)

// Client is the interface defined in spec Section 16.4.
type Client interface {
	CreateTransactionRequest(ctx context.Context, req *models.TransactionRequest) (*models.TransactionRequest, error)
	GetTransactionRequest(ctx context.Context, transactionRequestID string) (*models.TransactionRequest, error)
	ResolveRouting(ctx context.Context, to models.PaymentTo) (counterpartyID string, err error)
}

// StubClient is an in-memory implementation suitable for development and for
// running the OBP Bank Node before sandbox credentials are issued. It assigns IDs, marks
// transactions SUBMITTED, and resolves any non-empty routing to a deterministic
// counterparty ID derived from the address.
type StubClient struct {
	log *zap.Logger
	mu  sync.Mutex
	// In-memory copies — kept here only so the stub looks plausible to
	// integration tests. Production code should consult the outbox, not this.
	requests map[string]*models.TransactionRequest
}

func NewStubClient(log *zap.Logger) *StubClient {
	return &StubClient{
		log:      log,
		requests: make(map[string]*models.TransactionRequest),
	}
}

func (s *StubClient) CreateTransactionRequest(ctx context.Context, req *models.TransactionRequest) (*models.TransactionRequest, error) {
	s.mu.Lock()
	defer s.mu.Unlock()

	resp := *req
	if resp.TransactionRequestID == "" {
		resp.TransactionRequestID = uuid.NewString()
	}
	resp.Status = models.StatusSubmitted
	resp.StartDate = time.Now().UTC()
	s.requests[resp.TransactionRequestID] = &resp

	s.log.Info("platform: stub CreateTransactionRequest",
		zap.String("transaction_request_id", resp.TransactionRequestID),
		zap.String("from_bank", resp.From.BankID),
		zap.String("to_counterparty", resp.To.CounterpartyID),
		zap.String("currency", resp.Value.Currency),
		zap.String("amount", resp.Value.Amount))
	return &resp, nil
}

func (s *StubClient) GetTransactionRequest(ctx context.Context, transactionRequestID string) (*models.TransactionRequest, error) {
	s.mu.Lock()
	defer s.mu.Unlock()
	if tr, ok := s.requests[transactionRequestID]; ok {
		c := *tr
		return &c, nil
	}
	return nil, nil
}

// ResolveRouting maps the bank's chosen scheme/address pair to an OBP API
// counterparty ID. The stub deterministically derives a counterparty ID from the
// bank address — enough to exercise the south-side flow.
func (s *StubClient) ResolveRouting(ctx context.Context, to models.PaymentTo) (string, error) {
	if to.OtherBankRoutingAddress == "" || to.OtherAccountRoutingAddress == "" {
		return "", ErrUnresolvable
	}
	// Derive a stable, fake-looking counterparty ID. Real impl asks the OBP
	// API's resolution endpoint and caches the answer.
	return "cp-" + to.OtherBankRoutingScheme + "-" + to.OtherBankRoutingAddress + "-" + to.OtherAccountRoutingAddress, nil
}

// ErrUnresolvable is returned when routing details cannot be resolved to an
// Open Corridor participant. Mapped to OBP-BANK-NODE-ROUTING-001 by the API layer.
var ErrUnresolvable = &resolveErr{msg: "routing address could not be resolved"}

type resolveErr struct{ msg string }

func (e *resolveErr) Error() string { return e.msg }
