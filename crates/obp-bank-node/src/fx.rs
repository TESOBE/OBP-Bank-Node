//! Settle-time FX rates.
//!
//! Two real sources behind the same [`FxSource`] trait:
//!
//! - [`CoinGeckoFxSource`] — direct `asset/fiat` quote from CoinGecko
//!   (decision 2026-07-18). Works only for fiats CoinGecko carries in
//!   `vs_currencies`; KES was dropped from that list (found 2026-07-31), so
//!   direct quoting broke for the demo corridor.
//! - [`CrossRateFxSource`] — `asset/USD × USD/fiat`. The crypto leg is
//!   CoinGecko or an **API3 dAPI** (`Api3ReaderProxyV1.read()` via a raw
//!   `eth_call` — API3 feeds live on EVM chains, there is no REST API; the
//!   proxy address per feed comes from the API3 Market). The fiat leg is
//!   open.er-api.com (free, keyless, carries KES). API3 has no KES forex
//!   feed, so the fiat cross-leg stays necessary under either crypto leg.
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
use tracing::{debug, info};

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

/// Where the `asset/USD` half of a cross rate comes from.
pub enum CryptoUsdLeg {
    CoinGecko { base_url: String },
    /// An API3 dAPI (e.g. ADA/USD) read through its `Api3ReaderProxyV1` on an
    /// EVM chain. `proxy_address` comes from the API3 Market for the feed.
    Api3 { rpc_url: String, proxy_address: String },
}

/// `asset/fiat = asset/USD × USD/fiat`. The fiat leg is open.er-api.com's
/// daily USD table, which carries currencies CoinGecko's `vs_currencies`
/// dropped (KES) and everything API3 has no forex feed for.
pub struct CrossRateFxSource {
    http: reqwest::Client,
    crypto: CryptoUsdLeg,
    /// Normally `https://open.er-api.com`; overridable for tests.
    fiat_base_url: String,
}

impl CrossRateFxSource {
    pub fn new(
        crypto: CryptoUsdLeg,
        fiat_base_url: impl Into<String>,
        timeout_secs: u64,
    ) -> Result<Self> {
        let http = reqwest::Client::builder()
            .timeout(std::time::Duration::from_secs(timeout_secs.max(1)))
            .user_agent(concat!("obp-bank-node/", env!("CARGO_PKG_VERSION")))
            .build()
            .map_err(|e| BlockchainError::Internal(format!("building FX HTTP client: {e}")))?;
        Ok(Self {
            http,
            crypto,
            fiat_base_url: fiat_base_url.into().trim_end_matches('/').to_string(),
        })
    }

    /// USD per one whole `asset`, from the configured crypto leg.
    async fn usd_per_asset(&self, asset: &str) -> Result<f64> {
        match &self.crypto {
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
        }
    }

    /// `USD → currency` multiplier from the fiat table.
    async fn usd_to_fiat(&self, currency: &str) -> Result<f64> {
        let url = format!("{}/v6/latest/USD", self.fiat_base_url);
        debug!(%url, "fetching USD/fiat leg (er-api)");
        let body: serde_json::Value = self
            .http
            .get(&url)
            .send()
            .await
            .map_err(|e| BlockchainError::Rejected(format!("fx: fiat request failed: {e}")))?
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
                BlockchainError::Rejected(format!("fx: no USD/{currency} rate in fiat table"))
            })
    }

    fn source_label(&self) -> &'static str {
        match self.crypto {
            CryptoUsdLeg::CoinGecko { .. } => "coingecko×er-api",
            CryptoUsdLeg::Api3 { .. } => "api3×er-api",
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
        let usd = self.usd_per_asset(asset).await?;
        let fiat = self.usd_to_fiat(currency).await?;
        let price = usd * fiat;
        let minor_per_whole_asset =
            price_to_minor(price, 2).map_err(|e| BlockchainError::Rejected(format!("fx: {e}")))?;
        info!(
            asset,
            currency,
            usd,
            fiat,
            price,
            minor_per_whole_asset,
            source = self.source_label(),
            "settle-time FX quote (cross)"
        );
        Ok(FxQuote {
            asset: asset.to_ascii_uppercase(),
            currency: currency.to_ascii_uppercase(),
            minor_per_whole_asset,
            as_of: Utc::now(),
            source: self.source_label().into(),
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
            CryptoUsdLeg::CoinGecko { base_url: base.clone() },
            &base,
            5,
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
            CryptoUsdLeg::Api3 {
                rpc_url: format!("{base}/rpc"),
                proxy_address: "0x0000000000000000000000000000000000000001".into(),
            },
            &base,
            5,
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
            CryptoUsdLeg::CoinGecko { base_url: base.clone() },
            &base,
            5,
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
            CryptoUsdLeg::CoinGecko { base_url: "https://api.coingecko.com".into() },
            "https://open.er-api.com",
            10,
        )
        .unwrap();
        let q = fx.quote("ADA", "KES").await.unwrap();
        assert!(q.minor_per_whole_asset > 0);
        println!("live cross ADA/KES: {} minor per ADA ({})", q.minor_per_whole_asset, q.source);
    }
}
