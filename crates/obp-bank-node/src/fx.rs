//! Settle-time FX rates.
//!
//! Two real sources behind the same [`FxSource`] trait:
//!
//! - [`CoinGeckoFxSource`] — direct `asset/fiat` quote from CoinGecko
//!   (decision 2026-07-18). Works only for fiats CoinGecko carries in
//!   `vs_currencies`; KES was dropped from that list (found 2026-07-31), so
//!   direct quoting broke for the demo corridor.
//! - [`CrossRateFxSource`] — `asset/USD × USD/fiat`. Each leg is a
//!   **preference-ordered list of sources**, tried in order until one
//!   answers; a fallback is logged with the failed source's error. The quote
//!   `source` field records the sources that actually answered
//!   (`pyth×er-api`), so every persisted settlement names its rate origin
//!   per leg. Crypto-leg sources: CoinGecko, an **API3 dAPI**
//!   (`Api3ReaderProxyV1.read()` via a raw `eth_call` — API3 feeds live on
//!   EVM chains, there is no REST API; the proxy address per feed comes
//!   from the API3 Market), or **Pyth** (`Crypto.ADA/USD` from the public
//!   Hermes endpoint — the same feed the Pyth Lazer Cardano integration
//!   serves on-chain). Fiat-leg sources: Pyth `FX.USD/*` or open.er-api.com
//!   (free, keyless, carries KES). Pyth lists the corridor fiats (NGN, GHS,
//!   KES, TZS, …) but as of 2026-08 only USD/ZAR of those publishes — the
//!   rest are `coming_soon` and rejected as never-published, which is what
//!   the fallback to er-api is for. Pyth FX feeds also pause outside forex
//!   market hours, hence the staleness allowance sized to span a weekend.
//!
//! **Error mapping is deliberate:** every failure here returns
//! [`BlockchainError::Rejected`], because a quote failure happens *before any
//! chain interaction* — the settlement provably was not submitted, so the
//! settlement store treats it as retryable on redelivery (unlike ambiguous
//! transport failures around the chain submit itself).
//!
//! Rate note: both keyless tiers allow far more calls than settlement
//! frequency needs. If corridors multiply, add keys and a short cache here.

use async_trait::async_trait;
use chrono::Utc;
use obp_blockchain::settlement::{FxQuote, FxSource};
use obp_blockchain::{BlockchainError, Result};
use tracing::{debug, info, warn};

pub struct CoinGeckoFxSource {
    http: reqwest::Client,
    base_url: String,
}

impl CoinGeckoFxSource {
    /// `base_url` is normally `https://api.coingecko.com`; overridable for
    /// tests and for a proxying deployment.
    pub fn new(base_url: impl Into<String>, timeout_secs: u64) -> Result<Self> {
        let http = reqwest::Client::builder()
            .timeout(std::time::Duration::from_secs(timeout_secs.max(1)))
            // CoinGecko's edge rejects UA-less requests with 403.
            .user_agent(concat!("obp-bank-node/", env!("CARGO_PKG_VERSION")))
            .build()
            .map_err(|e| BlockchainError::Internal(format!("building FX HTTP client: {e}")))?;
        Ok(Self {
            http,
            base_url: base_url.into().trim_end_matches('/').to_string(),
        })
    }
}

/// CoinGecko coin id for a settlement asset ticker.
fn coin_id(asset: &str) -> Option<&'static str> {
    match asset.to_ascii_uppercase().as_str() {
        "ADA" => Some("cardano"),
        _ => None,
    }
}

/// Convert a major-unit price (e.g. 21.22 KES per ADA) to integer minor units
/// per whole asset (2122), rounding half-up at `exponent` decimals. Errors on
/// non-finite, non-positive, or overflowing prices, and on prices that round
/// to zero minor units (a rate we cannot settle at).
fn price_to_minor(price: f64, exponent: u32) -> std::result::Result<u128, String> {
    if !price.is_finite() || price <= 0.0 {
        return Err(format!("unusable price {price}"));
    }
    let scaled = price * 10f64.powi(exponent as i32);
    if scaled >= 1e30 {
        return Err(format!("price {price} overflows minor-unit domain"));
    }
    let minor = scaled.round() as u128;
    if minor == 0 {
        return Err(format!("price {price} rounds to zero minor units"));
    }
    Ok(minor)
}

#[async_trait]
impl FxSource for CoinGeckoFxSource {
    async fn quote(&self, asset: &str, currency: &str) -> Result<FxQuote> {
        let id = coin_id(asset).ok_or_else(|| {
            BlockchainError::Rejected(format!("fx: no CoinGecko mapping for asset {asset}"))
        })?;
        let vs = currency.to_ascii_lowercase();
        let url = format!(
            "{}/api/v3/simple/price?ids={id}&vs_currencies={vs}&precision=full",
            self.base_url
        );
        debug!(%url, "fetching FX quote");

        let resp = self
            .http
            .get(&url)
            .send()
            .await
            .map_err(|e| BlockchainError::Rejected(format!("fx: request failed: {e}")))?;
        if !resp.status().is_success() {
            return Err(BlockchainError::Rejected(format!(
                "fx: CoinGecko returned HTTP {}",
                resp.status()
            )));
        }
        let body: serde_json::Value = resp
            .json()
            .await
            .map_err(|e| BlockchainError::Rejected(format!("fx: bad response body: {e}")))?;

        let price = body
            .get(id)
            .and_then(|c| c.get(&vs))
            .and_then(serde_json::Value::as_f64)
            .ok_or_else(|| {
                BlockchainError::Rejected(format!("fx: no {asset}/{currency} quote in response"))
            })?;
        let minor_per_whole_asset =
            price_to_minor(price, 2).map_err(|e| BlockchainError::Rejected(format!("fx: {e}")))?;

        info!(
            asset,
            currency, price, minor_per_whole_asset, "settle-time FX quote"
        );
        Ok(FxQuote {
            asset: asset.to_ascii_uppercase(),
            currency: currency.to_ascii_uppercase(),
            minor_per_whole_asset,
            as_of: Utc::now(),
            source: "coingecko".into(),
        })
    }
}

