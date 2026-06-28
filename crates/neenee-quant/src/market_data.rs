//! Market-data tool — fetch quotes / klines / order-book snapshots.
//!
//! This is a stub implementation returning placeholder data so the plumbing
//! (profile isolation, schema, dispatch) can be exercised end-to-end before
//! a real exchange adapter is wired in.

use std::sync::Arc;

use async_trait::async_trait;
use neenee_core::Tool;
use serde_json::json;

/// Fetch market data (quotes, klines, depth) for a symbol.
///
/// Read-only by nature: it observes market state and never mutates an account
/// or the filesystem. A quant agent uses it to inform strategy decisions.
pub struct MarketDataTool {
    _private: (),
}

impl MarketDataTool {
    pub fn new() -> Self {
        Self { _private: () }
    }
}

impl Default for MarketDataTool {
    fn default() -> Self {
        Self::new()
    }
}

#[async_trait]
impl Tool for MarketDataTool {
    fn name(&self) -> &str {
        "market_data"
    }
    fn description(&self) -> &str {
        "Fetch market data for a trading symbol: latest quote, historical \
         klines (candlesticks), or order-book depth. Read-only — does not \
         place or modify any order. Use this to inform a trading strategy \
         decision before placing orders."
    }
    fn parameters(&self) -> serde_json::Value {
        json!({
            "type": "object",
            "properties": {
                "symbol": { "type": "string", "description": "Trading symbol, e.g. BTCUSDT or AAPL" },
                "kind": {
                    "type": "string",
                    "enum": ["quote", "klines", "depth"],
                    "description": "quote = latest price; klines = OHLCV candles; depth = order book"
                },
                "interval": {
                    "type": "string",
                    "description": "kline interval (e.g. 1m, 5m, 1h, 1d). Ignored unless kind=klines."
                },
                "limit": {
                    "type": "integer",
                    "description": "Max number of klines / depth levels to return."
                }
            },
            "required": ["symbol", "kind"]
        })
    }
    async fn call(&self, arguments: &str) -> Result<String, String> {
        let args: serde_json::Value =
            serde_json::from_str(arguments).map_err(|e| format!("Invalid JSON: {e}"))?;
        let symbol = args["symbol"]
            .as_str()
            .ok_or("Missing 'symbol'")?
            .to_string();
        let kind = args["kind"].as_str().ok_or("Missing 'kind'")?;
        match kind {
            "quote" => Ok(json!({
                "symbol": symbol,
                "price": 0.0,
                "note": "stub market_data.quote — wire a real exchange adapter",
            })
            .to_string()),
            "klines" => {
                let interval = args["interval"].as_str().unwrap_or("1h");
                Ok(json!({
                    "symbol": symbol,
                    "interval": interval,
                    "klines": [],
                    "note": "stub market_data.klines — wire a real exchange adapter",
                })
                .to_string())
            }
            "depth" => Ok(json!({
                "symbol": symbol,
                "bids": [],
                "asks": [],
                "note": "stub market_data.depth — wire a real exchange adapter",
            })
            .to_string()),
            other => Err(format!(
                "Unknown kind '{other}' (expected quote|klines|depth)"
            )),
        }
    }
}

/// Convenience: an `Arc<dyn Tool>` ready for an agent's tool list.
pub fn shared() -> Arc<dyn Tool> {
    Arc::new(MarketDataTool::new())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tool() -> MarketDataTool {
        MarketDataTool::new()
    }

    #[test]
    fn name_and_accessors() {
        assert_eq!(tool().name(), "market_data");
        // Description is non-empty and model-facing prose.
        assert!(!tool().description().is_empty());
        // Read-only tool: no scope target, no user, no control flow.
        assert!(!tool().requires_user());
        assert!(!tool().spawns_envoy());
        assert!(!tool().affects_control_flow());
    }

    #[test]
    fn schema_is_object_with_required_symbol_and_kind() {
        let schema = tool().parameters();
        assert_eq!(schema["type"], "object");
        let required = schema["required"].as_array().expect("required is array");
        let names: Vec<&str> = required.iter().filter_map(|v| v.as_str()).collect();
        assert!(names.contains(&"symbol"), "got: {names:?}");
        assert!(names.contains(&"kind"), "got: {names:?}");
        // `kind` is an enum of the three supported request types.
        let kind_enum: Vec<&str> = schema["properties"]["kind"]["enum"]
            .as_array()
            .expect("kind enum")
            .iter()
            .filter_map(|v| v.as_str())
            .collect();
        assert_eq!(kind_enum, ["quote", "klines", "depth"]);
    }

    #[tokio::test]
    async fn call_quote_returns_symbol_and_price() {
        let out = tool()
            .call(r#"{"symbol":"BTCUSDT","kind":"quote"}"#)
            .await
            .expect("quote ok");
        let v: serde_json::Value = serde_json::from_str(&out).unwrap();
        assert_eq!(v["symbol"], "BTCUSDT");
        assert!(v.get("price").is_some(), "price present: {v}");
    }

    #[tokio::test]
    async fn call_klines_defaults_interval_when_absent() {
        let out = tool()
            .call(r#"{"symbol":"ETHUSDT","kind":"klines"}"#)
            .await
            .expect("klines ok");
        let v: serde_json::Value = serde_json::from_str(&out).unwrap();
        // Missing `interval` falls back to "1h".
        assert_eq!(v["interval"], "1h", "default interval applied");
        assert_eq!(v["symbol"], "ETHUSDT");
    }

    #[tokio::test]
    async fn call_klines_honors_explicit_interval() {
        let out = tool()
            .call(r#"{"symbol":"ETHUSDT","kind":"klines","interval":"5m"}"#)
            .await
            .expect("klines ok");
        let v: serde_json::Value = serde_json::from_str(&out).unwrap();
        assert_eq!(v["interval"], "5m", "explicit interval honored");
    }

    #[tokio::test]
    async fn call_depth_returns_empty_books() {
        let out = tool()
            .call(r#"{"symbol":"AAPL","kind":"depth"}"#)
            .await
            .expect("depth ok");
        let v: serde_json::Value = serde_json::from_str(&out).unwrap();
        assert_eq!(v["symbol"], "AAPL");
        assert!(v["bids"].is_array());
        assert!(v["asks"].is_array());
    }

    #[tokio::test]
    async fn call_rejects_unknown_kind() {
        let err = tool()
            .call(r#"{"symbol":"X","kind":"trades"}"#)
            .await
            .expect_err("unknown kind should error");
        assert!(
            err.contains("Unknown kind") && err.contains("trades"),
            "err: {err}"
        );
    }

    #[tokio::test]
    async fn call_rejects_missing_symbol() {
        let err = tool()
            .call(r#"{"kind":"quote"}"#)
            .await
            .expect_err("missing symbol should error");
        assert!(err.contains("symbol"), "err: {err}");
    }

    #[tokio::test]
    async fn call_rejects_invalid_json() {
        let err = tool().call("not json").await.expect_err("bad json");
        assert!(err.contains("Invalid JSON"), "err: {err}");
    }

    #[test]
    fn shared_wraps_a_fresh_instance() {
        let t = shared();
        assert_eq!(t.name(), "market_data");
    }
}
