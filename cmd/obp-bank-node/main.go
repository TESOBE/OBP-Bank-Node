// Copyright (C) 2026 TESOBE GmbH
// SPDX-License-Identifier: AGPL-3.0-or-later

// Command obp-bank-node is the OBP Bank Node entry point.
//
// Wires together: config, telemetry, outbox, OBP API client, Cardano
// writer, RabbitMQ consumer, CBS delivery, and the south-side REST API. Each
// subsystem is constructed in main and injected — package code never reaches
// out for globals.
package main

import (
	"context"
	"errors"
	"flag"
	"fmt"
	"net/http"
	"os"
	"os/signal"
	"path/filepath"
	"syscall"
	"time"

	"github.com/prometheus/client_golang/prometheus"
	"go.uber.org/zap"

	"github.com/OpenBankProject/OBP-Bank-Node/internal/api"
	"github.com/OpenBankProject/OBP-Bank-Node/internal/cardano"
	"github.com/OpenBankProject/OBP-Bank-Node/internal/config"
	"github.com/OpenBankProject/OBP-Bank-Node/internal/delivery"
	"github.com/OpenBankProject/OBP-Bank-Node/internal/messaging"
	"github.com/OpenBankProject/OBP-Bank-Node/internal/outbox"
	"github.com/OpenBankProject/OBP-Bank-Node/internal/platform"
	"github.com/OpenBankProject/OBP-Bank-Node/internal/telemetry"
	"github.com/OpenBankProject/OBP-Bank-Node/pkg/models"
)

