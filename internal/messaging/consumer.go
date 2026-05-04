// Copyright (C) 2026 TESOBE GmbH
// SPDX-License-Identifier: AGPL-3.0-or-later

// Package messaging is the Interface C client — RabbitMQ consumer of OBP API
// RPC requests (Section 5).
//
// Wire pattern follows OBP-API's existing RabbitMQ connector convention
// (`obp-api/.../rabbitmq/RabbitMQUtils.scala`):
//
//   - Single shared request queue, default `obp_rpc_queue` (configurable via
//     RabbitMQConfig.RequestQueue, matching OBP's `rabbitmq_connector.request_queue`)
//   - OBP-API publishes RPC requests with these AMQP properties set:
//     MessageId      — operation name (e.g. `obp_credit_notification`)
//     CorrelationId  — UUID for matching the reply
//     ReplyTo        — the per-request reply queue OBP-API created
//   - Dispatch is by MessageId, not routing key
//   - We publish a JSON inbound envelope to ReplyTo with the same CorrelationId:
//     {"inboundAdapterCallContext":{"correlationId":...},
//      "status":{"errorCode":"","backendMessages":[]},
//      "data":{...}}
//
// Operations recognised by the OBP Bank Node — defined in spec Section 5:
//
//	obp_credit_notification      → Handlers.OnCreditNotification
//	obp_netting_snapshot         → Handlers.OnNettingSnapshot
//	obp_settlement_instruction   → Handlers.OnSettlementInstruction
//	obp_status_update            → Handlers.OnStatusUpdate
//
// Per Section 11 (Outbox and Resilience), the OBP Bank Node keeps running even
// when RabbitMQ is unreachable: connect is retried in the background with
// exponential backoff; Connected() reports live state for /health.
package messaging

import (
	"context"
	"encoding/json"
	"errors"
	"fmt"
	"net/url"
	"sync/atomic"
	"time"

	amqp "github.com/rabbitmq/amqp091-go"
	"go.uber.org/zap"

	"github.com/OpenBankProject/OBP-Bank-Node/internal/config"
	"github.com/OpenBankProject/OBP-Bank-Node/pkg/models"
)

// Consumer is the abstract RabbitMQ consumer interface. The real
// implementation is RabbitMQConsumer; tests can substitute a fake.
type Consumer interface {
	Start(ctx context.Context, h Handlers) error
	Connected() bool
	Close() error
}

// Handlers map a Section 5 message type to the action the OBP Bank Node should
// take when one arrives. nil handlers are tolerated (the message is acked and
// dropped).
type Handlers struct {
	OnCreditNotification    func(ctx context.Context, c *models.CreditInstruction) error
	OnNettingSnapshot       func(ctx context.Context, snap *models.NettingSnapshot) error
	OnSettlementInstruction func(ctx context.Context, instr *models.SettlementInstruction) error
	OnStatusUpdate          func(ctx context.Context, upd *models.StatusUpdate) error
}

// RabbitMQConsumer is the production AMQP consumer. Use NewRabbitMQConsumer.
type RabbitMQConsumer struct {
	cfg       config.RabbitMQConfig
	log       *zap.Logger
	connected atomic.Bool
	stop      chan struct{}
	stopped   chan struct{}
}

func NewRabbitMQConsumer(cfg config.RabbitMQConfig, log *zap.Logger) *RabbitMQConsumer {
	return &RabbitMQConsumer{
		cfg:     cfg,
		log:     log,
		stop:    make(chan struct{}),
		stopped: make(chan struct{}),
	}
}

func (c *RabbitMQConsumer) Connected() bool { return c.connected.Load() }

// Start kicks off the connect-and-consume loop in a background goroutine and
// returns immediately. Start itself never errors — a missing broker doesn't
// prevent the bank node from booting; the loop logs a warning and keeps
// retrying.
func (c *RabbitMQConsumer) Start(ctx context.Context, h Handlers) error {
	go c.runForever(ctx, h)
	return nil
}

func (c *RabbitMQConsumer) Close() error {
	select {
	case <-c.stop:
		// already closed
	default:
		close(c.stop)
	}
	// Wait for the background loop to exit so callers can rely on no further
	// log lines / handler invocations after Close returns.
	<-c.stopped
	return nil
}

