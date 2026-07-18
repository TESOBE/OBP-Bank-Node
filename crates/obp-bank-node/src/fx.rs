//! Settle-time FX rates from CoinGecko's public API.
//!
//! The interim real-world price source (decision 2026-07-18): free, no key at
//! our volume, and quotes ADA directly in the corridor fiats
//! (`/api/v3/simple/price?ids=cardano&vs_currencies=kes`). The API3 feed
//! integration (TESOBE partnership) can replace this behind the same
//! [`FxSource`] trait later.
//!
//! **Error mapping is deliberate:** every failure here returns
//! [`BlockchainError::Rejected`], because a quote failure happens *before any
//! chain interaction* — the settlement provably was not submitted, so the
//! settlement store treats it as retryable on redelivery (unlike ambiguous
//! transport failures around the chain submit itself).
//!
//! Rate note: the keyless tier allows roughly tens of calls/minute — far above
//! settlement frequency. If corridors multiply, add the demo/pro key header
//! and a short cache here.

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
        let minor_per_whole_asset = price_to_minor(price, 2)
            .map_err(|e| BlockchainError::Rejected(format!("fx: {e}")))?;

        info!(asset, currency, price, minor_per_whole_asset, "settle-time FX quote");
        Ok(FxQuote {
            asset: asset.to_ascii_uppercase(),
            currency: currency.to_ascii_uppercase(),
            minor_per_whole_asset,
            as_of: Utc::now(),
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
        let base = stub(serde_json::json!({}), axum::http::StatusCode::TOO_MANY_REQUESTS).await;
        let fx = CoinGeckoFxSource::new(base, 5).unwrap();
        assert!(matches!(fx.quote("ADA", "KES").await.unwrap_err(), BlockchainError::Rejected(_)));
    }

    #[tokio::test]
    async fn unreachable_endpoint_is_rejected() {
        let fx = CoinGeckoFxSource::new("http://127.0.0.1:1", 2).unwrap();
        assert!(matches!(fx.quote("ADA", "KES").await.unwrap_err(), BlockchainError::Rejected(_)));
    }

    #[tokio::test]
    async fn unknown_asset_is_rejected_without_a_request() {
        let fx = CoinGeckoFxSource::new("http://127.0.0.1:1", 2).unwrap();
        let err = fx.quote("DOGE", "KES").await.unwrap_err();
        assert!(err.to_string().contains("no CoinGecko mapping"));
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
}
