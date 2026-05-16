// Copyright (C) 2026 TESOBE GmbH
// SPDX-License-Identifier: AGPL-3.0-or-later

package delivery

import (
	"context"
	"database/sql"
	"errors"
	"fmt"
	"regexp"
	"time"

	_ "github.com/jackc/pgx/v5/stdlib" // registers "pgx" driver name
	"go.uber.org/zap"

	"github.com/OpenBankProject/OBP-Bank-Node/internal/config"
	"github.com/OpenBankProject/OBP-Bank-Node/pkg/models"
)

// Database — Section 3.3 Mode 3.
//
// The OBP Bank Node writes a row to a staging table in the bank's CBS database;
// the CBS reads the row, posts the credit to the customer's account, then
// updates the row with status='PROCESSED' and a cbs_reference. The OBP Bank
// Node's poll loop sees the update and forwards the ack back through the
// outbox.
//
// v0.1 implements PostgreSQL only via pgx. Other drivers (mysql / oracle /
// sqlserver) are accepted by config validation but error at construction time
// with a clear "not yet implemented" — adding them is mostly a matter of
// SQL-dialect adjustments to the schema and INSERT statement (e.g. ON
// CONFLICT vs ON DUPLICATE KEY UPDATE vs MERGE).
type Database struct {
	cfg config.DatabaseCfg
	log *zap.Logger
	db  *sql.DB

	// quotedTable is cfg.Table after validation, with double-quotes added so
	// it can be safely interpolated into SQL strings. Postgres can't
	// parameterise identifiers, so we have to interpolate — we lock the value
	// down with a strict regex at construction time to prevent injection.
	quotedTable string
}

// identRe is the strict whitelist for the configured table name. Allows the
// optional `schema.` prefix; both parts are simple identifiers.
var identRe = regexp.MustCompile(`^[a-zA-Z_][a-zA-Z0-9_]{0,62}(\.[a-zA-Z_][a-zA-Z0-9_]{0,62})?$`)

func NewDatabase(cfg config.DatabaseCfg, log *zap.Logger) (*Database, error) {
	switch cfg.Driver {
	case "postgresql":
		// supported
	case "mysql", "oracle", "sqlserver":
		return nil, fmt.Errorf("driver %q is not yet implemented in this build; only \"postgresql\" is supported", cfg.Driver)
	default:
		return nil, fmt.Errorf("unsupported database driver %q (use \"postgresql\")", cfg.Driver)
	}
	if cfg.Table == "" {
		return nil, errors.New("cbs_delivery.database.table is required")
	}
	if !identRe.MatchString(cfg.Table) {
		return nil, fmt.Errorf("cbs_delivery.database.table %q is not a valid identifier (allowed: letters, digits, underscores, optional schema.)", cfg.Table)
	}
	quoted := quoteQualifiedIdent(cfg.Table)

	dsn := fmt.Sprintf(
		"host=%s port=%d dbname=%s user=%s password=%s sslmode=disable",
		cfg.Host, cfg.Port, cfg.Name, cfg.Username, cfg.Password,
	)
	db, err := sql.Open("pgx", dsn)
	if err != nil {
		return nil, fmt.Errorf("open postgres: %w", err)
	}
	db.SetMaxOpenConns(8)
	db.SetMaxIdleConns(2)
	db.SetConnMaxIdleTime(5 * time.Minute)

	pingCtx, cancel := context.WithTimeout(context.Background(), 5*time.Second)
	defer cancel()
	if err := db.PingContext(pingCtx); err != nil {
		// Don't fail startup — Section 11 resilience: log and carry on.
		// Deliveries will error until the database comes back, and the bank
		// node will keep retrying.
		log.Warn("postgres ping failed at startup; deliveries will retry",
			zap.String("dsn", redactDSN(dsn)),
			zap.Error(err))
	}

	d := &Database{cfg: cfg, log: log, db: db, quotedTable: quoted}

	// Idempotent schema setup. The bank's DBA may already have created the
	// table (production); this is a no-op in that case. Useful for dev where
	// nobody's created it yet.
	if err := d.ensureSchema(context.Background()); err != nil {
		log.Warn("ensureSchema failed; if the table is missing deliveries will fail until it's created",
			zap.Error(err))
	}

	return d, nil
}

func (d *Database) Name() string { return "database" }

// Close releases the connection pool. main.go calls this on shutdown.
func (d *Database) Close() error {
	if d.db == nil {
		return nil
	}
	return d.db.Close()
}

