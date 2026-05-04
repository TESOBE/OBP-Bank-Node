// Copyright (C) 2026 TESOBE GmbH
// SPDX-License-Identifier: AGPL-3.0-or-later

// Package config loads obp-bank-node-config.yaml. The shape is documented in Section 7 of
// the spec. Every nested struct corresponds 1:1 to a top-level YAML key — keep
// them in sync.
package config

import (
	"fmt"
	"os"

	"gopkg.in/yaml.v3"
)

type Config struct {
	OBPBankNode        OBPBankNodeConfig        `yaml:"obp_bank_node"`
	Bank       BankConfig       `yaml:"bank"`
	OBPAPI     OBPAPIConfig     `yaml:"obp_api"`
	RabbitMQ   RabbitMQConfig   `yaml:"rabbitmq"`
	Cardano    CardanoConfig    `yaml:"cardano"`
	CBSDelivery CBSDelivery     `yaml:"cbs_delivery"`
	Telemetry  TelemetryConfig  `yaml:"telemetry"`
	Dashboard  DashboardConfig  `yaml:"dashboard"`
	Outbox     OutboxConfig     `yaml:"outbox"`
}

type OBPBankNodeConfig struct {
	Port        int    `yaml:"port"`
	LocalSecret string `yaml:"local_secret"`
}

type BankConfig struct {
	BankID    string `yaml:"bank_id"`
	AccountID string `yaml:"account_id"`
	ViewID    string `yaml:"view_id"`
}

type OBPAPIConfig struct {
	BaseURL              string `yaml:"base_url"`
	OAuth2ConsumerKey    string `yaml:"oauth2_consumer_key"`
	OAuth2ConsumerSecret string `yaml:"oauth2_consumer_secret"`
	OAuth2AccessToken    string `yaml:"oauth2_access_token"`
	OAuth2TokenSecret    string `yaml:"oauth2_token_secret"`
}

type RabbitMQConfig struct {
	Host         string `yaml:"host"`
	Port         int    `yaml:"port"`
	Username     string `yaml:"username"`
	Password     string `yaml:"password"`
	VirtualHost  string `yaml:"virtual_host"`
	InboundQueue string `yaml:"inbound_queue"`
}

type CardanoConfig struct {
	WalletAddress    string `yaml:"wallet_address"`
	SigningKeyPath   string `yaml:"signing_key_path"`
	Network          string `yaml:"network"`
	BlockfrostAPIKey string `yaml:"blockfrost_api_key"`
}

type CBSDelivery struct {
	Mode     string         `yaml:"mode"`
	Webhook  WebhookCfg     `yaml:"webhook"`
	Database DatabaseCfg    `yaml:"database"`
	File     FileCfg        `yaml:"file"`
}

type WebhookCfg struct {
	URL            string `yaml:"url"`
	TimeoutSeconds int    `yaml:"timeout_seconds"`
}

type DatabaseCfg struct {
	Host     string `yaml:"host"`
	Port     int    `yaml:"port"`
	Name     string `yaml:"name"`
	Username string `yaml:"username"`
	Password string `yaml:"password"`
	Table    string `yaml:"table"`
	Driver   string `yaml:"driver"`
}

type FileCfg struct {
	DropPath            string `yaml:"drop_path"`
	AcknowledgementPath string `yaml:"acknowledgement_path"`
	Format              string `yaml:"format"`
}

type TelemetryConfig struct {
	Type     string `yaml:"type"`
	Port     int    `yaml:"port"`
	LogLevel string `yaml:"log_level"`
}

type DashboardConfig struct {
	Enabled     bool   `yaml:"enabled"`
	BindAddress string `yaml:"bind_address"`
	Port        int    `yaml:"port"`
	Auth        struct {
		Enabled  bool   `yaml:"enabled"`
		Username string `yaml:"username"`
		Password string `yaml:"password"`
	} `yaml:"auth"`
}

type OutboxConfig struct {
	Path string `yaml:"path"`
}

// Load reads YAML from path and applies sensible defaults for fields absent in
// the file.
func Load(path string) (*Config, error) {
	b, err := os.ReadFile(path)
	if err != nil {
		return nil, fmt.Errorf("read config: %w", err)
	}
	var c Config
	if err := yaml.Unmarshal(b, &c); err != nil {
		return nil, fmt.Errorf("parse config: %w", err)
	}
	c.applyDefaults()
	if err := c.validate(); err != nil {
		return nil, err
	}
	return &c, nil
}

func (c *Config) applyDefaults() {
	if c.OBPBankNode.Port == 0 {
		c.OBPBankNode.Port = 8088
	}
	if c.Bank.ViewID == "" {
		c.Bank.ViewID = "owner"
	}
	if c.CBSDelivery.Mode == "" {
		c.CBSDelivery.Mode = "webhook_obp"
	}
	if c.CBSDelivery.Webhook.TimeoutSeconds == 0 {
		c.CBSDelivery.Webhook.TimeoutSeconds = 30
	}
	if c.CBSDelivery.File.Format == "" {
		c.CBSDelivery.File.Format = "json"
	}
	if c.Telemetry.Type == "" {
		c.Telemetry.Type = "prometheus"
	}
	if c.Telemetry.Port == 0 {
		c.Telemetry.Port = 9090
	}
	if c.Telemetry.LogLevel == "" {
		c.Telemetry.LogLevel = "INFO"
	}
	if c.Outbox.Path == "" {
		c.Outbox.Path = "/app/outbox/obp-bank-node.db"
	}
	if c.Dashboard.BindAddress == "" {
		c.Dashboard.BindAddress = "127.0.0.1"
	}
	if c.Dashboard.Port == 0 {
		c.Dashboard.Port = 8081
	}
}

func (c *Config) validate() error {
	if c.OBPBankNode.LocalSecret == "" {
		return fmt.Errorf("obp_bank_node.local_secret is required")
	}
	if c.Bank.BankID == "" {
		return fmt.Errorf("bank.bank_id is required")
	}
	switch c.CBSDelivery.Mode {
	case "webhook_obp", "webhook_iso20022":
		if c.CBSDelivery.Webhook.URL == "" {
			return fmt.Errorf("cbs_delivery.webhook.url required for mode %q", c.CBSDelivery.Mode)
		}
	case "database":
		if c.CBSDelivery.Database.Host == "" {
			return fmt.Errorf("cbs_delivery.database.host required for database mode")
		}
	case "file":
		if c.CBSDelivery.File.DropPath == "" {
			return fmt.Errorf("cbs_delivery.file.drop_path required for file mode")
		}
	default:
		return fmt.Errorf("unknown cbs_delivery.mode %q", c.CBSDelivery.Mode)
	}
	return nil
}
