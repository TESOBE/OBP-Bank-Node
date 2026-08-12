//! Settlement backend abstraction — the *value* leg of Open Corridor.
//!
//! [`BlockchainBackend`](crate::BlockchainBackend) is a *notary*: its
//! `write_settlement` records that a netted position was settled, but moves no
//! value. A [`SettlementBackend`] is the complement — it actually transfers
//! value to extinguish a netted position. The two compose: the netting engine
//! calls [`SettlementBackend::settle`] to move funds, then
//! [`BlockchainBackend::write_settlement`](crate::BlockchainBackend::write_settlement)
//! to anchor an audit record that references the resulting [`TxReference`].
//!
//! One implementation exists per settlement rail (the `settlement_system` field
//! of the ledger policy): ADA on Cardano (the proof-of-concept,
//! [`crate::cardano::settlement::CardanoAdaSettlement`]), PAPSS, or a stablecoin
//! such as AOC / cKES. They differ only in the body of `settle()`; the netting
//! engine selects one per (currency, policy) and is otherwise rail-agnostic.
//!
//! ## Why FX lives off-chain
//!
//! Netting, FX, and basket NAV are computed in OBP-API, so the settlement chain
//! itself needs *no* on-chain oracle. A backend obtains the settle-time rate
//! from an [`FxSource`] (production: the off-chain price service, e.g. API3
//! consumed by TESOBE; tests/PoC: [`StubFxSource`]). Sizing the transfer at
//! settle-time spot is what removes exposure to settlement-asset drift during
//! the promise→netting window — only the entry/exit FX friction remains, and a
//! stablecoin backend removes even that while reusing this same trait.

use async_trait::async_trait;
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

use crate::{ConfirmationStatus, Result, TxReference};

/// A party to a settlement, identified both by its ledger `bank_id` and by the
/// rail-specific `account` value moves to/from — a Cardano bech32 address, a
/// PAPSS participant id, a stablecoin account — interpreted by the backend.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct PartyRef {
    pub bank_id: String,
    pub account: String,
}

/// A request to settle a single netted position.
///
/// The obligation is denominated in the corridor's book currency
/// (`currency` + `net_amount_minor`); the backend converts to its own
/// settlement asset *at settle time* via an [`FxSource`]. `net_amount_minor` is
/// in the currency's minor units (e.g. cents) as an integer, so there is no
/// floating-point drift in the amount owed.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SettlementInstruction {
    pub snapshot_id: String,
    /// The bank that owes the net and pays it out.
    pub debtor: PartyRef,
    /// The bank that is owed the net and receives it.
    pub creditor: PartyRef,
    /// ISO-4217 code of the book currency, e.g. `"KES"`.
    pub currency: String,
    /// Net owed, in minor units of `currency` (e.g. cents).
    pub net_amount_minor: u128,
    /// Stable key so a retried instruction settles at most once. A backend
    /// should fold this into tx metadata / a deterministic builder input.
    pub idempotency_key: String,
}

/// The result of a submitted settlement transfer — enough detail for the audit
/// record ([`SettlementRecord`](crate::SettlementRecord)) and reconciliation.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SettlementOutcome {
    pub tx: TxReference,
    /// The asset that actually moved, e.g. `"ADA"`.
    pub asset: String,
    /// Amount of `asset` moved, in its smallest unit (e.g. lovelace), as a
    /// decimal string so it stays exact across JSON boundaries.
    pub asset_amount: String,
    /// The settle-time rate applied, if a fiat→asset conversion happened.
    pub fx: Option<FxQuote>,
}

/// A settle-time price quote: minor units of `currency` per one *whole* `asset`.
///
/// Example: 1 ADA = 35.42 KES with KES minor unit = cent ⇒ `asset = "ADA"`,
/// `currency = "KES"`, `minor_per_whole_asset = 3542`.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct FxQuote {
    pub asset: String,
    pub currency: String,
    pub minor_per_whole_asset: u128,
    pub as_of: DateTime<Utc>,
    /// Where the rate came from, for the audit trail and UI (e.g.
    /// `"coingecko"`, `"coingecko×er-api"`, `"api3×er-api"`, `"stub"`).
    #[serde(default)]
    pub source: String,
}

/// Source of settle-time FX rates. Kept off-chain deliberately (see module
/// docs). Production reads the price service; [`StubFxSource`] stands in for the
/// proof-of-concept and tests.
#[async_trait]
pub trait FxSource: Send + Sync + 'static {
    /// Quote `asset` priced in `currency` as of now.
    async fn quote(&self, asset: &str, currency: &str) -> Result<FxQuote>;
}

/// The value leg of settlement. One implementation per `settlement_system`.
#[async_trait]
pub trait SettlementBackend: Send + Sync + 'static {
    /// Identifier of the rail this backend settles on, e.g. `"cardano-ada"`.
    fn system(&self) -> &str;

    /// The account this backend pays *out of* (the debtor account it controls).
    /// Callers fill `SettlementInstruction.debtor.account` with this; `settle`
    /// rejects any instruction whose debtor account doesn't match.
    fn settles_from(&self) -> &str;

    /// Move value to extinguish one netted position. Distinct from
    /// [`BlockchainBackend::write_settlement`](crate::BlockchainBackend::write_settlement),
    /// which only notarizes the outcome and moves nothing.
    async fn settle(&self, instruction: &SettlementInstruction) -> Result<SettlementOutcome>;

    /// Poll whether a submitted settlement transfer has confirmed on its rail.
    async fn confirm(&self, tx: &TxReference) -> Result<ConfirmationStatus>;
}

/// A fixed-rate [`FxSource`] for the proof-of-concept and unit tests. Returns
/// the same `minor_per_whole_asset` for every pair.
#[derive(Debug, Clone)]
pub struct StubFxSource {
    pub minor_per_whole_asset: u128,
}

impl StubFxSource {
    pub fn new(minor_per_whole_asset: u128) -> Self {
        Self { minor_per_whole_asset }
    }
}

#[async_trait]
impl FxSource for StubFxSource {
    async fn quote(&self, asset: &str, currency: &str) -> Result<FxQuote> {
        Ok(FxQuote {
            asset: asset.to_string(),
            currency: currency.to_string(),
            minor_per_whole_asset: self.minor_per_whole_asset,
            as_of: Utc::now(),
            source: "stub".into(),
        })
    }
}
