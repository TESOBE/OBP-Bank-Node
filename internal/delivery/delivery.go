// Copyright (C) 2026 TESOBE GmbH
// SPDX-License-Identifier: AGPL-3.0-or-later

// Package delivery sends inbound credit instructions to the bank's CBS.
//
// Section 3.3 of the spec defines four delivery modes. They are interchangeable
// behind the Delivery interface — the choice is per-deployment, set in
// obp-bank-node-config.yaml. New modes drop in by implementing Delivery and adding a case
// to NewFromConfig.
package delivery

import (
	"context"
	"fmt"

	"go.uber.org/zap"

	"github.com/OpenBankProject/OBP-Bank-Node/internal/config"
	"github.com/OpenBankProject/OBP-Bank-Node/pkg/models"
)

// Delivery is the abstract delivery mechanism. Implementations either return
// nil (the CBS has acknowledged receipt) or an error — the caller handles
// retries.
type Delivery interface {
	Deliver(ctx context.Context, credit *models.CreditInstruction) (cbsReference string, err error)
	Name() string
}

// Poller is implemented by delivery modes where the CBS acknowledges credits
// asynchronously via a side channel rather than in the Deliver response.
// Currently only the database mode (the CBS polls a staging table and writes
// PROCESSED + cbs_reference back into the row).
//
// main.go runs a background loop calling Poll on whichever deliverer
// implements this; each returned ProcessedCredit is forwarded to the outbox
// so the local record reflects the CBS-side ack.
type Poller interface {
	Poll(ctx context.Context) ([]ProcessedCredit, error)
}

// ProcessedCredit is what Poller returns: a credit the CBS has finished
// processing and is ready for the OBP Bank Node to mark delivered.
type ProcessedCredit struct {
	TransactionRequestID string
	CBSReference         string
}

// NewFromConfig picks the right implementation for the configured mode.
func NewFromConfig(cfg *config.CBSDelivery, secret string, log *zap.Logger) (Delivery, error) {
	switch cfg.Mode {
	case "webhook_obp":
		return NewWebhookOBP(cfg.Webhook.URL, secret, cfg.Webhook.TimeoutSeconds, log), nil
	case "webhook_iso20022":
		return NewWebhookISO20022(cfg.Webhook.URL, secret, cfg.Webhook.TimeoutSeconds, log), nil
	case "database":
		return NewDatabase(cfg.Database, log)
	case "file":
		return NewFileDrop(cfg.File, log), nil
	default:
		return nil, fmt.Errorf("unknown delivery mode %q", cfg.Mode)
	}
}