// ---------------------------------------------------------------------------
// Cross-rate source: asset/USD × USD/fiat

/// One source for the `asset/USD` half of a cross rate.
pub enum CryptoUsdLeg {
    CoinGecko { base_url: String },
    /// An API3 dAPI (e.g. ADA/USD) read through its `Api3ReaderProxyV1` on an
    /// EVM chain. `proxy_address` comes from the API3 Market for the feed.
    Api3 { rpc_url: String, proxy_address: String },
    /// Pyth `Crypto.ADA/USD` from a Hermes endpoint (normally
    /// `https://hermes.pyth.network`).
    Pyth { hermes_url: String },
}

impl CryptoUsdLeg {
    fn label(&self) -> &'static str {
        match self {
            CryptoUsdLeg::CoinGecko { .. } => "coingecko",
            CryptoUsdLeg::Api3 { .. } => "api3",
            CryptoUsdLeg::Pyth { .. } => "pyth",
        }
    }
}

/// One source for the `USD/fiat` half of a cross rate.
pub enum FiatUsdLeg {
    /// open.er-api.com's daily USD table.
    ErApi { base_url: String },
    /// Pyth `FX.USD/<fiat>` from a Hermes endpoint. Only usable once Pyth
    /// actually publishes the pair (see module docs).
    Pyth { hermes_url: String },
}

impl FiatUsdLeg {
    fn label(&self) -> &'static str {
        match self {
            FiatUsdLeg::ErApi { .. } => "er-api",
            FiatUsdLeg::Pyth { .. } => "pyth",
        }
    }
}

/// Pyth Hermes feed id (hex) of `Crypto.ADA/USD`.
const PYTH_ADA_USD: &str = "2a01deaec9e51a579277b34b122399984d0bbf57e2458a7e42fecd2829867a0d";

/// Pyth Hermes feed id of `FX.USD/<currency>`, for the fiats Pyth lists.
/// Listed ≠ publishing: a `coming_soon` feed answers with a never-published
/// price, which [`CrossRateFxSource::pyth_price`] rejects.
fn pyth_usd_fiat_id(currency: &str) -> Option<&'static str> {
    Some(match currency.to_ascii_uppercase().as_str() {
        "AOA" => "d4bd2c4174efaef9f9d54a62ac2753ce24a4e9144149417e09bab4ee764a17c3",
        "BWP" => "ba7898c34e6b7ffc472bad521a67efd11ca9a046562fe63c4059c7de2bcbd590",
        "EGP" => "b6b6addd0750cb48e816234b7630f358e8aa290190fb4eb5166a38c4542952f8",
        "GHS" => "cdbc5039dad626cc503512321a527f18a1d8d2a168dc248ee4f487be93139272",
        "KES" => "33cc660971b0e63062d2f67b7183ba17f67b246d4a7170788649979258f7d007",
        "MAD" => "d23d9dd73b502074685a8c0b6692723805a9a847d760b2534ab667ca80eb6798",
        "MUR" => "6887223d3d6c8c737cbe66ffe2d38006cb76eea0d7c7db418b6c83f75cece7d2",
        "MWK" => "e7c1c00d4fda3005c14eb31b781c9d9e4581ace53a2577ee23066988b8d8b46b",
        "MZN" => "c27ec5e190f7daa21bd1ca76aad9248f2b3a605ac27af09024c4f2525b2693d2",
        "NGN" => "2f1601cdfed62c03d39fce4720f5d53e8517af244915fbd24ce8175bb25ab318",
        "RWF" => "2f425f5904e2a110eed99894f228e047a483baa6bd7bfb9f4501629b60a98e83",
        "TND" => "7e4f6ce047acde1f5c03951f49bf8a57c6b0b1e1e2e07252a2b570e258eff3d6",
        "TZS" => "abfb0c861c25124c54a818ec7bb9b02243bff01e9014ecf3e50808f5736f8f8e",
        "UGX" => "1f946ee84ce82dbbaa0fec69cdd4cb9e911076788c77c6f36d42a3491a284ce1",
        "XOF" => "78ce64c90dff33ef2f48e999eb4638ec44da416d5dd99dc6ddd7aeaffec64f0c",
        "ZAR" => "389d889017db82bf42141f23b61b8de938a4e2d156e36312175bebf797f493f1",
        "ZMW" => "e6ee7f0254857de0602c124de8cc8c91e3db17928125454e691c03763239dede",
        _ => return None,
    })
}

