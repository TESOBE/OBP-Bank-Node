// Copyright (C) 2026 TESOBE GmbH
// SPDX-License-Identifier: AGPL-3.0-or-later

package api

import (
	"net/http"

	"github.com/go-chi/chi/v5"
	"go.uber.org/zap"
)

// handleGetTransactionRequest — Section 3.4 / Section 8.
// Returns the current status of a transaction request including OBP Bank Node-specific
// fields (promise_id, netting_snapshot_id, settlement_id).
func (s *Server) handleGetTransactionRequest(w http.ResponseWriter, r *http.Request) {
	id := chi.URLParam(r, "transaction_request_id")
	tr, err := s.outbox.GetTransactionRequest(r.Context(), id)
	if err != nil {
		s.log.Error("outbox lookup failed", zap.Error(err))
		s.writeError(w, http.StatusInternalServerError, "OBP-BANK-NODE-INTERNAL-002", "Lookup failed")
		return
	}
	if tr == nil {
		s.writeError(w, http.StatusNotFound, "OBP-30017", "Transaction request not found")
		return
	}
	s.writeJSON(w, http.StatusOK, tr)
}

// handleListTransactionRequests returns the most recent N TRs submitted via
// this OBP Bank Node instance.
func (s *Server) handleListTransactionRequests(w http.ResponseWriter, r *http.Request) {
	trs, err := s.outbox.ListTransactionRequests(r.Context(), 100)
	if err != nil {
		s.writeError(w, http.StatusInternalServerError, "OBP-BANK-NODE-INTERNAL-002", "Lookup failed")
		return
	}
	if trs == nil {
		trs = nil
	}
	s.writeJSON(w, http.StatusOK, map[string]any{"transaction_requests": trs})
}
