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

	// RabbitMQ consumer (Interface C). Stub keeps the subscription "alive"
	// without actually connecting to a broker. Its handlers route incoming
	// credits straight to the configured deliverer.
	consumer := messaging.NewStubConsumer(log.Named("messaging"), cfg.RabbitMQ.InboundQueue)
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
	}

	rootCtx, cancel := signal.NotifyContext(context.Background(), os.Interrupt, syscall.SIGTERM)
	defer cancel()

	if err := consumer.Start(rootCtx, handlers); err != nil {
		log.Fatal("consumer start", zap.Error(err))
	}

	// REST API server (Interface A1, A2 status + partial OBP proxy).
	apiServer := api.NewServer(cfg, log.Named("api"), platformClient, cardanoWriter, ob)
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
	log.Info("stopped")
}
