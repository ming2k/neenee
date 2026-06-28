//! Order tools — place live orders and inspect open positions.
//!
//! Unlike [`crate::market_data`] and [`crate::backtest`] (read-only), these
//! tools take real, account-mutating actions: placing an order moves capital.
//! They are the quant domain's analogue of the coding domain's `write_file` /
//! `edit_file` (side-effecting, broker-gated). A quant agent that should only
//! *analyze* (not trade) must not receive them — the [`QUANT`](neenee_core::QUANT)
//! profile encodes exactly that split.

use std::sync::Arc;

use async_trait::async_trait;
use neenee_core::{ScopeTarget, Tool};
use serde_json::json;

/// Place a live order on the configured exchange/broker.
///
/// Side-effecting and account-mutating. Reports a [`ScopeTarget::Command`]
/// style target so an operation-scope gate can restrain live trading if
/// desired (e.g. a "paper-only" envoy scope).
pub struct PlaceOrderTool {
    _private: (),
}

impl PlaceOrderTool {
    pub fn new() -> Self {
        Self { _private: () }
    }
}

impl Default for PlaceOrderTool {
    fn default() -> Self {
        Self::new()
    }
}

#[async_trait]
impl Tool for PlaceOrderTool {
    fn name(&self) -> &str {
        "place_order"
    }
    fn description(&self) -> &str {
        "Place a LIVE order on the configured exchange/broker. This moves real \
         capital — use only after a strategy has been backtested and the user \
         has approved trading. Prefer ask_user to confirm ambiguous size/side \
         decisions before placing. Returns the exchange order id and status."
    }
    fn parameters(&self) -> serde_json::Value {
        json!({
            "type": "object",
            "properties": {
                "symbol": { "type": "string", "description": "Trading symbol, e.g. BTCUSDT" },
                "side": { "type": "string", "enum": ["buy", "sell"] },
                "type": {
                    "type": "string",
                    "enum": ["market", "limit"],
                    "description": "market = fill at best price; limit = fill at price or better"
                },
                "quantity": { "type": "number", "description": "Order size in base currency" },
                "price": { "type": "number", "description": "Required for limit orders; ignored for market" }
            },
            "required": ["symbol", "side", "type", "quantity"]
        })
    }
    /// Surfaced as a Command target so an operation scope can gate live
    /// trading (e.g. a paper-trading envoy). The command string carries
    /// the symbol/side so a permission prompt is informative.
    fn scope_target(&self, arguments: &str) -> ScopeTarget {
        let side = serde_json::from_str::<serde_json::Value>(arguments)
            .ok()
            .and_then(|v| v["side"].as_str().map(str::to_string))
            .unwrap_or_else(|| "?".to_string());
        ScopeTarget::Command(format!("trade:{side}"))
    }
    async fn call(&self, arguments: &str) -> Result<String, String> {
        let args: serde_json::Value =
            serde_json::from_str(arguments).map_err(|e| format!("Invalid JSON: {e}"))?;
        let symbol = args["symbol"]
            .as_str()
            .ok_or("Missing 'symbol'")?
            .to_string();
        let side = args["side"].as_str().ok_or("Missing 'side'")?;
        let order_type = args["type"].as_str().ok_or("Missing 'type'")?;
        let quantity = args["quantity"]
            .as_f64()
            .ok_or("Missing or non-numeric 'quantity'")?;
        Ok(json!({
            "status": "stub",
            "order_id": "STUB-000000",
            "symbol": symbol,
            "side": side,
            "type": order_type,
            "quantity": quantity,
            "note": "stub place_order — wire a real exchange adapter before going live",
        })
        .to_string())
    }
}

/// List currently open positions / working orders on the account.
///
/// Read-only with respect to the market, but reads private account state. A
/// quant agent uses it to review exposure before adjusting positions.
pub struct ListPositionsTool {
    _private: (),
}

impl ListPositionsTool {
    pub fn new() -> Self {
        Self { _private: () }
    }
}

impl Default for ListPositionsTool {
    fn default() -> Self {
        Self::new()
    }
}

#[async_trait]
impl Tool for ListPositionsTool {
    fn name(&self) -> &str {
        "list_positions"
    }
    fn description(&self) -> &str {
        "List the account's currently open positions and working orders. \
         Read-only — does not place or cancel anything. Use this to review \
         current exposure before deciding to adjust positions."
    }
    fn parameters(&self) -> serde_json::Value {
        json!({
            "type": "object",
            "properties": {
                "symbol": { "type": "string", "description": "Optional filter: only return positions for this symbol" }
            }
        })
    }
    async fn call(&self, _arguments: &str) -> Result<String, String> {
        Ok(json!({
            "positions": [],
            "open_orders": [],
            "note": "stub list_positions — wire a real exchange/broker adapter",
        })
        .to_string())
    }
}

/// Convenience: `Arc<dyn Tool>` ready for an agent's tool list.
pub fn place_order() -> Arc<dyn Tool> {
    Arc::new(PlaceOrderTool::new())
}

/// Convenience: `Arc<dyn Tool>` ready for an agent's tool list.
pub fn list_positions() -> Arc<dyn Tool> {
    Arc::new(ListPositionsTool::new())
}

#[cfg(test)]
mod tests {
    use super::*;

    // ---- PlaceOrderTool ----

    #[test]
    fn place_order_name_and_traits() {
        let t = PlaceOrderTool::new();
        assert_eq!(t.name(), "place_order");
        assert!(!t.description().is_empty());
        // Live trading is interactive-capable by nature but does not spawn
        // agents or control the process.
        assert!(!t.spawns_envoy());
        assert!(!t.affects_control_flow());
    }

