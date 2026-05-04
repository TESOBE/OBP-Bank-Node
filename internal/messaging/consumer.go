// Copyright (C) 2026 TESOBE GmbH
// SPDX-License-Identifier: AGPL-3.0-or-later

// Package messaging is the Interface C client — RabbitMQ consumer of OBP API
// instructions (Section 5).
//
// The four consumed message types are listed in the spec table; for each we
// invoke the corresponding handler. v0.1 ships only a stub consumer that
// fabricates no messages — it merely logs that the consumer started. A real
// amqp091-go implementation slots in here.
package messaging

import (
	"context"
	"sync"

	"go.uber.org/zap"

	"github.com/OpenBankProject/OBP-Bank-Node/pkg/models"
)

// Handlers are invoked by the consumer when a message of the matching type is
// received from the OBP API.
type Handlers struct {
	OnCreditNotification    func(ctx context.Context, c *models.CreditInstruction) error
	OnNettingSnapshot       func(ctx context.Context, snapshotID string) error
	OnSettlementInstruction func(ctx context.Context, transactionRequestID string) error
	OnStatusUpdate          func(ctx context.Context, transactionRequestID, newStatus string) error
}

// Consumer is the interface a real broker client implements; the StubConsumer
// in this package is the no-op that ships with the skeleton.
type Consumer interface {
	Start(ctx context.Context, h Handlers) error
	Close() error
}

type StubConsumer struct {
	log     *zap.Logger
	queue   string
	stopped chan struct{}
	once    sync.Once
}

func NewStubConsumer(log *zap.Logger, queue string) *StubConsumer {
	return &StubConsumer{log: log, queue: queue, stopped: make(chan struct{})}
}

func (c *StubConsumer) Start(ctx context.Context, h Handlers) error {
	c.log.Info("messaging: stub consumer started (no real RabbitMQ connection)",
		zap.String("queue", c.queue))
	go func() {
		<-ctx.Done()
		c.once.Do(func() { close(c.stopped) })
	}()
	return nil
}

func (c *StubConsumer) Close() error {
	c.once.Do(func() { close(c.stopped) })
	return nil
}

// InjectCredit lets tests and the dashboard exercise the inbound credit path
// without a real broker.
func (c *StubConsumer) InjectCredit(ctx context.Context, h Handlers, credit *models.CreditInstruction) error {
	if h.OnCreditNotification == nil {
		return nil
	}
	return h.OnCreditNotification(ctx, credit)
}