/// `asset/fiat = asset/USD × USD/fiat`, each leg from the first source in
/// its preference-ordered list that answers.
pub struct CrossRateFxSource {
    http: reqwest::Client,
    crypto: Vec<CryptoUsdLeg>,
    fiat: Vec<FiatUsdLeg>,
    /// Refuse a Pyth price published longer ago than this. Pyth FX feeds
    /// pause outside forex market hours, so a workable allowance spans a
    /// weekend; crypto feeds publish continuously and sit far under any
    /// sane value.
    max_stale_secs: u64,
}

impl CrossRateFxSource {
    /// `crypto` and `fiat` are tried in order; both must be non-empty.
    pub fn new(
        crypto: Vec<CryptoUsdLeg>,
        fiat: Vec<FiatUsdLeg>,
        timeout_secs: u64,
        max_stale_secs: u64,
    ) -> Result<Self> {
        if crypto.is_empty() || fiat.is_empty() {
            return Err(BlockchainError::Internal(
                "cross-rate FX source needs at least one source per leg".into(),
            ));
        }
        let http = reqwest::Client::builder()
            .timeout(std::time::Duration::from_secs(timeout_secs.max(1)))
            .user_agent(concat!("obp-bank-node/", env!("CARGO_PKG_VERSION")))
            .build()
            .map_err(|e| BlockchainError::Internal(format!("building FX HTTP client: {e}")))?;
        Ok(Self {
            http,
            crypto,
            fiat,
            max_stale_secs,
        })
    }

    /// Latest price of one Pyth feed via Hermes: `price × 10^expo`. Rejects
    /// a feed that has never published (`coming_soon` in Pyth's catalogue),
    /// a non-positive price, and a price older than `max_stale_secs`.
    async fn pyth_price(&self, hermes_url: &str, feed_id: &str, what: &str) -> Result<f64> {
        let url = format!(
            "{}/v2/updates/price/latest?ids[]={feed_id}&parsed=true",
            hermes_url.trim_end_matches('/')
        );
        debug!(%url, what, "fetching pyth leg (hermes)");
        let body: serde_json::Value = self
            .http
            .get(&url)
            .send()
            .await
            .map_err(|e| BlockchainError::Rejected(format!("fx: pyth request failed: {e}")))?
            .error_for_status()
            .map_err(|e| BlockchainError::Rejected(format!("fx: pyth: {e}")))?
            .json()
            .await
            .map_err(|e| BlockchainError::Rejected(format!("fx: pyth bad body: {e}")))?;
        let price_obj = body
            .get("parsed")
            .and_then(serde_json::Value::as_array)
            .and_then(|a| a.first())
            .and_then(|f| f.get("price"))
            .ok_or_else(|| {
                BlockchainError::Rejected(format!("fx: pyth reply has no parsed {what} price"))
            })?;
        // Hermes serialises the fixed-point mantissa as a string.
        let raw: f64 = match price_obj.get("price") {
            Some(serde_json::Value::String(s)) => s.parse().ok(),
            Some(v) => v.as_f64(),
            None => None,
        }
        .ok_or_else(|| BlockchainError::Rejected(format!("fx: pyth {what} price unreadable")))?;
        let expo = price_obj
            .get("expo")
            .and_then(serde_json::Value::as_i64)
            .ok_or_else(|| BlockchainError::Rejected(format!("fx: pyth {what} expo missing")))?;
        let publish_time = price_obj
            .get("publish_time")
            .and_then(serde_json::Value::as_i64)
            .unwrap_or(0);
        if publish_time <= 0 {
            return Err(BlockchainError::Rejected(format!(
                "fx: pyth {what} feed has never published (coming soon)"
            )));
        }
        let age = Utc::now().timestamp().saturating_sub(publish_time);
        if age > self.max_stale_secs as i64 {
            return Err(BlockchainError::Rejected(format!(
                "fx: pyth {what} price is {age}s old (max {})",
                self.max_stale_secs
            )));
        }
        let price = raw * 10f64.powi(expo as i32);
        if !price.is_finite() || price <= 0.0 {
            return Err(BlockchainError::Rejected(format!(
                "fx: pyth {what} price unusable: {price}"
            )));
        }
        Ok(price)
    }

    /// USD per one whole `asset`: first crypto-leg source that answers,
    /// with the label of the source that did.
    async fn usd_per_asset(&self, asset: &str) -> Result<(f64, &'static str)> {
        let mut failures = Vec::new();
        for leg in &self.crypto {
            match self.crypto_leg_usd(leg, asset).await {
                Ok(price) => {
                    if !failures.is_empty() {
                        warn!(
                            asset,
                            used = leg.label(),
                            failed = failures.join("; "),
                            "crypto/USD leg fell back"
                        );
                    }
                    return Ok((price, leg.label()));
                }
                Err(e) => failures.push(format!("{}: {e}", leg.label())),
            }
        }
        Err(BlockchainError::Rejected(format!(
            "fx: every crypto/USD source failed for {asset}: {}",
            failures.join("; ")
        )))
    }