// runForever is the connect → consume → reconnect outer loop. Exits only when
// the context is cancelled or Close is called.
func (c *RabbitMQConsumer) runForever(ctx context.Context, h Handlers) {
	defer close(c.stopped)

	const maxBackoff = 30 * time.Second
	backoff := time.Second

	for {
		select {
		case <-ctx.Done():
			return
		case <-c.stop:
			return
		default:
		}

		err := c.consumeOnce(ctx, h)
		c.connected.Store(false)

		if err == nil {
			// consumeOnce returned nil = ctx cancelled / Close called mid-flow
			return
		}

		c.log.Warn("RabbitMQ consume failed; will retry",
			zap.Error(err),
			zap.Duration("retry_in", backoff),
			zap.String("queue", c.cfg.RequestQueue))

		select {
		case <-ctx.Done():
			return
		case <-c.stop:
			return
		case <-time.After(backoff):
		}
		backoff *= 2
		if backoff > maxBackoff {
			backoff = maxBackoff
		}
	}
}

// consumeOnce dials the broker and consumes until the connection closes or the
// context is cancelled. Returns nil on graceful stop, error otherwise.
func (c *RabbitMQConsumer) consumeOnce(ctx context.Context, h Handlers) error {
	conn, err := amqp.Dial(c.amqpURL())
	if err != nil {
		return fmt.Errorf("dial: %w", err)
	}
	defer conn.Close()

	ch, err := conn.Channel()
	if err != nil {
		return fmt.Errorf("open channel: %w", err)
	}
	defer ch.Close()

	// Ensure the queue exists. In production the OBP API operator provisions
	// it at registration; QueueDeclare is idempotent against that (matching
	// params = no-op) and also makes local dev work without manual queue
	// setup. Params: durable (survives broker restart), not autoDelete, not
	// exclusive — these match how OBP-API declares it (see RabbitMQUtils.scala).
	if _, err := ch.QueueDeclare(c.cfg.RequestQueue, true, false, false, false, nil); err != nil {
		return fmt.Errorf("declare queue %s: %w", c.cfg.RequestQueue, err)
	}

	deliveries, err := ch.Consume(
		c.cfg.RequestQueue,
		"",    // consumer tag — broker-assigned
		false, // autoAck=false; we Ack/Nack explicitly per handler outcome
		false, // exclusive
		false, // noLocal
		false, // noWait
		nil,   // args
	)
	if err != nil {
		return fmt.Errorf("consume %s: %w", c.cfg.RequestQueue, err)
	}

	c.connected.Store(true)
	c.log.Info("RabbitMQ connected",
		zap.String("host", c.cfg.Host),
		zap.Int("port", c.cfg.Port),
		zap.String("queue", c.cfg.RequestQueue))

	closeNotify := conn.NotifyClose(make(chan *amqp.Error, 1))

	for {
		select {
		case <-ctx.Done():
			return nil
		case <-c.stop:
			return nil

		case amqpErr := <-closeNotify:
			if amqpErr != nil {
				return fmt.Errorf("connection closed: %w", amqpErr)
			}
			return errors.New("connection closed")

		case d, ok := <-deliveries:
			if !ok {
				return errors.New("delivery channel closed")
			}
			c.handle(ctx, ch, h, d)
		}
	}
}

// handle dispatches a single delivery to the right handler, publishes the OBP
// inbound-envelope reply to d.ReplyTo, and Acks. Unknown messageIds reply with
// a "not implemented" status so the upstream OBP-API doesn't time out.
//
// We always Ack — never requeue. A transient downstream failure (e.g. the
// CBS webhook briefly down) is reported back via the reply envelope's
// errorCode; OBP-API decides whether to retry. Requeueing here would create a
// poison-message tight-loop because the same consumer would just receive it
// again immediately.
func (c *RabbitMQConsumer) handle(ctx context.Context, ch *amqp.Channel, h Handlers, d amqp.Delivery) {
	data, handlerErr := c.dispatch(ctx, h, d.MessageId, d.Body)
	if handlerErr != nil {
		c.log.Error("handler failed; replying with errorCode",
			zap.String("message_id", d.MessageId),
			zap.String("correlation_id", d.CorrelationId),
			zap.Error(handlerErr))
	}
	if err := c.publishReply(ch, d, data, handlerErr); err != nil {
		c.log.Error("publish reply failed",
			zap.String("message_id", d.MessageId),
			zap.String("reply_to", d.ReplyTo),
			zap.Error(err))
	}
	_ = d.Ack(false)
}

