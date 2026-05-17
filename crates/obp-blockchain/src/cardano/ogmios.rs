//! Minimal Ogmios JSON-RPC client over WebSocket.
//!
//! Phase 1 scope: open a fresh WebSocket per call, send a single JSON-RPC
//! request, await the matching response, close. This is fine for read-only
//! queries and infrequent submissions; it will be promoted to a persistent
//! multiplexed connection (with chain-sync subscriptions) once we need real
//! confirmation streaming.

use std::time::Duration;

use futures_util::{SinkExt, StreamExt};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use thiserror::Error;
use tokio::time::timeout;
use tokio_tungstenite::tungstenite::Message;
use tracing::{debug, warn};

const DEFAULT_TIMEOUT: Duration = Duration::from_secs(15);

#[derive(Debug, Error)]
pub enum OgmiosError {
    #[error("connection failed: {0}")]
    Connect(String),
    #[error("transport: {0}")]
    Transport(String),
    #[error("protocol: {0}")]
    Protocol(String),
    #[error("timeout after {0:?}")]
    Timeout(Duration),
    #[error("rpc error {code}: {message}")]
    Rpc { code: i64, message: String },
}

pub type Result<T> = std::result::Result<T, OgmiosError>;

#[derive(Debug, Clone)]
pub struct OgmiosClient {
    url: String,
}

impl OgmiosClient {
    pub fn new(url: impl Into<String>) -> Self {
        Self { url: url.into() }
    }

    /// Fire a single JSON-RPC call and return the `result` value.
    pub async fn call(&self, method: &str, params: Value) -> Result<Value> {
        let req = JsonRpcRequest {
            jsonrpc: "2.0",
            method,
            params,
            id: "1",
        };
        let payload = serde_json::to_string(&req)
            .map_err(|e| OgmiosError::Protocol(format!("request encode: {e}")))?;
        debug!(url = %self.url, method, "ogmios call");

        let fut = async {
            let (mut ws, _) = tokio_tungstenite::connect_async(&self.url)
                .await
                .map_err(|e| OgmiosError::Connect(e.to_string()))?;
            ws.send(Message::Text(payload))
                .await
                .map_err(|e| OgmiosError::Transport(e.to_string()))?;
            while let Some(msg) = ws.next().await {
                let msg = msg.map_err(|e| OgmiosError::Transport(e.to_string()))?;
                match msg {
                    Message::Text(text) => {
                        let _ = ws.send(Message::Close(None)).await;
                        return parse_response(&text);
                    }
                    Message::Binary(_) | Message::Ping(_) | Message::Pong(_) => continue,
                    Message::Close(_) => {
                        return Err(OgmiosError::Protocol(
                            "server closed before response".into(),
                        ));
                    }
                    Message::Frame(_) => continue,
                }
            }
            Err(OgmiosError::Protocol("stream ended without response".into()))
        };

        timeout(DEFAULT_TIMEOUT, fut)
            .await
            .map_err(|_| OgmiosError::Timeout(DEFAULT_TIMEOUT))?
    }

    /// `queryNetwork/tip` — current chain tip.
    pub async fn tip(&self) -> Result<ChainPoint> {
        let v = self.call("queryNetwork/tip", Value::Object(Default::default())).await?;
        serde_json::from_value(v).map_err(|e| OgmiosError::Protocol(format!("tip decode: {e}")))
    }

    /// `queryLedgerState/utxo` filtered by a single address. Returns the raw
    /// UTxO array; downstream code interprets entries.
    pub async fn utxos_at(&self, address: &str) -> Result<Vec<Value>> {
        let params = serde_json::json!({ "addresses": [address] });
        let v = self.call("queryLedgerState/utxo", params).await?;
        match v {
            Value::Array(arr) => Ok(arr),
            other => Err(OgmiosError::Protocol(format!(
                "utxo: expected array, got {}",
                short_kind(&other)
            ))),
        }
    }

    /// `queryLedgerState/protocolParameters` — raw JSON, decoded by callers.
    pub async fn protocol_parameters(&self) -> Result<Value> {
        self.call(
            "queryLedgerState/protocolParameters",
            Value::Object(Default::default()),
        )
        .await
    }

    /// `submitTransaction` — submit a CBOR-encoded signed tx. Returns the
    /// transaction id reported by the node.
    pub async fn submit_transaction(&self, cbor_hex: &str) -> Result<String> {
        let params = serde_json::json!({ "transaction": { "cbor": cbor_hex } });
        let v = self.call("submitTransaction", params).await?;
        v.get("transaction")
            .and_then(|t| t.get("id"))
            .and_then(|id| id.as_str())
            .map(|s| s.to_string())
            .ok_or_else(|| {
                warn!(?v, "unexpected submitTransaction response shape");
                OgmiosError::Protocol("submitTransaction: missing transaction.id".into())
            })
    }
}

#[derive(Serialize)]
struct JsonRpcRequest<'a> {
    jsonrpc: &'static str,
    method: &'a str,
    params: Value,
    id: &'static str,
}

#[derive(Deserialize)]
struct JsonRpcEnvelope {
    #[serde(default)]
    result: Option<Value>,
    #[serde(default)]
    error: Option<JsonRpcErrorBody>,
}

#[derive(Deserialize)]
struct JsonRpcErrorBody {
    code: i64,
    message: String,
}

fn parse_response(text: &str) -> Result<Value> {
    let env: JsonRpcEnvelope = serde_json::from_str(text)
        .map_err(|e| OgmiosError::Protocol(format!("response decode: {e}")))?;
    if let Some(err) = env.error {
        return Err(OgmiosError::Rpc {
            code: err.code,
            message: err.message,
        });
    }
    env.result
        .ok_or_else(|| OgmiosError::Protocol("response had neither result nor error".into()))
}

fn short_kind(v: &Value) -> &'static str {
    match v {
        Value::Null => "null",
        Value::Bool(_) => "bool",
        Value::Number(_) => "number",
        Value::String(_) => "string",
        Value::Array(_) => "array",
        Value::Object(_) => "object",
    }
}

/// A Cardano chain point: slot + block hash + block height.
#[derive(Debug, Clone, Deserialize, Serialize, PartialEq, Eq)]
pub struct ChainPoint {
    pub slot: u64,
    pub id: String,
    #[serde(default)]
    pub height: Option<u64>,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_success_response() {
        let text = r#"{"jsonrpc":"2.0","method":"queryNetwork/tip","result":{"slot":12,"id":"abc","height":3},"id":"1"}"#;
        let v = parse_response(text).expect("ok");
        assert_eq!(v["slot"], 12);
        assert_eq!(v["id"], "abc");
    }

    #[test]
    fn parse_error_response() {
        let text = r#"{"jsonrpc":"2.0","error":{"code":-32602,"message":"bad params"},"id":"1"}"#;
        match parse_response(text) {
            Err(OgmiosError::Rpc { code, message }) => {
                assert_eq!(code, -32602);
                assert_eq!(message, "bad params");
            }
            other => panic!("expected Rpc error, got {other:?}"),
        }
    }

    #[test]
    fn parse_neither_result_nor_error() {
        let text = r#"{"jsonrpc":"2.0","id":"1"}"#;
        match parse_response(text) {
            Err(OgmiosError::Protocol(_)) => (),
            other => panic!("expected Protocol error, got {other:?}"),
        }
    }
}