    /// One crypto-leg source's USD price for `asset`.
    async fn crypto_leg_usd(&self, leg: &CryptoUsdLeg, asset: &str) -> Result<f64> {
        match leg {
            CryptoUsdLeg::CoinGecko { base_url } => {
                let id = coin_id(asset).ok_or_else(|| {
                    BlockchainError::Rejected(format!("fx: no CoinGecko mapping for asset {asset}"))
                })?;
                let url = format!(
                    "{base_url}/api/v3/simple/price?ids={id}&vs_currencies=usd&precision=full"
                );
                debug!(%url, "fetching crypto/USD leg (coingecko)");
                let body: serde_json::Value = self
                    .http
                    .get(&url)
                    .send()
                    .await
                    .map_err(|e| BlockchainError::Rejected(format!("fx: request failed: {e}")))?
                    .error_for_status()
                    .map_err(|e| BlockchainError::Rejected(format!("fx: {e}")))?
                    .json()
                    .await
                    .map_err(|e| BlockchainError::Rejected(format!("fx: bad response body: {e}")))?;
                body.get(id)
                    .and_then(|c| c.get("usd"))
                    .and_then(serde_json::Value::as_f64)
                    .ok_or_else(|| {
                        BlockchainError::Rejected(format!("fx: no {asset}/USD quote in response"))
                    })
            }
            CryptoUsdLeg::Api3 {
                rpc_url,
                proxy_address,
            } => {
                // The single configured proxy IS one specific feed; this node
                // settles in ADA only, so guard against silently pricing some
                // other asset off the ADA/USD feed.
                if !asset.eq_ignore_ascii_case("ADA") {
                    return Err(BlockchainError::Rejected(format!(
                        "fx: API3 leg is configured for ADA/USD, cannot price {asset}"
                    )));
                }
                // eth_call of `read()` (selector 0x57de26a4) on the
                // Api3ReaderProxyV1: returns (int224 value, uint32 timestamp),
                // value scaled to 18 decimals.
                let call = serde_json::json!({
                    "jsonrpc": "2.0", "id": 1, "method": "eth_call",
                    "params": [{ "to": proxy_address, "data": "0x57de26a4" }, "latest"],
                });
                debug!(%rpc_url, %proxy_address, "fetching crypto/USD leg (api3 dAPI)");
                let body: serde_json::Value = self
                    .http
                    .post(rpc_url)
                    .json(&call)
                    .send()
                    .await
                    .map_err(|e| BlockchainError::Rejected(format!("fx: api3 rpc failed: {e}")))?
                    .error_for_status()
                    .map_err(|e| BlockchainError::Rejected(format!("fx: api3 rpc: {e}")))?
                    .json()
                    .await
                    .map_err(|e| {
                        BlockchainError::Rejected(format!("fx: api3 rpc bad body: {e}"))
                    })?;
                if let Some(err) = body.get("error") {
                    return Err(BlockchainError::Rejected(format!("fx: api3 rpc error: {err}")));
                }
                let result = body
                    .get("result")
                    .and_then(serde_json::Value::as_str)
                    .ok_or_else(|| {
                        BlockchainError::Rejected("fx: api3 rpc reply has no result".into())
                    })?;
                decode_api3_read(result)
                    .map_err(|e| BlockchainError::Rejected(format!("fx: api3: {e}")))
            }
            CryptoUsdLeg::Pyth { hermes_url } => {
                // The wired feed IS Crypto.ADA/USD; refuse to price some
                // other asset off it.
                if !asset.eq_ignore_ascii_case("ADA") {
                    return Err(BlockchainError::Rejected(format!(
                        "fx: pyth crypto leg is Crypto.ADA/USD, cannot price {asset}"
                    )));
                }
                self.pyth_price(hermes_url, PYTH_ADA_USD, "ADA/USD").await
            }
        }
    }

    /// `USD → currency` multiplier: first fiat-leg source that answers,
    /// with the label of the source that did.
    async fn usd_to_fiat(&self, currency: &str) -> Result<(f64, &'static str)> {
        let mut failures = Vec::new();
        for leg in &self.fiat {
            match self.fiat_leg_rate(leg, currency).await {
                Ok(rate) => {
                    if !failures.is_empty() {
                        warn!(
                            currency,
                            used = leg.label(),
                            failed = failures.join("; "),
                            "USD/fiat leg fell back"
                        );
                    }
                    return Ok((rate, leg.label()));
                }
                Err(e) => failures.push(format!("{}: {e}", leg.label())),
            }
        }
        Err(BlockchainError::Rejected(format!(
            "fx: every USD/fiat source failed for {currency}: {}",
            failures.join("; ")
        )))
    }

    /// One fiat-leg source's `USD → currency` multiplier.
    async fn fiat_leg_rate(&self, leg: &FiatUsdLeg, currency: &str) -> Result<f64> {
        match leg {
            FiatUsdLeg::ErApi { base_url } => {
                let url = format!("{}/v6/latest/USD", base_url.trim_end_matches('/'));
                debug!(%url, "fetching USD/fiat leg (er-api)");
                let body: serde_json::Value = self
                    .http
                    .get(&url)
                    .send()
                    .await
                    .map_err(|e| {
                        BlockchainError::Rejected(format!("fx: fiat request failed: {e}"))
                    })?
                    .error_for_status()
                    .map_err(|e| BlockchainError::Rejected(format!("fx: fiat: {e}")))?
                    .json()
                    .await
                    .map_err(|e| BlockchainError::Rejected(format!("fx: fiat bad body: {e}")))?;
                body.get("rates")
                    .and_then(|r| r.get(currency.to_ascii_uppercase().as_str()))
                    .and_then(serde_json::Value::as_f64)
                    .filter(|r| r.is_finite() && *r > 0.0)
                    .ok_or_else(|| {
                        BlockchainError::Rejected(format!(
                            "fx: no USD/{currency} rate in fiat table"
                        ))
                    })
            }
            FiatUsdLeg::Pyth { hermes_url } => {
                let id = pyth_usd_fiat_id(currency).ok_or_else(|| {
                    BlockchainError::Rejected(format!("fx: pyth lists no FX.USD/{currency} feed"))
                })?;
                self.pyth_price(hermes_url, id, &format!("USD/{currency}")).await
            }
        }
    }

}