// Deliver writes a row to the staging table. Idempotent on transaction_request_id.
func (d *Database) Deliver(ctx context.Context, credit *models.CreditInstruction) (string, error) {
	q := fmt.Sprintf(`
		INSERT INTO %s (
			transaction_request_id, netting_snapshot_id, to_account_id,
			currency, amount, description, value_date,
			promise_id, promise_blockchain
		) VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9)
		ON CONFLICT (transaction_request_id) DO NOTHING`, d.quotedTable)

	valueDate, err := parseDate(credit.ValueDate)
	if err != nil {
		return "", fmt.Errorf("invalid value_date %q: %w", credit.ValueDate, err)
	}

	_, err = d.db.ExecContext(ctx, q,
		credit.TransactionRequestID,
		nullIfEmpty(credit.NettingSnapshotID),
		credit.To.AccountID,
		credit.Value.Currency,
		credit.Value.Amount, // numeric; pgx accepts string for NUMERIC
		nullIfEmpty(credit.Description),
		valueDate,
		nullIfEmpty(credit.PromiseID),
		nullIfEmpty(credit.PromiseBlockchain),
	)
	if err != nil {
		return "", fmt.Errorf("insert into %s: %w", d.cfg.Table, err)
	}

	d.log.Info("delivery/database: row inserted (PENDING)",
		zap.String("table", d.cfg.Table),
		zap.String("transaction_request_id", credit.TransactionRequestID))

	// Ack will arrive later via Poll() once the CBS sets status='PROCESSED'.
	return "", nil
}

// Poll returns rows the CBS has finished processing — status='PROCESSED' with
// a non-null cbs_reference. main.go's poll loop forwards each result to
// outbox.MarkCreditDelivered. Calling MarkCreditDelivered repeatedly for the
// same id is harmless (idempotent UPDATE), so we don't bother filtering rows
// we've already seen — keeps the SQL simple.
func (d *Database) Poll(ctx context.Context) ([]ProcessedCredit, error) {
	q := fmt.Sprintf(`
		SELECT transaction_request_id, cbs_reference
		FROM %s
		WHERE status = 'PROCESSED' AND cbs_reference IS NOT NULL
		ORDER BY processed_at ASC NULLS LAST
		LIMIT 100`, d.quotedTable)

	rows, err := d.db.QueryContext(ctx, q)
	if err != nil {
		return nil, fmt.Errorf("poll %s: %w", d.cfg.Table, err)
	}
	defer rows.Close()

	var out []ProcessedCredit
	for rows.Next() {
		var pc ProcessedCredit
		if err := rows.Scan(&pc.TransactionRequestID, &pc.CBSReference); err != nil {
			return nil, fmt.Errorf("scan: %w", err)
		}
		out = append(out, pc)
	}
	return out, rows.Err()
}

// ensureSchema runs CREATE TABLE IF NOT EXISTS. Column types come from spec
// §3.3 Mode 3. CREATE TABLE IF NOT EXISTS is idempotent in Postgres so it's
// safe to run on every startup.
func (d *Database) ensureSchema(ctx context.Context) error {
	q := fmt.Sprintf(`
		CREATE TABLE IF NOT EXISTS %s (
			transaction_request_id VARCHAR PRIMARY KEY,
			netting_snapshot_id    VARCHAR,
			to_account_id          VARCHAR NOT NULL,
			currency               CHAR(3) NOT NULL,
			amount                 NUMERIC(18,5) NOT NULL,
			description            VARCHAR,
			value_date             DATE,
			promise_id             VARCHAR,
			promise_blockchain     VARCHAR,
			status                 VARCHAR NOT NULL DEFAULT 'PENDING',
			cbs_reference          VARCHAR,
			created_at             TIMESTAMPTZ NOT NULL DEFAULT NOW(),
			processed_at           TIMESTAMPTZ
		)`, d.quotedTable)
	_, err := d.db.ExecContext(ctx, q)
	return err
}

// quoteQualifiedIdent wraps each part of a possibly-schema-qualified
// identifier in double-quotes (`schema.table` → `"schema"."table"`). The
// caller has already validated the input matches identRe, so this is just
// quoting, not sanitisation.
func quoteQualifiedIdent(s string) string {
	for i := 0; i < len(s); i++ {
		if s[i] == '.' {
			return `"` + s[:i] + `"."` + s[i+1:] + `"`
		}
	}
	return `"` + s + `"`
}

// parseDate accepts ISO 8601 date (YYYY-MM-DD); returns sql.NullTime so an
// empty value_date stores NULL.
func parseDate(s string) (sql.NullTime, error) {
	if s == "" {
		return sql.NullTime{}, nil
	}
	t, err := time.Parse("2006-01-02", s)
	if err != nil {
		return sql.NullTime{}, err
	}
	return sql.NullTime{Time: t, Valid: true}, nil
}

func nullIfEmpty(s string) sql.NullString {
	if s == "" {
		return sql.NullString{}
	}
	return sql.NullString{String: s, Valid: true}
}

// redactDSN strips the password before logging.
func redactDSN(dsn string) string {
	out := make([]byte, 0, len(dsn))
	i := 0
	for i < len(dsn) {
		if i+9 <= len(dsn) && dsn[i:i+9] == "password=" {
			out = append(out, []byte("password=***")...)
			i += 9
			for i < len(dsn) && dsn[i] != ' ' {
				i++
			}
			continue
		}
		out = append(out, dsn[i])
		i++
	}
	return string(out)
}
