// Copyright (C) 2026 TESOBE GmbH
// SPDX-License-Identifier: AGPL-3.0-or-later

package api

import (
	"net/http"
	"time"
)

// handleHealth — Section 9. Returns connection status for OBP API, RabbitMQ,
// and Cardano. RabbitMQ reflects the live AMQP connection state via the
// callback supplied at server construction. OBP API and Cardano are still
// stub-backed for v0.1 and report "stub".
//
// `status` at the top level is "healthy" if the bank node is *operating* —
// per Section 11 we're meant to keep working with degraded north-side
// connectivity, so we don't downgrade overall status when a single dependency
// is disconnected. Operators reading the per-connection map can act on it.
func (s *Server) handleHealth(w http.ResponseWriter, r *http.Request) {
	resp := map[string]any{
		"status":  "healthy",
		"service": "OBP-Bank-Node",
		"version": "0.1.0",
		"connections": map[string]string{
			"obp_api":  "stub",
			"rabbitmq": s.rabbitmqStatus(),
			"cardano":  "stub",
		},
		"uptime_seconds": int(time.Since(s.startedAt).Seconds()),
		"timestamp":      time.Now().UTC().Format(time.RFC3339),
	}
	s.writeJSON(w, http.StatusOK, resp)
}

// rabbitmqStatus returns "connected" / "disconnected" by calling the live
// status callback wired in at construction. Returns "unknown" if the callback
// wasn't supplied (e.g. in tests).
func (s *Server) rabbitmqStatus() string {
	if s.rabbitmqConnected == nil {
		return "unknown"
	}
	if s.rabbitmqConnected() {
		return "connected"
	}
	return "disconnected"
}

func (s *Server) handleReady(w http.ResponseWriter, r *http.Request) {
	s.writeJSON(w, http.StatusOK, map[string]string{"status": "ready"})
}
