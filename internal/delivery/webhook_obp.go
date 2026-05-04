// Copyright (C) 2026 TESOBE GmbH
// SPDX-License-Identifier: AGPL-3.0-or-later

package delivery

import (
	"bytes"
	"context"
	"encoding/json"
	"fmt"
	"io"
	"net/http"
	"time"

	"go.uber.org/zap"

	"github.com/OpenBankProject/OBP-Bank-Node/pkg/models"
)

// WebhookOBP — Section 3.3 Mode 1.
// Posts the CreditInstruction body verbatim to the configured CBS URL.
type WebhookOBP struct {
	url    string
	secret string
	client *http.Client
	log    *zap.Logger
}

func NewWebhookOBP(url, secret string, timeoutSec int, log *zap.Logger) *WebhookOBP {
	return &WebhookOBP{
		url:    url,
		secret: secret,
		client: &http.Client{Timeout: time.Duration(timeoutSec) * time.Second},
		log:    log,
	}
}

func (w *WebhookOBP) Name() string { return "webhook_obp" }

// cbsAck is the expected response body — matches the spec example.
type cbsAck struct {
	Status       string `json:"status"`
	CBSReference string `json:"cbs_reference"`
}

func (w *WebhookOBP) Deliver(ctx context.Context, credit *models.CreditInstruction) (string, error) {
	body, err := json.Marshal(credit)
	if err != nil {
		return "", err
	}
	req, err := http.NewRequestWithContext(ctx, http.MethodPost, w.url, bytes.NewReader(body))
	if err != nil {
		return "", err
	}
	req.Header.Set("Content-Type", "application/json")
	req.Header.Set("Authorization", "Bearer "+w.secret)

	resp, err := w.client.Do(req)
	if err != nil {
		return "", fmt.Errorf("webhook POST: %w", err)
	}
	defer resp.Body.Close()

	respBody, _ := io.ReadAll(resp.Body)
	if resp.StatusCode != http.StatusOK {
		return "", fmt.Errorf("CBS webhook returned %d: %s", resp.StatusCode, string(respBody))
	}

	var ack cbsAck
	if err := json.Unmarshal(respBody, &ack); err != nil {
		// Spec requires HTTP 200 for ack; tolerate missing/non-JSON body but
		// log it. The credit was accepted, but we can't extract a reference.
		w.log.Warn("CBS webhook returned 200 with unparseable body", zap.Error(err))
		return "", nil
	}
	if ack.Status != "ACCEPTED" {
		return "", fmt.Errorf("CBS webhook ack status %q", ack.Status)
	}
	return ack.CBSReference, nil
}
