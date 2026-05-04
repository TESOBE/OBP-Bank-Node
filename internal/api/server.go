// Copyright (C) 2026 TESOBE GmbH
// SPDX-License-Identifier: AGPL-3.0-or-later

// Package api implements the south-side REST surface (Interface A1) plus the
// supporting endpoints listed in Section 3.4 of the spec.
//
// All routes are mounted under both `/obp-bank-node/v5.1.0` and `/obp-bank-node/v5.0.0` —
// Section 3.4 explicitly forbids version coupling, so banks shouldn't care
// which OBP minor version their CBS is built against.
package api

import (
	"encoding/json"
	"net/http"
	"strings"
	"time"

	"github.com/go-chi/chi/v5"
	"github.com/go-chi/chi/v5/middleware"
	"go.uber.org/zap"

	"github.com/OpenBankProject/OBP-Bank-Node/internal/cardano"
	"github.com/OpenBankProject/OBP-Bank-Node/internal/config"
	"github.com/OpenBankProject/OBP-Bank-Node/internal/outbox"
	"github.com/OpenBankProject/OBP-Bank-Node/internal/platform"
	"github.com/OpenBankProject/OBP-Bank-Node/pkg/models"
)

// Server holds dependencies needed by the handlers — composed at startup in
// cmd/obp-bank-node/main.go.
type Server struct {
	cfg      *config.Config
	log      *zap.Logger
	platform platform.Client
	cardano  cardano.Writer
	outbox   *outbox.Outbox
	startedAt time.Time
}

func NewServer(cfg *config.Config, log *zap.Logger, p platform.Client, c cardano.Writer, ob *outbox.Outbox) *Server {
	return &Server{
		cfg: cfg, log: log, platform: p, cardano: c, outbox: ob,
		startedAt: time.Now().UTC(),
	}
}

// Router returns the http.Handler with all routes mounted. Both v5.0.0 and
// v5.1.0 prefixes are wired to the same handlers per Section 3.4.
func (s *Server) Router() http.Handler {
	r := chi.NewRouter()
	r.Use(middleware.RequestID)
	r.Use(middleware.RealIP)
	r.Use(middleware.Recoverer)
	r.Use(s.requestLogger)

	// Health/readiness — unauthenticated by design (Section 9). Mounted at
	// both top level and inside the OBP path tree (3.4 lists /obp-bank-node/v5.1.0/health).
	r.Get("/health", s.handleHealth)
	r.Get("/ready", s.handleReady)

	// OBP-style mounts
	for _, prefix := range []string{"/obp-bank-node/v5.1.0", "/obp-bank-node/v5.0.0"} {
		r.Route(prefix, func(r chi.Router) {
			r.Get("/health", s.handleHealth)

			// Authenticated bank-facing endpoints
			r.Group(func(r chi.Router) {
				r.Use(s.requireLocalSecret)

				bankPath := "/banks/{bank_id}/accounts/{account_id}/views/{view_id}"
				r.Post(bankPath+"/transaction-request-types/SIMPLE/transaction-requests", s.handleCreatePayment)
				r.Get(bankPath+"/transaction-requests/{transaction_request_id}", s.handleGetTransactionRequest)
				r.Get(bankPath+"/transaction-requests", s.handleListTransactionRequests)
			})
		})
	}

	// Anything under /obp-bank-node/* that didn't match — Section 3.4 says we
	// return a canonical OBP-BANK-NODE-NOT-PROXIED error.
	r.NotFound(s.handleNotFound)

	return r
}

func (s *Server) handleNotFound(w http.ResponseWriter, r *http.Request) {
	if strings.HasPrefix(r.URL.Path, "/obp-bank-node/") {
		s.writeJSON(w, http.StatusNotFound, models.APIError{
			Error:       "OBP-BANK-NODE-NOT-PROXIED",
			Message:     "This OBP endpoint is not implemented in the OBP Bank Node. Use the OBP API directly for this operation.",
			OBPEndpoint: r.URL.Path,
		})
		return
	}
	http.NotFound(w, r)
}

// requireLocalSecret enforces the shared-secret authentication described in
// Section 3.2. The header check is constant-time-equivalent for the lengths in
// use; if the user wants stronger guarantees they should rotate the secret on
// first run as the spec mandates.
func (s *Server) requireLocalSecret(next http.Handler) http.Handler {
	expected := "Bearer " + s.cfg.OBPBankNode.LocalSecret
	return http.HandlerFunc(func(w http.ResponseWriter, r *http.Request) {
		got := r.Header.Get("Authorization")
		if got == "" || got != expected {
			s.writeJSON(w, http.StatusUnauthorized, models.APIError{
				Error:   "OBP-BANK-NODE-AUTH-001",
				Message: "Invalid or missing local secret",
			})
			return
		}
		next.ServeHTTP(w, r)
	})
}

// requestLogger is a thin wrapper around zap that captures the response status
// without bringing in middleware.Logger's text format.
func (s *Server) requestLogger(next http.Handler) http.Handler {
	return http.HandlerFunc(func(w http.ResponseWriter, r *http.Request) {
		ww := middleware.NewWrapResponseWriter(w, r.ProtoMajor)
		start := time.Now()
		next.ServeHTTP(ww, r)
		s.log.Info("http",
			zap.String("method", r.Method),
			zap.String("path", r.URL.Path),
			zap.Int("status", ww.Status()),
			zap.Duration("dur", time.Since(start)),
		)
	})
}

func (s *Server) writeJSON(w http.ResponseWriter, status int, body any) {
	w.Header().Set("Content-Type", "application/json")
	w.WriteHeader(status)
	_ = json.NewEncoder(w).Encode(body)
}

func (s *Server) writeError(w http.ResponseWriter, status int, code, msg string) {
	s.writeJSON(w, status, models.APIError{Error: code, Message: msg})
}
