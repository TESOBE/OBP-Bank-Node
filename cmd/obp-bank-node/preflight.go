// Copyright (C) 2026 TESOBE GmbH
// SPDX-License-Identifier: AGPL-3.0-or-later

package main

import (
	"fmt"
	"io"
	"net"
	"net/url"
	"strings"
	"time"

	"github.com/OpenBankProject/OBP-Bank-Node/internal/config"
)

// printPreflightStatus runs a quick reachability sweep over the bank node's
// dependencies and prints a human-readable table to the given writer.
//
// It is best-effort: every check has a short timeout and any failure is
// reported as "unreachable" without aborting startup. Section 11 mandates the
// bank node keep running with degraded north-side connectivity, so this is
// purely informational for the operator watching startup.
//
// Call this *after* consumer.Start has had a moment to attempt its first
// connection, otherwise RabbitMQ will always show as disconnected.
func printPreflightStatus(w io.Writer, cfg *config.Config, rabbitmqConnected func() bool) {
	type row struct{ name, target, status string }

	probeTimeout := 2 * time.Second
	rows := []row{
		{"Outbox", cfg.Outbox.Path, "ok"},
		{"OBP API", cfg.OBPAPI.BaseURL, tcpProbeURL(cfg.OBPAPI.BaseURL, probeTimeout)},
		{
			"RabbitMQ",
			fmt.Sprintf("%s:%d", cfg.RabbitMQ.Host, cfg.RabbitMQ.Port),
			rabbitmqStatus(rabbitmqConnected),
		},
		{"Cardano", "(stubbed)", "ok (stub)"},
	}

	switch cfg.CBSDelivery.Mode {
	case "webhook_obp", "webhook_iso20022":
		rows = append(rows, row{
			"CBS Webhook", cfg.CBSDelivery.Webhook.URL,
			tcpProbeURL(cfg.CBSDelivery.Webhook.URL, probeTimeout),
		})
	case "database":
		d := cfg.CBSDelivery.Database
		rows = append(rows, row{
			"CBS Database",
			fmt.Sprintf("%s://%s:%d/%s", d.Driver, d.Host, d.Port, d.Name),
			tcpProbe(d.Host, d.Port, probeTimeout),
		})
	case "file":
		rows = append(rows, row{
			"CBS File Drop", cfg.CBSDelivery.File.DropPath,
			"configured (not probed)",
		})
	}

	const (
		bar = "════════════════════════════════════════════════════════════════════════════════"
	)
	fmt.Fprintln(w)
	fmt.Fprintln(w, bar)
	fmt.Fprintln(w, "  OBP Bank Node — Startup Dependency Status")
	fmt.Fprintln(w, bar)
	fmt.Fprintf(w, "  %-14s  %-44s  %s\n", "Service", "Target", "Status")
	fmt.Fprintf(w, "  %-14s  %-44s  %s\n", strings.Repeat("-", 14), strings.Repeat("-", 44), strings.Repeat("-", 24))
	for _, r := range rows {
		fmt.Fprintf(w, "  %-14s  %-44s  %s\n", r.name, truncate(r.target, 44), r.status)
	}
	fmt.Fprintln(w, bar)
	fmt.Fprintln(w)
}

// rabbitmqStatus reads the live consumer state. We mark "disconnected" with
// "(retrying)" so the operator knows the consumer hasn't given up — it'll
// keep trying in the background per Section 11.
func rabbitmqStatus(connected func() bool) string {
	if connected == nil {
		return "unknown"
	}
	if connected() {
		return "connected"
	}
	return "disconnected (retrying)"
}

// tcpProbeURL parses a URL, derives a host:port, and TCP-dials it. Returns
// "reachable" / "unreachable" / "invalid url".
func tcpProbeURL(rawURL string, timeout time.Duration) string {
	u, err := url.Parse(rawURL)
	if err != nil || u.Host == "" {
		return "invalid url"
	}
	host := u.Hostname()
	port := u.Port()
	if port == "" {
		switch u.Scheme {
		case "https":
			port = "443"
		case "http":
			port = "80"
		case "amqp":
			port = "5672"
		case "amqps":
			port = "5671"
		default:
			return "no port for scheme " + u.Scheme
		}
	}
	conn, err := net.DialTimeout("tcp", net.JoinHostPort(host, port), timeout)
	if err != nil {
		return "unreachable"
	}
	_ = conn.Close()
	return "reachable"
}

// tcpProbe dials a bare host:port (used for the database delivery mode where
// there's no URL to parse).
func tcpProbe(host string, port int, timeout time.Duration) string {
	conn, err := net.DialTimeout("tcp", net.JoinHostPort(host, fmt.Sprintf("%d", port)), timeout)
	if err != nil {
		return "unreachable"
	}
	_ = conn.Close()
	return "reachable"
}

// truncate cuts a string to n runes, appending "…" if it had to.
func truncate(s string, n int) string {
	if len(s) <= n {
		return s
	}
	return s[:n-1] + "…"
}
