//! Order tools — place broker orders and inspect open positions.
//!
//! Unlike [`crate::market_data`] and [`crate::backtest`] (read-only), these
//! tools are account-mutating in whichever broker runtime is configured. The
//! default runtime is paper trading; a live adapter must be configured
//! explicitly and remain broker-gated. They are the quant domain's analogue of
//! the coding domain's `write_file` / `edit_file` (side-effecting,
//! broker-gated). A quant agent that should only *analyze* (not trade) must
//! not receive them — the [`QUANT`](neenee_core::QUANT) profile encodes
//! exactly that split.

use std::sync::Arc;

use async_trait::async_trait;
use neenee_core::{ScopeTarget, Tool};
use serde_json::json;

use crate::runtime::{CancelOrderRequest, OrderRequest, OrderSide, OrderType, QuantRuntime};

/// Place an order on the configured exchange/broker runtime.
///
/// Side-effecting and account-mutating. The default implementation fills in a
/// shared paper account. A future live adapter must keep the same
/// [`ScopeTarget::Command`] classification so an operation-scope gate can
/// restrain live trading.
pub struct PlaceOrderTool {
    runtime: QuantRuntime,
}

impl PlaceOrderTool {
    pub fn new() -> Self {
        Self::with_runtime(QuantRuntime::new())
    }

    pub fn with_runtime(runtime: QuantRuntime) -> Self {
        Self { runtime }
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
        "Place an order on the configured exchange/broker runtime. The default \
         runtime is paper trading; a live adapter must be configured explicitly \
         and user-approved because it can move real capital. Prefer ask_user to \
         confirm ambiguous size/side decisions before placing. Returns the \
         broker order id and status."
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
        let side = parse_side(args["side"].as_str().ok_or("Missing 'side'")?)?;
        let order_type = parse_order_type(args["type"].as_str().ok_or("Missing 'type'")?)?;
        let quantity = args["quantity"]
            .as_f64()
            .ok_or("Missing or non-numeric 'quantity'")?;
        if quantity <= 0.0 || !quantity.is_finite() {
            return Err("quantity must be a positive finite number".to_string());
        }
        let order = self.runtime.place_order(OrderRequest {
            symbol,
            side,
            order_type,
            quantity,
            price: args["price"].as_f64(),
        })?;
        serde_json::to_string(&order).map_err(|e| format!("Serialize order failed: {e}"))
    }
}

/// List currently open positions / working orders on the account.
///
/// Read-only with respect to the market, but reads private account state. A
/// quant agent uses it to review exposure before adjusting positions.
pub struct ListPositionsTool {
    runtime: QuantRuntime,
}

impl ListPositionsTool {
    pub fn new() -> Self {
        Self::with_runtime(QuantRuntime::new())
    }

    pub fn with_runtime(runtime: QuantRuntime) -> Self {
        Self { runtime }
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
    async fn call(&self, arguments: &str) -> Result<String, String> {
        let args = serde_json::from_str::<serde_json::Value>(arguments).unwrap_or_default();
        let symbol = args["symbol"]
            .as_str()
            .map(str::trim)
            .filter(|symbol| !symbol.is_empty());
        self.runtime.sync_portfolio_market(symbol)?;
        serde_json::to_string(&self.runtime.portfolio(symbol))
            .map_err(|e| format!("Serialize portfolio failed: {e}"))
    }
}

/// Cancel a working order on the configured exchange/broker runtime.
///
/// Side-effecting and account-mutating because it changes broker order state.
/// The default paper broker only cancels open paper orders; filled or unknown
/// orders return a structured rejection.
pub struct CancelOrderTool {
    runtime: QuantRuntime,
}

impl CancelOrderTool {
    pub fn new() -> Self {
        Self::with_runtime(QuantRuntime::new())
    }

    pub fn with_runtime(runtime: QuantRuntime) -> Self {
        Self { runtime }
    }
}

impl Default for CancelOrderTool {
    fn default() -> Self {
        Self::new()
    }
}

#[async_trait]
impl Tool for CancelOrderTool {
    fn name(&self) -> &str {
        "cancel_order"
    }

