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
