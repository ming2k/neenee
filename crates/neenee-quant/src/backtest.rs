//! Backtest tool — run a strategy against historical data and report metrics.
//!
//! Read-only with respect to the live account and the filesystem: it simulates
//! trades in-memory and returns performance statistics (PnL, Sharpe, drawdown).
//! A quant agent uses it to validate a strategy before any live order.

use std::sync::Arc;

use async_trait::async_trait;
use neenee_core::Tool;
use serde_json::json;

/// Run a backtest of a trading strategy over historical market data.
///
/// Despite producing "orders" internally, it never touches a live account —
/// all fills are simulated. Returns summary performance metrics.
pub struct BacktestTool {
    _private: (),
}

impl BacktestTool {
    pub fn new() -> Self {
        Self { _private: () }
    }
}

impl Default for BacktestTool {
    fn default() -> Self {
        Self::new()
    }
}

#[async_trait]
impl Tool for BacktestTool {
    fn name(&self) -> &str {
        "backtest"
    }
    fn description(&self) -> &str {
        "Backtest a trading strategy over historical market data and return \
         performance metrics (total return, annualized return, Sharpe ratio, \
         max drawdown, number of trades). Fully simulated — places no live \
         orders and writes no files. Use this to evaluate a strategy before \
         risking real capital."
    }
    fn parameters(&self) -> serde_json::Value {
        json!({
            "type": "object",
            "properties": {
                "symbol": { "type": "string", "description": "Trading symbol to backtest" },
                "strategy": {
                    "type": "string",
                    "description": "Strategy identifier or short spec, e.g. 'sma_cross(50,200)'"
                },
                "start": { "type": "string", "description": "Backtest start date (YYYY-MM-DD)" },
                "end": { "type": "string", "description": "Backtest end date (YYYY-MM-DD)" },
                "initial_capital": { "type": "number", "description": "Starting capital for the simulation" }
            },
            "required": ["symbol", "strategy"]
        })
    }
    async fn call(&self, arguments: &str) -> Result<String, String> {
        let args: serde_json::Value =
            serde_json::from_str(arguments).map_err(|e| format!("Invalid JSON: {e}"))?;
        let symbol = args["symbol"]
            .as_str()
            .ok_or("Missing 'symbol'")?
            .to_string();
        let strategy = args["strategy"]
            .as_str()
            .ok_or("Missing 'strategy'")?
            .to_string();
        Ok(json!({
            "symbol": symbol,
            "strategy": strategy,
            "total_return_pct": 0.0,
            "annualized_return_pct": 0.0,
            "sharpe_ratio": 0.0,
            "max_drawdown_pct": 0.0,
            "trades": 0,
            "note": "stub backtest — wire a real backtest engine",
        })
        .to_string())
    }
}

/// Convenience: an `Arc<dyn Tool>` ready for an agent's tool list.
pub fn shared() -> Arc<dyn Tool> {
    Arc::new(BacktestTool::new())
}