    fn description(&self) -> &str {
        "Cancel a working order on the configured exchange/broker runtime. The \
         default runtime is paper trading; a live adapter must be configured \
         explicitly and user-approved because cancelling real orders can change \
         risk. Returns a structured broker decision."
    }

    fn parameters(&self) -> serde_json::Value {
        json!({
            "type": "object",
            "properties": {
                "order_id": { "type": "string", "description": "Broker order id to cancel, e.g. PAPER-000000" }
            },
            "required": ["order_id"]
        })
    }

    fn scope_target(&self, arguments: &str) -> ScopeTarget {
        let order_id = serde_json::from_str::<serde_json::Value>(arguments)
            .ok()
            .and_then(|v| v["order_id"].as_str().map(str::to_string))
            .unwrap_or_else(|| "?".to_string());
        ScopeTarget::Command(format!("trade:cancel:{order_id}"))
    }

    async fn call(&self, arguments: &str) -> Result<String, String> {
        let args: serde_json::Value =
            serde_json::from_str(arguments).map_err(|e| format!("Invalid JSON: {e}"))?;
        let order_id = args["order_id"]
            .as_str()
            .map(str::trim)
            .filter(|value| !value.is_empty())
            .ok_or("Missing 'order_id'")?
            .to_string();
        let decision = self.runtime.cancel_order(CancelOrderRequest { order_id })?;
        serde_json::to_string(&decision).map_err(|e| format!("Serialize cancel failed: {e}"))
    }
}

/// Convenience: `Arc<dyn Tool>` ready for an agent's tool list.
pub fn place_order() -> Arc<dyn Tool> {
    Arc::new(PlaceOrderTool::new())
}

/// Convenience: `Arc<dyn Tool>` ready for an agent's tool list.
pub fn cancel_order() -> Arc<dyn Tool> {
    Arc::new(CancelOrderTool::new())
}

/// Convenience: `Arc<dyn Tool>` ready for an agent's tool list.
pub fn list_positions() -> Arc<dyn Tool> {
    Arc::new(ListPositionsTool::new())
}

fn parse_side(raw: &str) -> Result<OrderSide, String> {
    match raw {
        "buy" => Ok(OrderSide::Buy),
        "sell" => Ok(OrderSide::Sell),
        other => Err(format!("Unknown side '{other}' (expected buy|sell)")),
    }
}

fn parse_order_type(raw: &str) -> Result<OrderType, String> {
    match raw {
        "market" => Ok(OrderType::Market),
        "limit" => Ok(OrderType::Limit),
        other => Err(format!(
            "Unknown order type '{other}' (expected market|limit)"
        )),
    }
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
        assert_eq!(v["decision_id"], "DECISION-000000");
        assert_eq!(v["status"], "filled_paper");
        assert_eq!(v["order"]["symbol"], "BTCUSDT");
        assert_eq!(v["order"]["side"], "buy");
        assert_eq!(v["order"]["type"], "market");
        assert_eq!(v["order"]["quantity"], 0.5);
        assert_eq!(v["order"]["fill_price"], 65032.5);
        assert_eq!(v["order"]["filled_quantity"], 0.5);
        assert!(
            v["order"]["order_id"]
                .as_str()
                .unwrap()
                .starts_with("PAPER-")
        );
        assert_eq!(v["account"]["cash"], 67483.75);
        assert!(
            v["risk_checks"]
                .as_array()
                .is_some_and(|checks| checks.iter().all(|check| check["passed"] == true))
        );
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

    #[tokio::test]
    async fn place_order_rejects_single_order_over_risk_limit() {
        let out = PlaceOrderTool::new()
            .call(r#"{"symbol":"BTCUSDT","side":"buy","type":"market","quantity":2}"#)
            .await
            .expect("risk rejection is a structured decision");
        let v: serde_json::Value = serde_json::from_str(&out).unwrap();
        assert_eq!(v["status"], "rejected_risk");
        assert_eq!(v["decision_id"], "DECISION-000000");
        assert_eq!(v["order"], serde_json::Value::Null);
        assert!(
            v["rejection_reason"]
                .as_str()
                .unwrap()
                .contains("order_notional_exceeds_limit")
        );
        assert!(
            v["risk_checks"]
                .as_array()
                .unwrap()
                .iter()
                .any(|check| check["name"] == "max_order_notional" && check["passed"] == false)
        );
    }

    #[tokio::test]
    async fn place_order_rejects_sells_without_inventory() {
        let out = PlaceOrderTool::new()
            .call(r#"{"symbol":"BTCUSDT","side":"sell","type":"market","quantity":0.1}"#)
            .await
            .expect("risk rejection is a structured decision");
        let v: serde_json::Value = serde_json::from_str(&out).unwrap();
        assert_eq!(v["status"], "rejected_risk");
        assert_eq!(v["order"], serde_json::Value::Null);
        assert!(
            v["rejection_reason"]
                .as_str()
                .unwrap()
                .contains("short_selling_disabled")
        );
        assert!(
            v["risk_checks"]
                .as_array()
                .unwrap()
                .iter()
                .any(|check| check["name"] == "inventory_available" && check["passed"] == false)
        );
    }

    #[tokio::test]
    async fn non_marketable_limit_order_stays_open_without_position_fill() {
        let runtime = crate::QuantRuntime::new();
        let place = PlaceOrderTool::with_runtime(runtime.clone());
        let cancel = CancelOrderTool::with_runtime(runtime.clone());
        let list = ListPositionsTool::with_runtime(runtime);

        let out = place
            .call(
                r#"{"symbol":"BTCUSDT","side":"buy","type":"limit","quantity":0.1,"price":64000}"#,
            )
            .await
            .expect("limit order accepted");
        let v: serde_json::Value = serde_json::from_str(&out).unwrap();
        assert_eq!(v["status"], "open_paper");
        assert_eq!(v["order"]["limit_price"], 64000.0);
        assert_eq!(v["order"]["fill_price"], serde_json::Value::Null);
        assert_eq!(v["order"]["filled_quantity"], 0.0);
        assert_eq!(v["account"]["cash"], 100000.0);

        let out = list.call(r#"{"symbol":"BTCUSDT"}"#).await.expect("list ok");
        let v: serde_json::Value = serde_json::from_str(&out).unwrap();
        assert!(v["positions"].as_array().unwrap().is_empty());
        assert_eq!(v["open_orders"][0]["status"], "open_paper");
        assert_eq!(v["order_history"][0]["status"], "open_paper");

        let out = cancel
            .call(r#"{"order_id":"PAPER-000000"}"#)
            .await
            .expect("cancel ok");
        let v: serde_json::Value = serde_json::from_str(&out).unwrap();
        assert_eq!(v["status"], "cancelled_paper");
        assert_eq!(v["order"]["status"], "cancelled_paper");

        let out = list.call(r#"{"symbol":"BTCUSDT"}"#).await.expect("list ok");
        let v: serde_json::Value = serde_json::from_str(&out).unwrap();
        assert!(v["open_orders"].as_array().unwrap().is_empty());
        assert_eq!(v["order_history"][1]["status"], "cancelled_paper");
    }

    // ---- CancelOrderTool ----

    #[test]
    fn cancel_order_name_schema_and_traits() {
        let t = CancelOrderTool::new();
        assert_eq!(t.name(), "cancel_order");
        assert!(!t.description().is_empty());
        assert!(!t.spawns_envoy());
        assert!(!t.affects_control_flow());
        let schema = t.parameters();
        assert!(schema["properties"]["order_id"].is_object());
        let required: Vec<&str> = schema["required"]
            .as_array()
            .expect("required array")
            .iter()
            .filter_map(|v| v.as_str())
            .collect();
        assert_eq!(required, ["order_id"]);
    }

    #[test]
    fn cancel_order_scope_target_classifies_as_command() {
        let t = CancelOrderTool::new();
        let target = t.scope_target(r#"{"order_id":"PAPER-000000"}"#);
        assert!(matches!(target, ScopeTarget::Command(ref c) if c == "trade:cancel:PAPER-000000"));
        let target = t.scope_target("not json");
        assert!(matches!(target, ScopeTarget::Command(ref c) if c == "trade:cancel:?"));
    }

    #[tokio::test]
    async fn cancel_order_rejects_missing_order_id() {
        let err = CancelOrderTool::new()
            .call(r#"{"order_id":""}"#)
            .await
            .expect_err("missing order id");
        assert!(err.contains("order_id"), "err: {err}");
    }

    #[tokio::test]
    async fn cancel_order_rejects_unknown_or_already_cancelled_order() {
        let runtime = crate::QuantRuntime::new();
        let place = PlaceOrderTool::with_runtime(runtime.clone());
        let cancel = CancelOrderTool::with_runtime(runtime);

        place
            .call(
                r#"{"symbol":"BTCUSDT","side":"buy","type":"limit","quantity":0.1,"price":64000}"#,
            )
            .await
            .expect("limit order accepted");
        cancel
            .call(r#"{"order_id":"PAPER-000000"}"#)
            .await
            .expect("first cancel ok");
        let out = cancel
            .call(r#"{"order_id":"PAPER-000000"}"#)
            .await
            .expect("cancel rejection is structured");
        let v: serde_json::Value = serde_json::from_str(&out).unwrap();
        assert_eq!(v["status"], "rejected_cancel");
        assert!(
            v["rejection_reason"]
                .as_str()
                .unwrap()
                .contains("not found")
        );
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
        assert!(v["order_history"].as_array().unwrap().is_empty());
        assert_eq!(v["account"]["cash"], 100000.0);
        assert_eq!(v["risk_limits"]["allow_short_selling"], false);
    }

    #[tokio::test]
    async fn list_positions_call_ignores_arguments() {
        // The paper adapter accepts filters even when the account is empty.
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
        assert!(out.contains("positions"));
    }

    #[tokio::test]
    async fn list_positions_empty_symbol_means_no_filter() {
        let runtime = crate::QuantRuntime::new();
        let place = PlaceOrderTool::with_runtime(runtime.clone());
        let list = ListPositionsTool::with_runtime(runtime);

        place
            .call(r#"{"symbol":"AAPL","side":"buy","type":"market","quantity":1}"#)
            .await
            .expect("place aapl");
        place
            .call(r#"{"symbol":"MSFT","side":"buy","type":"market","quantity":1}"#)
            .await
            .expect("place msft");

        let out = list.call(r#"{"symbol":"   "}"#).await.expect("list ok");
        let v: serde_json::Value = serde_json::from_str(&out).unwrap();
        assert_eq!(v["positions"].as_array().unwrap().len(), 2);
    }

    #[tokio::test]
    async fn place_order_updates_shared_paper_positions() {
        let runtime = crate::QuantRuntime::new();
        let place = PlaceOrderTool::with_runtime(runtime.clone());
        let list = ListPositionsTool::with_runtime(runtime);

        place
            .call(r#"{"symbol":"BTCUSDT","side":"buy","type":"market","quantity":0.5}"#)
            .await
            .expect("place ok");
        let out = list.call(r#"{"symbol":"BTCUSDT"}"#).await.expect("list ok");
        let v: serde_json::Value = serde_json::from_str(&out).unwrap();
        assert_eq!(v["positions"][0]["symbol"], "BTCUSDT");
        assert_eq!(v["positions"][0]["quantity"], 0.5);
        assert_eq!(v["order_history"][0]["status"], "filled_paper");
        assert_eq!(v["order_history"][0]["decision_id"], "DECISION-000000");
    }

    // ---- convenience accessors ----

    #[test]
    fn accessors_wrap_fresh_instances() {
        assert_eq!(place_order().name(), "place_order");
        assert_eq!(cancel_order().name(), "cancel_order");
        assert_eq!(list_positions().name(), "list_positions");
    }
}
