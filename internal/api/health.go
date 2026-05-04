// Copyright (C) 2026 TESOBE GmbH
// SPDX-License-Identifier: AGPL-3.0-or-later

package api

import (
	"net/http"
	"time"
)

// handleHealth — Section 9. Returns connection status for OBP API,
// RabbitMQ, and Cardano. The skeleton reports a static "connected" for stubs;
// real clients should expose a Ping/Status method that this handler queries.
func (s *Server) handleHealth(w http.ResponseWriter, r *http.Request) {
	resp := map[string]any{
		"status":  "healthy",
		"service": "OBP-Bank-Node",
		"version": "0.1.0",
		"connections": map[string]string{
			"obp_api":  "connected",
			"rabbitmq": "connected",
			"cardano":  "connected",
		},
		"uptime_seconds": int(time.Since(s.startedAt).Seconds()),
		"timestamp":      time.Now().UTC().Format(time.RFC3339),
	}
	s.writeJSON(w, http.StatusOK, resp)
}

func (s *Server) handleReady(w http.ResponseWriter, r *http.Request) {
	s.writeJSON(w, http.StatusOK, map[string]string{"status": "ready"})
}
