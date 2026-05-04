// Copyright (C) 2026 TESOBE GmbH
// SPDX-License-Identifier: AGPL-3.0-or-later

package delivery

import (
	"context"
	"fmt"

	"go.uber.org/zap"

	"github.com/OpenBankProject/OBP-Bank-Node/internal/config"
	"github.com/OpenBankProject/OBP-Bank-Node/pkg/models"
)

// Database — Section 3.3 Mode 3.
// The OBP Bank Node writes a row to the bank's staging table; the CBS polls and updates
// the row when it has processed the credit.
//
// Driver-specific SQL (postgres / mysql / oracle / sqlserver) is selected by
// cfg.Driver. v0.1 ships the structural skeleton — concrete drivers and
// connection management are left for the production implementation, since
// each driver has its own SQL dialect for the upsert and timestamp columns.
type Database struct {
	cfg config.DatabaseCfg
	log *zap.Logger
}

func NewDatabase(cfg config.DatabaseCfg, log *zap.Logger) (*Database, error) {
	switch cfg.Driver {
	case "postgresql", "mysql", "oracle", "sqlserver":
	default:
		return nil, fmt.Errorf("unsupported database driver %q", cfg.Driver)
	}
	return &Database{cfg: cfg, log: log}, nil
}

func (d *Database) Name() string { return "database" }

func (d *Database) Deliver(ctx context.Context, credit *models.CreditInstruction) (string, error) {
	// The skeleton logs the intended insert; a real implementation opens a
	// driver-specific connection pool at startup and executes the INSERT
	// described in Section 3.3 Mode 3 against cfg.Table.
	d.log.Info("delivery/database: would INSERT row",
		zap.String("driver", d.cfg.Driver),
		zap.String("table", d.cfg.Table),
		zap.String("transaction_request_id", credit.TransactionRequestID),
		zap.String("to_account_id", credit.To.AccountID),
		zap.String("currency", credit.Value.Currency),
		zap.String("amount", credit.Value.Amount),
		zap.String("value_date", credit.ValueDate),
	)
	// CBS picks up the row asynchronously; ack arrives later via the polling
	// loop (not the return value here). For the skeleton we treat the insert
	// itself as success.
	return "", nil
}