/// Decode the hex result of `Api3ReaderProxyV1.read()`: two 32-byte words,
/// `(int224 value, uint32 timestamp)`, value scaled to 18 decimals.
fn decode_api3_read(result: &str) -> std::result::Result<f64, String> {
    let hex = result.strip_prefix("0x").unwrap_or(result);
    if hex.len() < 64 {
        return Err(format!("result too short ({} hex chars)", hex.len()));
    }
    let word = &hex[..64];
    // A price fits comfortably in u128; a value beyond the low 16 bytes (or a
    // negative int224, which sets high bits) is not a usable price.
    if word[..32].bytes().any(|b| b != b'0') {
        return Err("value exceeds expected price range".into());
    }
    let raw = u128::from_str_radix(&word[32..], 16).map_err(|e| format!("bad value word: {e}"))?;
    if raw == 0 {
        return Err("feed value is zero".into());
    }
    Ok(raw as f64 / 1e18)
}

#[async_trait]
impl FxSource for CrossRateFxSource {
    async fn quote(&self, asset: &str, currency: &str) -> Result<FxQuote> {
        let (usd, crypto_source) = self.usd_per_asset(asset).await?;
        let (fiat, fiat_source) = self.usd_to_fiat(currency).await?;
        let price = usd * fiat;
        let minor_per_whole_asset =
            price_to_minor(price, 2).map_err(|e| BlockchainError::Rejected(format!("fx: {e}")))?;
        // `source` names the sources that actually answered each leg —
        // persisted with the settlement and shown in the UI.
        let source = format!("{crypto_source}×{fiat_source}");
        info!(
            asset,
            currency,
            usd,
            fiat,
            price,
            minor_per_whole_asset,
            crypto_source,
            fiat_source,
            "settle-time FX quote (cross)"
        );
        Ok(FxQuote {
            asset: asset.to_ascii_uppercase(),
            currency: currency.to_ascii_uppercase(),
            minor_per_whole_asset,
            as_of: Utc::now(),
            source,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::{routing::get, Json, Router};

    async fn stub(body: serde_json::Value, status: axum::http::StatusCode) -> String {
        let app = Router::new().route(
            "/api/v3/simple/price",
            get(move || {
                let body = body.clone();
                async move { (status, Json(body)) }
            }),
        );
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
        format!("http://{addr}")
    }

    #[test]
    fn price_to_minor_converts_and_rounds() {
        assert_eq!(price_to_minor(21.22111820918581, 2).unwrap(), 2122);
        assert_eq!(price_to_minor(35.42, 2).unwrap(), 3542);
        assert_eq!(price_to_minor(21.229, 2).unwrap(), 2123, "rounds half-up");
        assert_eq!(price_to_minor(0.01, 2).unwrap(), 1);
        assert!(price_to_minor(0.004, 2).is_err(), "rounds to zero");
        assert!(price_to_minor(0.0, 2).is_err());
        assert!(price_to_minor(-1.0, 2).is_err());
        assert!(price_to_minor(f64::NAN, 2).is_err());
        assert!(price_to_minor(1e40, 2).is_err());
    }

    #[tokio::test]
    async fn quotes_ada_in_kes_from_response() {
        let base = stub(
            serde_json::json!({ "cardano": { "kes": 21.22111820918581 } }),
            axum::http::StatusCode::OK,
        )
        .await;
        let fx = CoinGeckoFxSource::new(base, 5).unwrap();
        let q = fx.quote("ADA", "KES").await.unwrap();
        assert_eq!(q.minor_per_whole_asset, 2122);
        assert_eq!(q.asset, "ADA");
        assert_eq!(q.currency, "KES");
    }

    #[tokio::test]
    async fn missing_pair_is_rejected_and_retryable() {
        let base = stub(
            serde_json::json!({ "cardano": { "usd": 0.16 } }), // no KES
            axum::http::StatusCode::OK,
        )
        .await;
        let fx = CoinGeckoFxSource::new(base, 5).unwrap();
        let err = fx.quote("ADA", "KES").await.unwrap_err();
        assert!(
            matches!(err, BlockchainError::Rejected(_)),
            "quote failures must be Rejected (retryable pre-submit), got {err:?}"
        );
    }

    #[tokio::test]
    async fn http_error_is_rejected() {
        let base = stub(
            serde_json::json!({}),
            axum::http::StatusCode::TOO_MANY_REQUESTS,
        )
        .await;
        let fx = CoinGeckoFxSource::new(base, 5).unwrap();
        assert!(matches!(
            fx.quote("ADA", "KES").await.unwrap_err(),
            BlockchainError::Rejected(_)
        ));
    }

    #[tokio::test]
    async fn unreachable_endpoint_is_rejected() {
        let fx = CoinGeckoFxSource::new("http://127.0.0.1:1", 2).unwrap();
        assert!(matches!(
            fx.quote("ADA", "KES").await.unwrap_err(),
            BlockchainError::Rejected(_)
        ));
    }

    #[tokio::test]
    async fn unknown_asset_is_rejected_without_a_request() {
        let fx = CoinGeckoFxSource::new("http://127.0.0.1:1", 2).unwrap();
        let err = fx.quote("DOGE", "KES").await.unwrap_err();
        assert!(err.to_string().contains("no CoinGecko mapping"));
    }

    // ---- cross-rate source -------------------------------------------------

    use axum::routing::post;

    /// One stub serving all three upstream shapes: CoinGecko USD price,
    /// er-api USD table, and an EVM JSON-RPC answering `eth_call`.
    async fn cross_stub(usd_price: f64, fiat_rates: serde_json::Value, api3_raw: u128) -> String {
        let price_body = serde_json::json!({ "cardano": { "usd": usd_price } });
        let fiat_body = serde_json::json!({ "result": "success", "rates": fiat_rates });
        // (int224 value, uint32 timestamp) — two 32-byte words.
        let rpc_result = format!("0x{:064x}{:064x}", api3_raw, 1_754_900_000u64);
        let app = Router::new()
            .route(
                "/api/v3/simple/price",
                get(move || {
                    let b = price_body.clone();
                    async move { Json(b) }
                }),
            )
            .route(
                "/v6/latest/USD",
                get(move || {
                    let b = fiat_body.clone();
                    async move { Json(b) }
                }),
            )
            .route(
                "/rpc",
                post(move || {
                    let r = rpc_result.clone();
                    async move { Json(serde_json::json!({ "jsonrpc": "2.0", "id": 1, "result": r })) }
                }),
            );
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
        format!("http://{addr}")
    }

    #[test]
    fn decode_api3_read_scales_18_decimals() {
        let word = format!("0x{:064x}{:064x}", 620_000_000_000_000_000u128, 1u64);
        assert!((decode_api3_read(&word).unwrap() - 0.62).abs() < 1e-12);
        assert!(decode_api3_read("0x00").is_err(), "too short");
        let zero = format!("0x{:064x}{:064x}", 0u128, 1u64);
        assert!(decode_api3_read(&zero).is_err(), "zero value");
        // High bits set (negative int224 / out of range) is refused.
        let neg = format!("0x{}{:064x}", "f".repeat(64), 1u64);
        assert!(decode_api3_read(&neg).is_err());
    }

    #[tokio::test]
    async fn cross_rate_coingecko_leg_multiplies_usd_and_fiat() {
        let base = cross_stub(0.62, serde_json::json!({ "KES": 129.0 }), 0).await;
        let fx = CrossRateFxSource::new(
            vec![CryptoUsdLeg::CoinGecko { base_url: base.clone() }],
            vec![FiatUsdLeg::ErApi { base_url: base.clone() }],
            5,
            259_200,
        )
        .unwrap();
        let q = fx.quote("ADA", "KES").await.unwrap();
        // 0.62 USD/ADA × 129 KES/USD = 79.98 KES/ADA = 7998 minor.
        assert_eq!(q.minor_per_whole_asset, 7998);
        assert_eq!(q.source, "coingecko×er-api");
        assert_eq!(q.currency, "KES");
    }

    #[tokio::test]
    async fn cross_rate_api3_leg_reads_the_dapi() {
        let base = cross_stub(0.0, serde_json::json!({ "KES": 129.0 }), 620_000_000_000_000_000).await;
        let fx = CrossRateFxSource::new(
            vec![CryptoUsdLeg::Api3 {
                rpc_url: format!("{base}/rpc"),
                proxy_address: "0x0000000000000000000000000000000000000001".into(),
            }],
            vec![FiatUsdLeg::ErApi { base_url: base.clone() }],
            5,
            259_200,
        )
        .unwrap();
        let q = fx.quote("ADA", "KES").await.unwrap();
        assert_eq!(q.minor_per_whole_asset, 7998);
        assert_eq!(q.source, "api3×er-api");
        // The configured proxy is the ADA/USD feed — other assets refused.
        assert!(fx.quote("BTC", "KES").await.is_err());
    }

    #[tokio::test]
    async fn cross_rate_missing_fiat_currency_is_rejected() {
        let base = cross_stub(0.62, serde_json::json!({ "EUR": 0.9 }), 0).await;
        let fx = CrossRateFxSource::new(
            vec![CryptoUsdLeg::CoinGecko { base_url: base.clone() }],
            vec![FiatUsdLeg::ErApi { base_url: base.clone() }],
            5,
            259_200,
        )
        .unwrap();
        assert!(matches!(
            fx.quote("ADA", "KES").await.unwrap_err(),
            BlockchainError::Rejected(_)
        ));
    }

    /// Live check against the real API. Ignored by default (network +
    /// rate-limit dependent): `cargo test -p obp-bank-node -- --ignored fx::`
    #[tokio::test]
    #[ignore = "hits the real CoinGecko API"]
    async fn live_coingecko_quotes_ada_kes() {
        let fx = CoinGeckoFxSource::new("https://api.coingecko.com", 10).unwrap();
        let q = fx.quote("ADA", "KES").await.unwrap();
        assert!(q.minor_per_whole_asset > 0);
        println!("live ADA/KES: {} minor per ADA", q.minor_per_whole_asset);
    }

    /// Live cross-rate check (CoinGecko USD × er-api KES).
    #[tokio::test]
    #[ignore = "hits real CoinGecko + er-api APIs"]
    async fn live_cross_rate_quotes_ada_kes() {
        let fx = CrossRateFxSource::new(
            vec![CryptoUsdLeg::CoinGecko { base_url: "https://api.coingecko.com".into() }],
            vec![FiatUsdLeg::ErApi { base_url: "https://open.er-api.com".into() }],
            10,
            259_200,
        )
        .unwrap();
        let q = fx.quote("ADA", "KES").await.unwrap();
        assert!(q.minor_per_whole_asset > 0);
        println!("live cross ADA/KES: {} minor per ADA ({})", q.minor_per_whole_asset, q.source);
    }

    // ---- pyth legs ---------------------------------------------------------

    /// A Hermes stub: serves `/v2/updates/price/latest`, picking the entry
    /// whose feed id appears in the query string. Prices are Hermes-style
    /// `(mantissa-as-string, expo, publish_time)`.
    async fn hermes_stub(entries: Vec<(&'static str, i64, i64, i64)>) -> String {
        use axum::extract::RawQuery;
        let app = Router::new().route(
            "/v2/updates/price/latest",
            get(move |RawQuery(q): RawQuery| {
                let q = q.unwrap_or_default();
                let hit = entries.iter().find(|(id, ..)| q.contains(id)).copied();
                async move {
                    match hit {
                        Some((id, mantissa, expo, publish_time)) => Json(serde_json::json!({
                            "binary": { "encoding": "hex", "data": ["deadbeef"] },
                            "parsed": [{
                                "id": id,
                                "price": {
                                    "price": mantissa.to_string(),
                                    "conf": "1",
                                    "expo": expo,
                                    "publish_time": publish_time,
                                },
                            }],
                        })),
                        None => Json(serde_json::json!({ "parsed": [] })),
                    }
                }
            }),
        );
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
        format!("http://{addr}")
    }

    const ZAR_ID: &str = "389d889017db82bf42141f23b61b8de938a4e2d156e36312175bebf797f493f1";

    #[tokio::test]
    async fn cross_rate_pyth_crypto_leg_with_erapi_fiat() {
        let now = Utc::now().timestamp();
        let hermes = hermes_stub(vec![(PYTH_ADA_USD, 62_000_000, -8, now)]).await;
        let fiat = cross_stub(0.0, serde_json::json!({ "KES": 129.0 }), 0).await;
        let fx = CrossRateFxSource::new(
            vec![CryptoUsdLeg::Pyth { hermes_url: hermes }],
            vec![FiatUsdLeg::ErApi { base_url: fiat }],
            5,
            259_200,
        )
        .unwrap();
        let q = fx.quote("ADA", "KES").await.unwrap();
        assert_eq!(q.minor_per_whole_asset, 7998);
        assert_eq!(q.source, "pyth×er-api");
        // The wired feed is ADA/USD — other assets refused.
        assert!(fx.quote("BTC", "KES").await.is_err());
    }

    #[tokio::test]
    async fn cross_rate_full_pyth_quotes_both_legs() {
        let now = Utc::now().timestamp();
        let hermes = hermes_stub(vec![
            (PYTH_ADA_USD, 62_000_000, -8, now),
            (ZAR_ID, 1_800_000, -5, now), // 18.00 ZAR per USD
        ])
        .await;
        let fx = CrossRateFxSource::new(
            vec![CryptoUsdLeg::Pyth { hermes_url: hermes.clone() }],
            vec![FiatUsdLeg::Pyth { hermes_url: hermes }],
            5,
            259_200,
        )
        .unwrap();
        let q = fx.quote("ADA", "ZAR").await.unwrap();
        // 0.62 USD/ADA × 18 ZAR/USD = 11.16 ZAR/ADA = 1116 minor.
        assert_eq!(q.minor_per_whole_asset, 1116);
        assert_eq!(q.source, "pyth×pyth");
    }

    #[tokio::test]
    async fn pyth_never_published_feed_is_rejected() {
        let now = Utc::now().timestamp();
        let hermes = hermes_stub(vec![
            (PYTH_ADA_USD, 62_000_000, -8, now),
            (ZAR_ID, 0, -5, 0), // coming_soon: price 0, never published
        ])
        .await;
        let fx = CrossRateFxSource::new(
            vec![CryptoUsdLeg::Pyth { hermes_url: hermes.clone() }],
            vec![FiatUsdLeg::Pyth { hermes_url: hermes }],
            5,
            259_200,
        )
        .unwrap();
        let err = fx.quote("ADA", "ZAR").await.unwrap_err();
        assert!(matches!(err, BlockchainError::Rejected(_)));
        assert!(err.to_string().contains("never published"), "{err}");
    }

    #[tokio::test]
    async fn pyth_stale_price_is_rejected() {
        let old = Utc::now().timestamp() - 10_000;
        let hermes = hermes_stub(vec![(PYTH_ADA_USD, 62_000_000, -8, old)]).await;
        let fiat = cross_stub(0.0, serde_json::json!({ "KES": 129.0 }), 0).await;
        let fx = CrossRateFxSource::new(
            vec![CryptoUsdLeg::Pyth { hermes_url: hermes }],
            vec![FiatUsdLeg::ErApi { base_url: fiat }],
            5,
            60, // far tighter than the 10_000s-old price
        )
        .unwrap();
        let err = fx.quote("ADA", "KES").await.unwrap_err();
        assert!(matches!(err, BlockchainError::Rejected(_)));
        assert!(err.to_string().contains("old"), "{err}");
    }

    #[tokio::test]
    async fn pyth_unlisted_fiat_is_rejected_without_a_request() {
        let fx = CrossRateFxSource::new(
            vec![CryptoUsdLeg::CoinGecko { base_url: "http://127.0.0.1:1".into() }],
            vec![FiatUsdLeg::Pyth { hermes_url: "http://127.0.0.1:1".into() }],
            2,
            259_200,
        )
        .unwrap();
        // CHF has no FX.USD/* entry in the corridor table; the crypto leg is
        // consulted first, so use the fiat-leg helper's error directly.
        let err = fx.usd_to_fiat("CHF").await.unwrap_err();
        assert!(err.to_string().contains("lists no FX.USD/CHF"), "{err}");
    }

    // ---- fallback across leg sources ---------------------------------------

    #[tokio::test]
    async fn fiat_leg_falls_back_from_coming_soon_pyth_to_erapi() {
        let now = Utc::now().timestamp();
        // Pyth serves ADA/USD but has no KES entry (coming_soon in the
        // catalogue → empty parsed reply from the stub).
        let hermes = hermes_stub(vec![(PYTH_ADA_USD, 62_000_000, -8, now)]).await;
        let erapi = cross_stub(0.0, serde_json::json!({ "KES": 129.0 }), 0).await;
        let fx = CrossRateFxSource::new(
            vec![CryptoUsdLeg::Pyth { hermes_url: hermes.clone() }],
            vec![
                FiatUsdLeg::Pyth { hermes_url: hermes },
                FiatUsdLeg::ErApi { base_url: erapi },
            ],
            5,
            259_200,
        )
        .unwrap();
        let q = fx.quote("ADA", "KES").await.unwrap();
        assert_eq!(q.minor_per_whole_asset, 7998);
        // The source names what actually answered each leg.
        assert_eq!(q.source, "pyth×er-api");
    }

    #[tokio::test]
    async fn crypto_leg_falls_back_from_unreachable_pyth_to_coingecko() {
        let base = cross_stub(0.62, serde_json::json!({ "KES": 129.0 }), 0).await;
        let fx = CrossRateFxSource::new(
            vec![
                CryptoUsdLeg::Pyth { hermes_url: "http://127.0.0.1:1".into() },
                CryptoUsdLeg::CoinGecko { base_url: base.clone() },
            ],
            vec![FiatUsdLeg::ErApi { base_url: base }],
            5,
            259_200,
        )
        .unwrap();
        let q = fx.quote("ADA", "KES").await.unwrap();
        assert_eq!(q.minor_per_whole_asset, 7998);
        assert_eq!(q.source, "coingecko×er-api");
    }

    #[tokio::test]
    async fn all_leg_sources_failing_reports_every_error() {
        let fx = CrossRateFxSource::new(
            vec![
                CryptoUsdLeg::Pyth { hermes_url: "http://127.0.0.1:1".into() },
                CryptoUsdLeg::CoinGecko { base_url: "http://127.0.0.1:1".into() },
            ],
            vec![FiatUsdLeg::ErApi { base_url: "http://127.0.0.1:1".into() }],
            2,
            259_200,
        )
        .unwrap();
        let err = fx.quote("ADA", "KES").await.unwrap_err();
        assert!(matches!(err, BlockchainError::Rejected(_)));
        let msg = err.to_string();
        assert!(msg.contains("every crypto/USD source failed"), "{msg}");
        assert!(msg.contains("pyth:") && msg.contains("coingecko:"), "{msg}");
    }

    #[test]
    fn empty_leg_list_is_refused() {
        assert!(CrossRateFxSource::new(vec![], vec![], 5, 259_200).is_err());
    }

    /// Live check: Pyth ADA/USD (Hermes) × er-api KES.
    #[tokio::test]
    #[ignore = "hits real Pyth Hermes + er-api APIs"]
    async fn live_pyth_cross_quotes_ada_kes() {
        let fx = CrossRateFxSource::new(
            vec![CryptoUsdLeg::Pyth { hermes_url: "https://hermes.pyth.network".into() }],
            vec![FiatUsdLeg::ErApi { base_url: "https://open.er-api.com".into() }],
            10,
            259_200,
        )
        .unwrap();
        let q = fx.quote("ADA", "KES").await.unwrap();
        assert!(q.minor_per_whole_asset > 0);
        println!("live pyth×er-api ADA/KES: {} minor per ADA", q.minor_per_whole_asset);
    }

    /// Live check: both legs from Pyth (ZAR is the one African fiat feed
    /// publishing as of 2026-08). Outside forex hours this may reject as
    /// stale — that is the guard working, not a bug.
    #[tokio::test]
    #[ignore = "hits the real Pyth Hermes API"]
    async fn live_full_pyth_quotes_ada_zar() {
        let fx = CrossRateFxSource::new(
            vec![CryptoUsdLeg::Pyth { hermes_url: "https://hermes.pyth.network".into() }],
            vec![FiatUsdLeg::Pyth { hermes_url: "https://hermes.pyth.network".into() }],
            10,
            259_200,
        )
        .unwrap();
        let q = fx.quote("ADA", "ZAR").await.unwrap();
        assert!(q.minor_per_whole_asset > 0);
        println!("live pyth×pyth ADA/ZAR: {} minor per ADA", q.minor_per_whole_asset);
    }
}