    #[test]
    fn place_order_schema_requires_core_fields_and_enum_sides() {
        let schema = PlaceOrderTool::new().parameters();
        let required: Vec<&str> = schema["required"]
            .as_array()
            .expect("required array")
            .iter()
            .filter_map(|v| v.as_str())
            .collect();
        for field in ["symbol", "side", "type", "quantity"] {
            assert!(required.contains(&field), "missing required {field}");
        }
        let sides: Vec<&str> = schema["properties"]["side"]["enum"]
            .as_array()
            .expect("side enum")
            .iter()
            .filter_map(|v| v.as_str())
            .collect();
        assert_eq!(sides, ["buy", "sell"]);
        let types: Vec<&str> = schema["properties"]["type"]["enum"]
            .as_array()
            .expect("type enum")
            .iter()
            .filter_map(|v| v.as_str())
            .collect();
        assert_eq!(types, ["market", "limit"]);
    }

    #[test]
    fn place_order_scope_target_classifies_as_command_with_side() {
        let t = PlaceOrderTool::new();
        // A buy surfaces as `trade:buy` so a permission prompt is informative.
        let buy = t.scope_target(r#"{"side":"buy","symbol":"BTCUSDT"}"#);
        assert!(matches!(buy, ScopeTarget::Command(ref c) if c == "trade:buy"));
        // A sell surfaces as `trade:sell`.
        let sell = t.scope_target(r#"{"side":"sell"}"#);
        assert!(matches!(sell, ScopeTarget::Command(ref c) if c == "trade:sell"));
    }

    #[test]
    fn place_order_scope_target_degrades_gracefully_on_bad_input() {
        let t = PlaceOrderTool::new();
        // Malformed JSON or a missing `side` falls back to a placeholder
        // target rather than panicking — the scope gate still classifies it
        // as a Command, so live trading can be gated.
        let malformed = t.scope_target("not json");
        assert!(matches!(malformed, ScopeTarget::Command(ref c) if c == "trade:?"));
        let missing = t.scope_target(r#"{"symbol":"X"}"#);
        assert!(matches!(missing, ScopeTarget::Command(ref c) if c == "trade:?"));
    }

    #[tokio::test]
    async fn place_order_call_echoes_request_fields() {
        let out = PlaceOrderTool::new()
            .call(r#"{"symbol":"BTCUSDT","side":"buy","type":"market","quantity":0.5}"#)
            .await
            .expect("place ok");
        let v: serde_json::Value = serde_json::from_str(&out).unwrap();
        assert_eq!(v["symbol"], "BTCUSDT");
        assert_eq!(v["side"], "buy");
        assert_eq!(v["type"], "market");
        assert_eq!(v["quantity"], 0.5);
        // Stub implementation flags itself clearly.
        assert_eq!(v["status"], "stub");
        assert!(v["order_id"].as_str().unwrap().starts_with("STUB-"));
    }

    #[tokio::test]
    async fn place_order_call_rejects_non_numeric_quantity() {
        let err = PlaceOrderTool::new()
            .call(r#"{"symbol":"X","side":"buy","type":"market","quantity":"lots"}"#)
            .await
            .expect_err("bad quantity");
        assert!(err.contains("quantity"), "err: {err}");
    }

    #[tokio::test]
    async fn place_order_call_rejects_missing_side() {
        let err = PlaceOrderTool::new()
            .call(r#"{"symbol":"X","type":"market","quantity":1}"#)
            .await
            .expect_err("missing side");
        assert!(err.contains("side"), "err: {err}");
    }

    #[tokio::test]
    async fn place_order_call_rejects_invalid_json() {
        let err = PlaceOrderTool::new()
            .call("nope")
            .await
            .expect_err("bad json");
        assert!(err.contains("Invalid JSON"), "err: {err}");
    }

    // ---- ListPositionsTool ----

    #[test]
    fn list_positions_name_and_readonly_traits() {
        let t = ListPositionsTool::new();
        assert_eq!(t.name(), "list_positions");
        assert!(!t.requires_user());
        assert!(!t.spawns_envoy());
        assert!(!t.affects_control_flow());
        // Read-only account query declares no scope target.
        assert!(matches!(t.scope_target("{}"), ScopeTarget::Unspecified));
    }

    #[test]
    fn list_positions_schema_has_optional_symbol_filter() {
        let schema = ListPositionsTool::new().parameters();
        // `symbol` is present as a property but NOT required (it's a filter).
        assert!(schema["properties"]["symbol"].is_object());
        let required = schema["required"].as_array();
        assert!(
            required.map(|r| r.is_empty()).unwrap_or(true),
            "no required fields for list_positions, got: {required:?}"
        );
    }

    #[tokio::test]
    async fn list_positions_call_returns_empty_account() {
        let out = ListPositionsTool::new().call("{}").await.expect("list ok");
        let v: serde_json::Value = serde_json::from_str(&out).unwrap();
        assert!(v["positions"].is_array());
        assert!(v["open_orders"].is_array());
    }

    #[tokio::test]
    async fn list_positions_call_ignores_arguments() {
        // Even with a symbol filter, the stub returns the full (empty) state —
        // and never errors on argument parsing because the contract is lenient.
        let out = ListPositionsTool::new()
            .call(r#"{"symbol":"BTCUSDT"}"#)
            .await
            .expect("list ok");
        assert!(out.contains("positions"));
    }

    #[tokio::test]
    async fn list_positions_call_tolerates_garbage_input() {
        // `_arguments` is ignored, so malformed JSON still succeeds.
        let out = ListPositionsTool::new()
            .call("garbage")
            .await
            .expect("list ok despite garbage");
        assert!(out.contains("note"));
    }

    // ---- convenience accessors ----

    #[test]
    fn accessors_wrap_fresh_instances() {
        assert_eq!(place_order().name(), "place_order");
        assert_eq!(list_positions().name(), "list_positions");
    }
}
