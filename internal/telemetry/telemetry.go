// Copyright (C) 2026 TESOBE GmbH
// SPDX-License-Identifier: AGPL-3.0-or-later

// Package telemetry exposes Prometheus metrics on a separate port and provides
// a structured logger configured from cfg.Telemetry.LogLevel.
package telemetry

import (
	"fmt"
	"net/http"
	"strings"

	"github.com/prometheus/client_golang/prometheus"
	"github.com/prometheus/client_golang/prometheus/promhttp"
	"go.uber.org/zap"
	"go.uber.org/zap/zapcore"
)

// Metrics is a small bag of OBP Bank Node-specific counters/histograms. Add fields here
// when you need to expose new measurements; we register everything once at
// startup.
type Metrics struct {
	PaymentRequests   *prometheus.CounterVec
	PaymentLatency    *prometheus.HistogramVec
	CreditDeliveries  *prometheus.CounterVec
	OutboxQueueDepth  prometheus.Gauge
}

func NewMetrics(reg prometheus.Registerer) *Metrics {
	m := &Metrics{
		PaymentRequests: prometheus.NewCounterVec(prometheus.CounterOpts{
			Namespace: "ocn",
			Subsystem: "api",
			Name:      "payment_requests_total",
			Help:      "Count of payment requests received on Interface A1, by outcome.",
		}, []string{"outcome"}),
		PaymentLatency: prometheus.NewHistogramVec(prometheus.HistogramOpts{
			Namespace: "ocn",
			Subsystem: "api",
			Name:      "payment_request_duration_seconds",
			Help:      "End-to-end latency of an Interface A1 call.",
			Buckets:   prometheus.DefBuckets,
		}, []string{"outcome"}),
		CreditDeliveries: prometheus.NewCounterVec(prometheus.CounterOpts{
			Namespace: "ocn",
			Subsystem: "delivery",
			Name:      "credit_deliveries_total",
			Help:      "Count of inbound credit deliveries by mode and outcome.",
		}, []string{"mode", "outcome"}),
		OutboxQueueDepth: prometheus.NewGauge(prometheus.GaugeOpts{
			Namespace: "ocn",
			Subsystem: "outbox",
			Name:      "queue_depth",
			Help:      "Number of items currently queued in the outbox awaiting processing.",
		}),
	}
	reg.MustRegister(m.PaymentRequests, m.PaymentLatency, m.CreditDeliveries, m.OutboxQueueDepth)
	return m
}

// StartPrometheusServer exposes /metrics on the given port. The returned
// channel is closed when the server exits — wait on it during graceful
// shutdown.
func StartPrometheusServer(port int, reg *prometheus.Registry, log *zap.Logger) (*http.Server, error) {
	mux := http.NewServeMux()
	mux.Handle("/metrics", promhttp.HandlerFor(reg, promhttp.HandlerOpts{Registry: reg}))
	srv := &http.Server{Addr: fmt.Sprintf(":%d", port), Handler: mux}
	go func() {
		log.Info("telemetry: Prometheus server listening", zap.Int("port", port))
		if err := srv.ListenAndServe(); err != nil && err != http.ErrServerClosed {
			log.Error("telemetry server stopped", zap.Error(err))
		}
	}()
	return srv, nil
}

// NewLogger builds a zap logger at the configured level. We use the production
// JSON encoder by default (better for log aggregation in bank operations
// stacks); developers running locally can pipe through `jq` for readability.
func NewLogger(level string) (*zap.Logger, error) {
	lvl := zapcore.InfoLevel
	switch strings.ToUpper(level) {
	case "DEBUG":
		lvl = zapcore.DebugLevel
	case "INFO":
		lvl = zapcore.InfoLevel
	case "WARN", "WARNING":
		lvl = zapcore.WarnLevel
	case "ERROR":
		lvl = zapcore.ErrorLevel
	}
	cfg := zap.NewProductionConfig()
	cfg.Level = zap.NewAtomicLevelAt(lvl)
	return cfg.Build()
}
