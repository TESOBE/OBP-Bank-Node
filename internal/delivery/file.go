// Copyright (C) 2026 TESOBE GmbH
// SPDX-License-Identifier: AGPL-3.0-or-later

package delivery

import (
	"context"
	"encoding/csv"
	"encoding/json"
	"fmt"
	"os"
	"path/filepath"

	"go.uber.org/zap"

	"github.com/OpenBankProject/OBP-Bank-Node/internal/config"
	"github.com/OpenBankProject/OBP-Bank-Node/pkg/models"
)

// FileDrop — Section 3.3 Mode 4.
// Writes `{transaction_request_id}.json` (or `.csv`) to a watched directory.
// The CBS picks it up and signals acknowledgement by moving a file to
// AcknowledgementPath.
type FileDrop struct {
	cfg config.FileCfg
	log *zap.Logger
}

func NewFileDrop(cfg config.FileCfg, log *zap.Logger) *FileDrop {
	return &FileDrop{cfg: cfg, log: log}
}

func (f *FileDrop) Name() string { return "file" }

func (f *FileDrop) Deliver(ctx context.Context, credit *models.CreditInstruction) (string, error) {
	if err := os.MkdirAll(f.cfg.DropPath, 0o755); err != nil {
		return "", fmt.Errorf("create drop dir: %w", err)
	}
	switch f.cfg.Format {
	case "json", "":
		path := filepath.Join(f.cfg.DropPath, credit.TransactionRequestID+".json")
		body, err := json.MarshalIndent(credit, "", "  ")
		if err != nil {
			return "", err
		}
		if err := os.WriteFile(path, body, 0o644); err != nil {
			return "", fmt.Errorf("write %s: %w", path, err)
		}
		f.log.Info("delivery/file: wrote credit", zap.String("path", path))
	case "csv":
		path := filepath.Join(f.cfg.DropPath, credit.TransactionRequestID+".csv")
		file, err := os.Create(path)
		if err != nil {
			return "", err
		}
		defer file.Close()
		w := csv.NewWriter(file)
		_ = w.Write([]string{"transaction_request_id", "from_bank_id", "to_account_id", "currency", "amount", "value_date", "description", "promise_id"})
		_ = w.Write([]string{
			credit.TransactionRequestID,
			credit.From.BankID,
			credit.To.AccountID,
			credit.Value.Currency,
			credit.Value.Amount,
			credit.ValueDate,
			credit.Description,
			credit.PromiseID,
		})
		w.Flush()
		if err := w.Error(); err != nil {
			return "", err
		}
		f.log.Info("delivery/file: wrote credit", zap.String("path", path))
	default:
		return "", fmt.Errorf("unsupported file format %q", f.cfg.Format)
	}
	return "", nil
}