func main() {
	cfgPath := flag.String("config", "/app/obp-bank-node-config.yaml", "Path to obp-bank-node-config.yaml")
	flag.Parse()

	cfg, err := config.Load(*cfgPath)
	if err != nil {
		fmt.Fprintf(os.Stderr, "config: %v\n", err)
		os.Exit(2)
	}

	log, err := telemetry.NewLogger(cfg.Telemetry.LogLevel)
	if err != nil {
		fmt.Fprintf(os.Stderr, "logger: %v\n", err)
		os.Exit(2)
	}
	defer log.Sync()

	log.Info("starting OBP Bank Node",
		zap.String("bank_id", cfg.Bank.BankID),
		zap.String("delivery_mode", cfg.CBSDelivery.Mode),
		zap.Int("port", cfg.OBPBankNode.Port),
	)

	// Outbox — every other subsystem depends on this.
	if err := os.MkdirAll(filepath.Dir(cfg.Outbox.Path), 0o755); err != nil {
		log.Fatal("create outbox dir", zap.Error(err))
	}
	ob, err := outbox.Open(cfg.Outbox.Path)
	if err != nil {
		log.Fatal("open outbox", zap.Error(err))
	}
	defer ob.Close()

	// Telemetry / Prometheus.
	reg := prometheus.NewRegistry()
	metrics := telemetry.NewMetrics(reg)
	_ = metrics // used by handlers once instrumentation is added in the API layer
	promSrv, err := telemetry.StartPrometheusServer(cfg.Telemetry.Port, reg, log)
	if err != nil {
		log.Fatal("prometheus", zap.Error(err))
	}

	// Subsystem clients — stub implementations for v0.1.
	platformClient := platform.NewStubClient(log.Named("platform"))
	cardanoWriter := cardano.NewStubWriter(log.Named("cardano"))

	// CBS delivery — chosen by config.
	deliverer, err := delivery.NewFromConfig(&cfg.CBSDelivery, cfg.OBPBankNode.LocalSecret, log.Named("delivery"))
	if err != nil {
		log.Fatal("delivery", zap.Error(err))
	}
	log.Info("delivery mode initialised", zap.String("mode", deliverer.Name()))

	// RabbitMQ consumer (Interface C). Real AMQP client — if the broker is
	// down at startup the consumer logs a warning and keeps reconnecting in
	// the background; the bank node continues to accept Interface A1 calls
	// (Section 11 — Outbox and Resilience).
	consumer := messaging.NewRabbitMQConsumer(cfg.RabbitMQ, log.Named("messaging"))
	handlers := messaging.Handlers{
		OnCreditNotification: func(ctx context.Context, c *models.CreditInstruction) error {
			if err := ob.SaveCredit(ctx, c); err != nil {
				log.Error("save credit", zap.Error(err))
				return err
			}
			ref, err := deliverer.Deliver(ctx, c)
			if err != nil {
				metrics.CreditDeliveries.WithLabelValues(deliverer.Name(), "error").Inc()
				return err
			}
			metrics.CreditDeliveries.WithLabelValues(deliverer.Name(), "ok").Inc()
			if ref != "" {
				_ = ob.MarkCreditDelivered(ctx, c.TransactionRequestID, ref)
			}
			return nil
		},

		// Section 5: netting snapshot — log + (future) reconcile against
		// Cardano. The on-chain Record 2 write is the OBP API's responsibility
		// in the spec wording; we receive the snapshot for visibility and
		// audit.
		OnNettingSnapshot: func(ctx context.Context, snap *models.NettingSnapshot) error {
			log.Info("netting snapshot received",
				zap.String("netting_snapshot_id", snap.NettingSnapshotID),
				zap.String("netting_blockchain", snap.NettingBlockchain),
				zap.Strings("currencies", snap.Currencies))
			return nil
		},

		// Section 5: settlement instruction. For Cardano-system instructions
		// we'd initiate an ADA transfer here; for fiat rails (CHAPS, NIBSS)
		// the bank's treasury system would handle it after we surface the
		// instruction. v0.1 logs and returns success — concrete settlement
		// adapters slot in next.
		OnSettlementInstruction: func(ctx context.Context, instr *models.SettlementInstruction) error {
			log.Info("settlement instruction received",
				zap.String("netting_snapshot_id", instr.NettingSnapshotID),
				zap.String("settlement_system", instr.SettlementSystem),
				zap.String("currency", instr.Value.Currency),
				zap.String("amount", instr.Value.Amount),
				zap.String("destination", instr.DestinationAddress))
			return nil
		},

		// Section 5: status update — patch the Transaction Request's status
		// in the local outbox so the bank's status query (Section 8) reflects
		// what the OBP API knows.
		OnStatusUpdate: func(ctx context.Context, upd *models.StatusUpdate) error {
			log.Info("status update received",
				zap.String("transaction_request_id", upd.TransactionRequestID),
				zap.String("status", upd.Status))
			if err := ob.UpdateStatus(ctx, upd.TransactionRequestID, upd.Status); err != nil {
				log.Warn("status update: outbox UpdateStatus failed (TR may be unknown locally)",
					zap.String("transaction_request_id", upd.TransactionRequestID),
					zap.Error(err))
				// Don't propagate — an unknown TR isn't a reason to requeue
				// the message; the upstream status update is informational.
			}
			return nil
		},
	}

	rootCtx, cancel := signal.NotifyContext(context.Background(), os.Interrupt, syscall.SIGTERM)
	defer cancel()

	if err := consumer.Start(rootCtx, handlers); err != nil {
		log.Fatal("consumer start", zap.Error(err))
	}

	// If the chosen delivery mode acks asynchronously (currently: database),
	// run a poll loop that picks up rows the CBS has marked PROCESSED and
	// forwards each to the outbox so MarkCreditDelivered records the ack.
	if poller, ok := deliverer.(delivery.Poller); ok {
		go runDeliveryPollLoop(rootCtx, poller, ob, log.Named("delivery.poll"))
	}

	// Brief pause so the RabbitMQ consumer's first connect attempt either
	// succeeds or fails before we print the preflight summary — otherwise
	// it'd always show "disconnected (retrying)" purely from a race.
	time.Sleep(2 * time.Second)
	printPreflightStatus(os.Stderr, cfg, consumer.Connected)

	// REST API server (Interface A1, A2 status + partial OBP proxy). The
	// consumer's Connected method is passed in so /health can report the live
	// RabbitMQ state instead of a hardcoded "connected".
	apiServer := api.NewServer(cfg, log.Named("api"), platformClient, cardanoWriter, ob, consumer.Connected)
	httpSrv := &http.Server{
		Addr:              fmt.Sprintf(":%d", cfg.OBPBankNode.Port),
		Handler:           apiServer.Router(),
		ReadHeaderTimeout: 10 * time.Second,
	}
	go func() {
		log.Info("api: listening", zap.Int("port", cfg.OBPBankNode.Port))
		if err := httpSrv.ListenAndServe(); err != nil && !errors.Is(err, http.ErrServerClosed) {
			log.Error("api server stopped", zap.Error(err))
		}
	}()

	<-rootCtx.Done()
	log.Info("shutdown signal received")

	shutdownCtx, shutdownCancel := context.WithTimeout(context.Background(), 10*time.Second)
	defer shutdownCancel()
	_ = httpSrv.Shutdown(shutdownCtx)
	_ = promSrv.Shutdown(shutdownCtx)
	_ = consumer.Close()
	if closer, ok := deliverer.(interface{ Close() error }); ok {
		_ = closer.Close()
	}
	log.Info("stopped")
}

// runDeliveryPollLoop ticks every 5 seconds, asks the deliverer for processed
// credits, and marks each one delivered in the local outbox. Idempotent on
// transactionRequestID, so re-seeing the same row is harmless.
func runDeliveryPollLoop(ctx context.Context, p delivery.Poller, ob *outbox.Outbox, log *zap.Logger) {
	const interval = 5 * time.Second
	t := time.NewTicker(interval)
	defer t.Stop()

	log.Info("delivery poll loop started", zap.Duration("interval", interval))
	for {
		select {
		case <-ctx.Done():
			log.Info("delivery poll loop stopped")
			return
		case <-t.C:
			processed, err := p.Poll(ctx)
			if err != nil {
				log.Warn("poll failed", zap.Error(err))
				continue
			}
			if len(processed) == 0 {
				continue
			}
			log.Info("CBS-processed credits picked up; marking delivered in outbox",
				zap.Int("count", len(processed)))
			for _, pc := range processed {
				if err := ob.MarkCreditDelivered(ctx, pc.TransactionRequestID, pc.CBSReference); err != nil {
					log.Warn("MarkCreditDelivered failed",
						zap.String("transaction_request_id", pc.TransactionRequestID),
						zap.Error(err))
				}
			}
		}
	}
}
