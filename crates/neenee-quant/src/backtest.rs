//! Backtest tool — run a strategy against historical data and report metrics.
//!
//! Read-only with respect to the live account and the filesystem: it simulates
//! trades in-memory and returns performance statistics (PnL, Sharpe, drawdown).
//! A quant agent uses it to validate a strategy before any live order.

use std::sync::Arc;

use async_trait::async_trait;
use chrono::NaiveDate;
use neenee_core::Tool;
use serde_json::json;

use crate::runtime::{BacktestOptions, QuantRuntime};

/// Run a backtest of a trading strategy over historical market data.
///
/// Despite producing "orders" internally, it never touches a live account —
/// all fills are simulated. Returns summary performance metrics.
pub struct BacktestTool {
    runtime: QuantRuntime,
}

impl BacktestTool {
    pub fn new() -> Self {
        Self::with_runtime(QuantRuntime::new())
    }

    pub fn with_runtime(runtime: QuantRuntime) -> Self {
        Self { runtime }
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
                "interval": {
                    "type": "string",
                    "description": "Candle interval, e.g. 1d, 1h. Defaults to 1d."
                },
                "limit": {
                    "type": "integer",
                    "description": "Maximum candle count to request. Defaults to the start/end daily span or 365."
                },
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
        let initial_capital = args["initial_capital"].as_f64().unwrap_or(100_000.0);
        let start = optional_string(&args, "start");
        let end = optional_string(&args, "end");
        let interval = args["interval"].as_str().unwrap_or("1d").to_string();
        let default_limit = infer_limit(&interval, start.as_deref(), end.as_deref())?;
        let limit = args["limit"]
            .as_u64()
            .map(|value| value as usize)
            .unwrap_or(default_limit);
        serde_json::to_string(&self.runtime.run_backtest_with_options(
            &symbol,
            &strategy,
            initial_capital,
            BacktestOptions {
                interval,
                limit,
                start,
                end,
            },
        )?)
        .map_err(|e| format!("Serialize backtest failed: {e}"))
    }
}

/// Convenience: an `Arc<dyn Tool>` ready for an agent's tool list.
pub fn shared() -> Arc<dyn Tool> {
    Arc::new(BacktestTool::new())
}

fn optional_string(value: &serde_json::Value, key: &str) -> Option<String> {
    value[key]
        .as_str()
        .map(str::trim)
        .filter(|text| !text.is_empty())
        .map(str::to_string)
}

fn infer_limit(interval: &str, start: Option<&str>, end: Option<&str>) -> Result<usize, String> {
    if interval != "1d" {
        return Ok(365);
    }
    let (Some(start), Some(end)) = (start, end) else {
        return Ok(365);
    };
    let start = parse_date("start", start)?;
    let end = parse_date("end", end)?;
    if end < start {
        return Err("end date must be on or after start date".to_string());
    }
    let days = end.signed_duration_since(start).num_days() + 1;
    Ok((days as usize).clamp(1, 1000))
}

fn parse_date(label: &str, value: &str) -> Result<NaiveDate, String> {
    NaiveDate::parse_from_str(value, "%Y-%m-%d")
        .map_err(|e| format!("{label} must use YYYY-MM-DD: {e}"))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tool() -> BacktestTool {
        BacktestTool::new()
    }

    #[test]
    fn name_and_readonly_traits() {
        assert_eq!(tool().name(), "backtest");
        assert!(!tool().description().is_empty());
        // Simulation only: no account mutation, no user, no control flow.
        assert!(!tool().requires_user());
        assert!(!tool().spawns_envoy());
        assert!(!tool().affects_control_flow());
        // Read-only tools declare no locatable scope target.
        assert!(matches!(
            tool().scope_target("{}"),
            neenee_core::ScopeTarget::Unspecified
        ));
    }

    #[test]
    fn schema_requires_symbol_and_strategy() {
        let schema = tool().parameters();
        assert_eq!(schema["type"], "object");
        let required: Vec<&str> = schema["required"]
            .as_array()
            .expect("required array")
            .iter()
            .filter_map(|v| v.as_str())
            .collect();
        assert!(required.contains(&"symbol"));
        assert!(required.contains(&"strategy"));
    }

    #[tokio::test]
    async fn call_returns_metrics_for_valid_request() {
        let out = tool()
            .call(
                r#"{
                    "symbol":"BTCUSDT",
                    "strategy":"sma_cross(50,200)",
                    "start":"2024-01-01",
                    "end":"2024-12-31",
                    "initial_capital":100000
                }"#,
            )
            .await
            .expect("backtest ok");
        let v: serde_json::Value = serde_json::from_str(&out).unwrap();
        assert_eq!(v["symbol"], "BTCUSDT");
        assert_eq!(v["strategy"], "sma_cross(50,200)");
        assert_eq!(v["interval"], "1d");
        assert_eq!(v["requested_start"], "2024-01-01");
        assert_eq!(v["requested_end"], "2024-12-31");
        assert_eq!(v["candles"], 366);
        assert_eq!(v["market_data_source"], "synthetic-paper");
        // All advertised metrics are present and numeric.
        for key in [
            "total_return_pct",
            "annualized_return_pct",
            "sharpe_ratio",
            "max_drawdown_pct",
        ] {
            assert!(v[key].is_number(), "{key} numeric: {v}");
        }
        assert!(v["trades"].is_i64(), "trades is integer");
        assert!(
            v["trade_log"].as_array().is_some(),
            "trade log present: {v}"
        );
        assert!(
            v["equity_curve"]
                .as_array()
                .is_some_and(|curve| curve.len() == 366),
            "equity curve present: {v}"
        );
    }

    #[tokio::test]
    async fn call_rejects_unknown_strategy() {
        let err = tool()
            .call(r#"{"symbol":"BTCUSDT","strategy":"magic_alpha"}"#)
            .await
            .expect_err("unknown strategy");
        assert!(err.contains("unsupported strategy"), "err: {err}");
    }

    #[tokio::test]
    async fn call_rejects_inverted_date_range() {
        let err = tool()
            .call(
                r#"{
                    "symbol":"BTCUSDT",
                    "strategy":"buy_hold",
                    "start":"2024-12-31",
                    "end":"2024-01-01"
                }"#,
            )
            .await
            .expect_err("bad date range");
        assert!(err.contains("end date"), "err: {err}");
    }

    #[tokio::test]
    async fn call_rejects_missing_strategy() {
        let err = tool()
            .call(r#"{"symbol":"X"}"#)
            .await
            .expect_err("missing strategy");
        assert!(err.contains("strategy"), "err: {err}");
    }

    #[tokio::test]
    async fn call_rejects_missing_symbol() {
        let err = tool()
            .call(r#"{"strategy":"mean_reversion"}"#)
            .await
            .expect_err("missing symbol");
        assert!(err.contains("symbol"), "err: {err}");
    }

    #[tokio::test]
    async fn call_rejects_invalid_json() {
        let err = tool().call("{bad").await.expect_err("bad json");
        assert!(err.contains("Invalid JSON"), "err: {err}");
    }

    #[test]
    fn shared_wraps_a_fresh_instance() {
        assert_eq!(shared().name(), "backtest");
    }
}