// dispatch decodes the body to the typed message and invokes the handler.
// Returns (responseData, error). responseData is currently always nil — the
// reply envelope's data field is empty on success — but the signature leaves
// room for handlers to return a richer payload when needed.
func (c *RabbitMQConsumer) dispatch(ctx context.Context, h Handlers, messageID string, body []byte) (any, error) {
	switch messageID {
	case "obp_credit_notification":
		if h.OnCreditNotification == nil {
			return nil, errNotImplemented
		}
		var msg models.CreditInstruction
		if err := json.Unmarshal(body, &msg); err != nil {
			return nil, fmt.Errorf("decode credit notification: %w", err)
		}
		return nil, h.OnCreditNotification(ctx, &msg)

	case "obp_netting_snapshot":
		if h.OnNettingSnapshot == nil {
			return nil, errNotImplemented
		}
		var msg models.NettingSnapshot
		if err := json.Unmarshal(body, &msg); err != nil {
			return nil, fmt.Errorf("decode netting snapshot: %w", err)
		}
		return nil, h.OnNettingSnapshot(ctx, &msg)

	case "obp_settlement_instruction":
		if h.OnSettlementInstruction == nil {
			return nil, errNotImplemented
		}
		var msg models.SettlementInstruction
		if err := json.Unmarshal(body, &msg); err != nil {
			return nil, fmt.Errorf("decode settlement instruction: %w", err)
		}
		return nil, h.OnSettlementInstruction(ctx, &msg)

	case "obp_status_update":
		if h.OnStatusUpdate == nil {
			return nil, errNotImplemented
		}
		var msg models.StatusUpdate
		if err := json.Unmarshal(body, &msg); err != nil {
			return nil, fmt.Errorf("decode status update: %w", err)
		}
		return nil, h.OnStatusUpdate(ctx, &msg)

	default:
		c.log.Warn("unknown messageId; replying not-implemented",
			zap.String("message_id", messageID))
		return nil, errNotImplemented
	}
}

// errNotImplemented is the sentinel for "this messageId isn't handled by the
// OBP Bank Node." It surfaces in the reply envelope as a specific errorCode so
// callers can distinguish "missing implementation" from genuine handler errors.
var errNotImplemented = errors.New("OBP-BANK-NODE-NOT-IMPLEMENTED")

// publishReply sends the OBP-shaped inbound envelope to the per-request reply
// queue identified by d.ReplyTo, carrying the same correlationId. If the
// upstream didn't set ReplyTo it's a fire-and-forget message and we skip.
func (c *RabbitMQConsumer) publishReply(ch *amqp.Channel, d amqp.Delivery, data any, handlerErr error) error {
	if d.ReplyTo == "" {
		return nil
	}
	if data == nil {
		// JSON empty object reads better in OBP-API logs than `null`.
		data = struct{}{}
	}

	errorCode := ""
	if handlerErr != nil {
		errorCode = handlerErr.Error()
	}

	envelope := map[string]any{
		"inboundAdapterCallContext": map[string]any{
			"correlationId": d.CorrelationId,
		},
		"status": map[string]any{
			"errorCode":       errorCode,
			"backendMessages": []any{},
		},
		"data": data,
	}
	body, err := json.Marshal(envelope)
	if err != nil {
		return fmt.Errorf("encode reply: %w", err)
	}

	// Publish on the default exchange — routing key = queue name = ReplyTo.
	return ch.PublishWithContext(
		context.Background(),
		"",        // default exchange
		d.ReplyTo, // routing key (= queue name on default exchange)
		false,     // mandatory
		false,     // immediate
		amqp.Publishing{
			ContentType:   "application/json",
			CorrelationId: d.CorrelationId,
			Body:          body,
		},
	)
}

// amqpURL builds the connection URL with credentials URL-escaped so special
// characters in the broker password don't break the dial.
func (c *RabbitMQConsumer) amqpURL() string {
	user := url.QueryEscape(c.cfg.Username)
	pass := url.QueryEscape(c.cfg.Password)
	vhost := c.cfg.VirtualHost
	if vhost != "" && vhost[0] != '/' {
		vhost = "/" + vhost
	}
	return fmt.Sprintf("amqp://%s:%s@%s:%d%s", user, pass, c.cfg.Host, c.cfg.Port, vhost)
}
