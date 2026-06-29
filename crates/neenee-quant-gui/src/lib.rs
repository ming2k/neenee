#![cfg_attr(test, allow(clippy::unwrap_used, clippy::expect_used))]

use neenee_core::Tool;
use serde_json::{Value, json};
use tokio::runtime::{Builder, Runtime};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum View {
    Market,
    Backtest,
    Portfolio,
    Orders,
    Config,
}

impl View {
    pub fn label(self) -> &'static str {
        match self {
            View::Market => "Market",
            View::Backtest => "Backtest",
            View::Portfolio => "Portfolio",
            View::Orders => "Orders",
            View::Config => "Config",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TradingMode {
    Paper,
    TradingArmed,
}

impl TradingMode {
    pub fn label(self) -> &'static str {
        match self {
            TradingMode::Paper => "Paper",
            TradingMode::TradingArmed => "Trading armed",
        }
    }
}

pub struct AppState {
    runtime: Runtime,
    market_data: neenee_quant::MarketDataTool,
    backtest: neenee_quant::BacktestTool,
    place_order: neenee_quant::PlaceOrderTool,
    cancel_order: neenee_quant::CancelOrderTool,
    list_positions: neenee_quant::ListPositionsTool,
    pub view: View,
    pub mode: TradingMode,
    pub market_kind: i32,
    pub order_side: i32,
    pub order_type: i32,
    pub last_action: String,
    pub last_result: String,
    pub risk_status: String,
    pub account_summary: String,
    pub positions_summary: String,
    pub open_orders_summary: String,
    pub recent_order_summary: String,
    pub market_data_source: String,
    pub config_summary: String,
    pub config: neenee_quant::QuantConfig,
    pub symbol: String,
    pub interval: String,
    pub strategy: String,
    pub start: String,
    pub end: String,
    pub capital: String,
    pub quantity: String,
    pub price: String,
    pub order_id: String,
}

impl AppState {
    pub fn new() -> Result<Self, std::io::Error> {
        Self::from_config(neenee_quant::QuantConfig::default())
    }

    pub fn from_environment() -> Result<Self, std::io::Error> {
        let config =
            neenee_quant::QuantConfig::from_environment().map_err(std::io::Error::other)?;
        Self::from_config(config)
    }

    pub fn from_config(config: neenee_quant::QuantConfig) -> Result<Self, std::io::Error> {
        let market_data_source = config.market_data_source_label().to_string();
        let config_summary = config.summary();
        let runtime = config.build_runtime().map_err(std::io::Error::other)?;
        Self::with_runtime(runtime, market_data_source, config_summary, config)
    }

    pub fn with_runtime(
        runtime: neenee_quant::QuantRuntime,
        market_data_source: impl Into<String>,
        config_summary: impl Into<String>,
        config: neenee_quant::QuantConfig,
    ) -> Result<Self, std::io::Error> {
        let market_data_source = market_data_source.into();
        let config_summary = config_summary.into();
        let starting_cash = config.paper.starting_cash;
        let (risk_status, account_summary) = if config.broker.mode == "live-http" {
            (
                "Live account: refresh positions before trading".to_string(),
                "Live account: pending refresh".to_string(),
            )
        } else {
            (
                format!(
                    "Paper account: cash {starting_cash:.2}, available {starting_cash:.2}, equity {starting_cash:.2}"
                ),
                format!(
                    "Cash {starting_cash:.2} · Available {starting_cash:.2} · Equity {starting_cash:.2}"
                ),
            )
        };
        Ok(Self {
            runtime: Builder::new_current_thread().enable_all().build()?,
            market_data: neenee_quant::MarketDataTool::with_runtime(runtime.clone()),
            backtest: neenee_quant::BacktestTool::with_runtime(runtime.clone()),
            place_order: neenee_quant::PlaceOrderTool::with_runtime(runtime.clone()),
            cancel_order: neenee_quant::CancelOrderTool::with_runtime(runtime.clone()),
            list_positions: neenee_quant::ListPositionsTool::with_runtime(runtime),
            view: View::Market,
            mode: TradingMode::Paper,
            market_kind: 0,
            order_side: 0,
            order_type: 0,
            last_action: "Ready".to_string(),
            last_result: format!(
                "Select a workspace action to call a quant tool. Market data source: {market_data_source}."
            ),
            risk_status,
            account_summary,
            positions_summary: "Positions: 0".to_string(),
            open_orders_summary: "Open orders: 0 · Reserved buy 0.00 · Reserved sell 0.00"
                .to_string(),
            recent_order_summary: "Recent order: none".to_string(),
            market_data_source,
            config_summary,
            config,
            symbol: "BTCUSDT".to_string(),
            interval: "1h".to_string(),
            strategy: "sma_cross(50,200)".to_string(),
            start: "2024-01-01".to_string(),
            end: "2024-12-31".to_string(),
            capital: "100000".to_string(),
            quantity: "0.1".to_string(),
            price: String::new(),
            order_id: "PAPER-000000".to_string(),
        })
    }

    pub fn set_view(&mut self, view: View) {
        self.view = view;
    }

    pub fn toggle_mode(&mut self) {
        self.mode = match self.mode {
            TradingMode::Paper => TradingMode::TradingArmed,
            TradingMode::TradingArmed => TradingMode::Paper,
        };
    }

    pub fn fetch_market_data(&mut self) {
        let symbol = match parse_symbol(&self.symbol) {
            Ok(symbol) => symbol,
            Err(err) => {
                self.record_input_error("market_data", err);
                return;
            }
        };
        let kinds = ["quote", "klines", "depth"];
        let kind = select(&kinds, self.market_kind);
        let args = json!({
            "symbol": symbol,
            "kind": kind,
            "interval": self.interval.trim(),
            "limit": 100,
        });
        self.record_tool_result(
            "market_data",
            run_tool(&self.runtime, &self.market_data, args),
        );
    }

    pub fn run_backtest(&mut self) {
        let symbol = match parse_symbol(&self.symbol) {
            Ok(symbol) => symbol,
            Err(err) => {
                self.record_input_error("backtest", err);
                return;
            }
        };
        let initial_capital = match parse_positive_f64("initial_capital", &self.capital) {
            Ok(value) => value,
            Err(err) => {
                self.record_input_error("backtest", err);
                return;
            }
        };
        let args = json!({
            "symbol": symbol,
            "strategy": self.strategy.as_str(),
            "start": self.start.as_str(),
            "end": self.end.as_str(),
            "interval": self.interval.as_str(),
            "initial_capital": initial_capital,
        });
        self.record_tool_result("backtest", run_tool(&self.runtime, &self.backtest, args));
    }

    pub fn refresh_positions(&mut self) {
        let args = json!({
            "symbol": self.symbol_value(),
        });
        self.record_tool_result(
            "list_positions",
            run_tool(&self.runtime, &self.list_positions, args),
        );
    }

    pub fn submit_order(&mut self) {
        if self.mode != TradingMode::TradingArmed {
            self.last_action = "Order blocked".to_string();
            self.last_result =
                "Switch to Trading armed before sending an account-mutating order.".to_string();
            return;
        }

        let symbol = match parse_symbol(&self.symbol) {
            Ok(symbol) => symbol,
            Err(err) => {
                self.record_input_error("place_order", err);
                return;
            }
        };
        let sides = ["buy", "sell"];
        let order_types = ["market", "limit"];
        let side = select(&sides, self.order_side);
        let order_type = select(&order_types, self.order_type);
        let quantity = match parse_positive_f64("quantity", &self.quantity) {
            Ok(value) => value,
            Err(err) => {
                self.record_input_error("place_order", err);
                return;
            }
        };
        let mut args = json!({
            "symbol": symbol,
            "side": side,
            "type": order_type,
            "quantity": quantity,
        });
        if order_type == "limit" {
            let price = match parse_positive_f64("limit price", &self.price) {
                Ok(value) => value,
                Err(err) => {
                    self.record_input_error("place_order", err);
                    return;
                }
            };
            args["price"] = json!(price);
        }
        self.record_tool_result(
            "place_order",
            run_tool(&self.runtime, &self.place_order, args),
        );
        if let Some(order_id) = order_id_from_result(&self.last_result) {
            self.order_id = order_id;
        }
        self.refresh_portfolio_summary_silent();
    }

    pub fn cancel_order(&mut self) {
        if self.mode != TradingMode::TradingArmed {
            self.last_action = "Cancel blocked".to_string();
            self.last_result =
                "Switch to Trading armed before cancelling an account-mutating order.".to_string();
            return;
        }
        let order_id = self.order_id.trim();
        if order_id.is_empty() {
            self.record_input_error("cancel_order", "order_id is required".to_string());
            return;
        }
        let args = json!({ "order_id": order_id });
        self.record_tool_result(
            "cancel_order",
            run_tool(&self.runtime, &self.cancel_order, args),
        );
        self.refresh_portfolio_summary_silent();
    }

    pub fn symbol_value(&self) -> String {
        self.symbol.trim().to_uppercase()
    }

    fn record_tool_result(&mut self, action: &str, result: String) {
        self.last_action = action.to_string();
        if action == "list_positions"
            && let Some(error) = tool_error(&result)
        {
            self.risk_status = format!("Portfolio refresh failed: {error}");
            self.account_summary = "Account: refresh failed".to_string();
            self.positions_summary = "Positions: refresh failed".to_string();
            self.open_orders_summary = "Open orders: refresh failed".to_string();
            self.last_result = result;
            return;
        }
        if let Some(status) = account_status(&result) {
            self.risk_status = status;
        }
        if let Some(summary) = account_summary(&result) {
            self.account_summary = summary;
        }
        if let Some(summary) = positions_summary(&result) {
            self.positions_summary = summary;
        }
        if let Some(summary) = open_orders_summary(&result) {
            self.open_orders_summary = summary;
        }
        if let Some(summary) = recent_order_summary(&result) {
            self.recent_order_summary = summary;
        }
        self.last_result = result;
    }

    fn record_input_error(&mut self, action: &str, error: String) {
        self.last_action = format!("{action} input error");
        self.last_result = format!("Input error: {error}");
    }

    fn refresh_portfolio_summary_silent(&mut self) {
        let args = json!({ "symbol": self.symbol_value() });
        let result = run_tool(&self.runtime, &self.list_positions, args);
        if let Some(error) = tool_error(&result) {
            self.risk_status = format!("Portfolio refresh failed: {error}");
            self.account_summary = "Account: refresh failed".to_string();
            self.positions_summary = "Positions: refresh failed".to_string();
            self.open_orders_summary = "Open orders: refresh failed".to_string();
            return;
        }
        if let Some(status) = account_status(&result) {
            self.risk_status = status;
        }
        if let Some(summary) = account_summary(&result) {
            self.account_summary = summary;
        }
        if let Some(summary) = positions_summary(&result) {
            self.positions_summary = summary;
        }
        if let Some(summary) = open_orders_summary(&result) {
            self.open_orders_summary = summary;
        }
    }
}

fn run_tool(runtime: &Runtime, tool: &dyn Tool, args: Value) -> String {
    let input = args.to_string();
    match runtime.block_on(tool.call(&input)) {
        Ok(raw) => pretty_json(&raw),
        Err(err) => format!("Error: {err}"),
    }
}

fn parse_positive_f64(field: &str, text: &str) -> Result<f64, String> {
    let trimmed = text.trim();
    if trimmed.is_empty() {
        return Err(format!("{field} is required"));
    }
    let value = trimmed
        .parse::<f64>()
        .map_err(|e| format!("{field} must be numeric: {e}"))?;
    if value <= 0.0 || !value.is_finite() {
        Err(format!("{field} must be a positive finite number"))
    } else {
        Ok(value)
    }
}

fn parse_symbol(text: &str) -> Result<String, String> {
    let symbol = text.trim().to_uppercase();
    if symbol.is_empty() {
        Err("symbol is required".to_string())
    } else {
        Ok(symbol)
    }
}

fn pretty_json(raw: &str) -> String {
    match serde_json::from_str::<Value>(raw) {
        Ok(value) => serde_json::to_string_pretty(&value).unwrap_or_else(|_| raw.to_string()),
        Err(_) => raw.to_string(),
    }
}

fn tool_error(raw: &str) -> Option<&str> {
    raw.strip_prefix("Error: ").map(str::trim)
}

fn account_status(raw: &str) -> Option<String> {
    let value = serde_json::from_str::<Value>(raw).ok()?;
    let account = value.get("account")?;
    let cash = account.get("cash")?.as_f64()?;
    let available_cash = account
        .get("available_cash")
        .and_then(Value::as_f64)
        .unwrap_or(cash);
    let equity = account.get("equity")?.as_f64()?;
    let realized = account
        .get("realized_pnl")
        .and_then(Value::as_f64)
        .unwrap_or_default();
    let total_commission = account
        .get("total_commission")
        .and_then(Value::as_f64)
        .unwrap_or_default();
    let net_pnl = account
        .get("net_pnl")
        .and_then(Value::as_f64)
        .unwrap_or(realized - total_commission);
    let buying_power = account.get("buying_power")?.as_f64()?;
    let gross_exposure = account.get("gross_exposure")?.as_f64()?;
    let projected_gross = account
        .get("projected_gross_exposure")
        .and_then(Value::as_f64)
        .unwrap_or(gross_exposure);
    let reserved_buy = account
        .get("reserved_buy_notional")
        .and_then(Value::as_f64)
        .unwrap_or_default();
    Some(format!(
        "Paper account: cash {cash:.2}, available {available_cash:.2}, equity {equity:.2}, realized {realized:.2}, fees {total_commission:.2}, net {net_pnl:.2}, buying power {buying_power:.2}, reserved buy {reserved_buy:.2}, gross exposure {gross_exposure:.2}, projected {projected_gross:.2}"
    ))
}

fn account_summary(raw: &str) -> Option<String> {
    let value = serde_json::from_str::<Value>(raw).ok()?;
    let account = value.get("account")?;
    let cash = account.get("cash")?.as_f64()?;
    let available_cash = account
        .get("available_cash")
        .and_then(Value::as_f64)
        .unwrap_or(cash);
    let equity = account.get("equity")?.as_f64()?;
    let realized = account
        .get("realized_pnl")
        .and_then(Value::as_f64)
        .unwrap_or_default();
    let total_commission = account
        .get("total_commission")
        .and_then(Value::as_f64)
        .unwrap_or_default();
    let net_pnl = account
        .get("net_pnl")
        .and_then(Value::as_f64)
        .unwrap_or(realized - total_commission);
    let buying_power = account.get("buying_power")?.as_f64()?;
    let reserved_buy = account
        .get("reserved_buy_notional")
        .and_then(Value::as_f64)
        .unwrap_or_default();
    Some(format!(
        "Cash {cash:.2} · Available {available_cash:.2} · Equity {equity:.2} · Realized {realized:.2} · Fees {total_commission:.2} · Net {net_pnl:.2} · Buying power {buying_power:.2} · Reserved buy {reserved_buy:.2}"
    ))
}

fn positions_summary(raw: &str) -> Option<String> {
    let value = serde_json::from_str::<Value>(raw).ok()?;
    let positions = value.get("positions")?.as_array()?;
    let gross = value
        .get("account")
        .and_then(|account| account.get("gross_exposure"))
        .and_then(Value::as_f64)
        .unwrap_or_default();
    Some(format!(
        "Positions: {} · Gross exposure {gross:.2}",
        positions.len()
    ))
}

fn open_orders_summary(raw: &str) -> Option<String> {
    let value = serde_json::from_str::<Value>(raw).ok()?;
    let open_orders = value.get("open_orders")?.as_array()?;
    let reserved_buy = value
        .get("account")
        .and_then(|account| account.get("reserved_buy_notional"))
        .and_then(Value::as_f64)
        .unwrap_or_default();
    let reserved_sell = value
        .get("account")
        .and_then(|account| account.get("reserved_sell_notional"))
        .and_then(Value::as_f64)
        .unwrap_or_default();
    Some(format!(
        "Open orders: {} · Reserved buy {reserved_buy:.2} · Reserved sell {reserved_sell:.2}",
        open_orders.len()
    ))
}

fn recent_order_summary(raw: &str) -> Option<String> {
    let value = serde_json::from_str::<Value>(raw).ok()?;
    let decision = if value.get("status").is_some() {
        &value
    } else {
        value
            .get("order_history")?
            .as_array()?
            .iter()
            .rev()
            .find(|decision| decision.get("status").is_some())?
    };
    let status = decision.get("status")?.as_str()?;
    let order_id = decision
        .get("order")
        .and_then(|order| order.get("order_id"))
        .and_then(Value::as_str)
        .unwrap_or("-");
    Some(format!("Recent order: {status} · {order_id}"))
}

fn order_id_from_result(raw: &str) -> Option<String> {
    let value = serde_json::from_str::<Value>(raw).ok()?;
    value
        .get("order")
        .and_then(|order| order.get("order_id"))
        .and_then(Value::as_str)
        .map(str::to_string)
}

fn select<'a>(items: &'a [&str], index: i32) -> &'a str {
    items
        .get(index.max(0) as usize)
        .copied()
        .unwrap_or(items[0])
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
    };

    #[derive(Clone, Default)]
    struct SwitchableMarketData {
        fail_quotes: Arc<AtomicBool>,
    }

    impl SwitchableMarketData {
        fn fail_quotes(&self) {
            self.fail_quotes.store(true, Ordering::Relaxed);
        }
    }

    impl neenee_quant::MarketDataAdapter for SwitchableMarketData {
        fn quote(
            &self,
            symbol: &str,
        ) -> neenee_quant::runtime::MarketDataResult<neenee_quant::runtime::Quote> {
            if self.fail_quotes.load(Ordering::Relaxed) {
                return Err("quote unavailable".to_string());
            }
            Ok(neenee_quant::runtime::Quote {
                symbol: symbol.trim().to_uppercase(),
                price: 100.0,
                bid: 99.5,
                ask: 100.5,
                timestamp_ms: 1,
                source: "switchable-test".to_string(),
            })
        }

        fn candles(
            &self,
            symbol: &str,
            interval: &str,
            _limit: usize,
        ) -> neenee_quant::runtime::MarketDataResult<Vec<neenee_quant::runtime::Candle>> {
            Ok(vec![neenee_quant::runtime::Candle {
                symbol: symbol.trim().to_uppercase(),
                interval: interval.to_string(),
                open: 100.0,
                high: 101.0,
                low: 99.0,
                close: 100.0,
                volume: 1.0,
                index: 0,
                source: "switchable-test".to_string(),
            }])
        }

        fn depth(
            &self,
            symbol: &str,
            _limit: usize,
        ) -> neenee_quant::runtime::MarketDataResult<neenee_quant::runtime::OrderBook> {
            Ok(neenee_quant::runtime::OrderBook {
                symbol: symbol.trim().to_uppercase(),
                bids: vec![neenee_quant::runtime::BookLevel {
                    price: 99.5,
                    quantity: 1.0,
                }],
                asks: vec![neenee_quant::runtime::BookLevel {
                    price: 100.5,
                    quantity: 1.0,
                }],
                timestamp_ms: 1,
                source: "switchable-test".to_string(),
            })
        }
    }

    #[test]
    fn market_action_calls_quant_tool() {
        let mut state = AppState::new().expect("state");
        assert_eq!(state.market_data_source, "synthetic-paper");
        state.fetch_market_data();
        assert_eq!(state.last_action, "market_data");
        assert!(state.last_result.contains("\"symbol\": \"BTCUSDT\""));
        assert!(
            state
                .last_result
                .contains("\"source\": \"synthetic-paper\"")
        );
    }

    #[test]
    fn config_state_reflects_loaded_config() {
        let state_path = std::env::temp_dir().join(format!(
            "neenee-quant-gui-state-{}.json",
            std::process::id()
        ));
        let _ = std::fs::remove_file(&state_path);
        let mut config = neenee_quant::QuantConfig::default();
        config.market_data.source = "binance".to_string();
        config.market_data.binance_base_url = "https://example.test".to_string();
        config.paper.starting_cash = 25_000.0;
        config.paper.commission_bps = 6.5;
        config.paper.state_path = Some(state_path.clone());
        config.paper.audit_log = Some(std::path::PathBuf::from("/tmp/audit.jsonl"));
        config.paper.risk.max_order_notional = 123.0;
        config.paper.risk.max_gross_exposure = 456.0;
        config.paper.risk.allow_short_selling = true;

        let state = AppState::from_config(config).expect("state");

        assert_eq!(state.market_data_source, "binance-http");
        assert_eq!(
            state.config.market_data.binance_base_url,
            "https://example.test"
        );
        assert_eq!(state.config.paper.starting_cash, 25_000.0);
        assert_eq!(state.config.paper.commission_bps, 6.5);
        assert_eq!(
            state.config.paper.state_path.as_deref(),
            Some(state_path.as_path())
        );
        assert!(state.risk_status.contains("cash 25000.00"));
        assert_eq!(state.config.paper.risk.max_order_notional, 123.0);
        assert!(state.config_summary.contains("paper_cash=25000"));
        assert!(state.config_summary.contains("commission_bps=6.5"));
        assert!(
            state
                .config_summary
                .contains(&format!("state={}", state_path.display()))
        );
        assert!(state.config_summary.contains("audit=/tmp/audit.jsonl"));
        let _ = std::fs::remove_file(&state_path);
    }

    #[test]
    fn live_http_config_initial_state_is_not_labeled_paper() {
        let mut config = neenee_quant::QuantConfig::default();
        config.broker.mode = "live-http".to_string();
        config.broker.live_http.base_url = "https://broker.test".to_string();
        config.broker.live_http.token = Some("secret-token".to_string());

        let state = AppState::from_config(config).expect("state");

        assert!(state.risk_status.contains("Live account"));
        assert_eq!(state.account_summary, "Live account: pending refresh");
        assert!(state.config_summary.contains("broker=live-http"));
    }

    #[test]
    fn backtest_action_calls_quant_tool() {
        let mut state = AppState::new().expect("state");
        state.run_backtest();
        assert_eq!(state.last_action, "backtest");
        assert!(
            state
                .last_result
                .contains("\"strategy\": \"sma_cross(50,200)\"")
        );
    }

    #[test]
    fn backtest_rejects_invalid_capital_before_tool_call() {
        let mut state = AppState::new().expect("state");
        state.capital = "not-money".to_string();

        state.run_backtest();

        assert_eq!(state.last_action, "backtest input error");
        assert!(
            state
                .last_result
                .contains("initial_capital must be numeric")
        );
    }

    #[test]
    fn symbol_is_required_for_market_backtest_and_orders() {
        let mut state = AppState::new().expect("state");
        state.symbol = "  ".to_string();

        state.fetch_market_data();
        assert_eq!(state.last_action, "market_data input error");
        assert!(state.last_result.contains("symbol is required"));

        state.run_backtest();
        assert_eq!(state.last_action, "backtest input error");
        assert!(state.last_result.contains("symbol is required"));

        state.toggle_mode();
        state.submit_order();
        assert_eq!(state.last_action, "place_order input error");
        assert!(state.last_result.contains("symbol is required"));
    }

    #[test]
    fn order_is_blocked_until_trading_armed() {
        let mut state = AppState::new().expect("state");
        state.submit_order();
        assert_eq!(state.last_action, "Order blocked");
        assert!(state.last_result.contains("Trading armed"));
    }

    #[test]
    fn trading_armed_order_calls_quant_tool() {
        let mut state = AppState::new().expect("state");
        state.order_id = "STALE".to_string();
        state.toggle_mode();
        state.submit_order();
        assert_eq!(state.last_action, "place_order");
        assert!(
            state
                .last_result
                .contains("\"decision_id\": \"DECISION-000000\"")
        );
        assert!(state.last_result.contains("\"risk_checks\""));
        assert!(state.last_result.contains("\"order_id\": \"PAPER-000000\""));
        assert!(state.risk_status.contains("cash 93496.75"));
        assert!(state.account_summary.contains("Cash 93496.75"));
        assert!(state.positions_summary.contains("Positions: 1"));
        assert!(state.open_orders_summary.contains("Open orders: 0"));
        assert_eq!(state.order_id, "PAPER-000000");
        assert!(
            state
                .recent_order_summary
                .contains("filled_paper · PAPER-000000")
        );
    }

    #[test]
    fn trading_order_rejects_invalid_quantity_before_tool_call() {
        let mut state = AppState::new().expect("state");
        state.quantity = "lots".to_string();
        state.toggle_mode();

        state.submit_order();

        assert_eq!(state.last_action, "place_order input error");
        assert!(state.last_result.contains("quantity must be numeric"));
    }

    #[test]
    fn limit_order_requires_positive_price_before_tool_call() {
        let mut state = AppState::new().expect("state");
        state.order_type = 1;
        state.price = String::new();
        state.toggle_mode();

        state.submit_order();

        assert_eq!(state.last_action, "place_order input error");
        assert!(state.last_result.contains("limit price is required"));
    }

    #[test]
    fn cancel_order_is_blocked_until_trading_armed() {
        let mut state = AppState::new().expect("state");

        state.cancel_order();

        assert_eq!(state.last_action, "Cancel blocked");
        assert!(state.last_result.contains("Trading armed"));
    }

    #[test]
    fn cancel_order_requires_order_id_before_tool_call() {
        let mut state = AppState::new().expect("state");
        state.order_id = String::new();
        state.toggle_mode();

        state.cancel_order();

        assert_eq!(state.last_action, "cancel_order input error");
        assert!(state.last_result.contains("order_id is required"));
    }

    #[test]
    fn cancel_order_calls_quant_tool_for_open_order() {
        let mut state = AppState::new().expect("state");
        state.order_type = 1;
        state.price = "64000".to_string();
        state.toggle_mode();

        state.submit_order();
        assert_eq!(state.last_action, "place_order");
        assert!(state.last_result.contains("\"status\": \"open_paper\""));
        assert!(state.open_orders_summary.contains("Open orders: 1"));
        assert!(state.open_orders_summary.contains("Reserved buy 6400.00"));

        state.cancel_order();

        assert_eq!(state.last_action, "cancel_order");
        assert!(
            state
                .last_result
                .contains("\"status\": \"cancelled_paper\"")
        );
        assert!(state.last_result.contains("\"order_id\": \"PAPER-000000\""));
        assert!(state.open_orders_summary.contains("Open orders: 0"));
        assert!(
            state
                .recent_order_summary
                .contains("cancelled_paper · PAPER-000000")
        );
    }

    #[test]
    fn paper_order_is_visible_in_positions() {
        let mut state = AppState::new().expect("state");
        state.toggle_mode();
        state.submit_order();
        state.refresh_positions();
        assert_eq!(state.last_action, "list_positions");
        assert!(state.last_result.contains("\"positions\""));
        assert!(state.last_result.contains("\"quantity\": 0.1"));
        assert!(state.risk_status.contains("gross exposure 6500.00"));
    }

    #[test]
    fn recent_order_summary_reads_portfolio_order_history() {
        let raw = json!({
            "positions": [],
            "open_orders": [],
            "order_history": [
                {
                    "status": "open_paper",
                    "order": { "order_id": "PAPER-000000" }
                },
                {
                    "status": "filled_paper",
                    "order": { "order_id": "PAPER-000000" }
                }
            ]
        })
        .to_string();

        assert_eq!(
            recent_order_summary(&raw).as_deref(),
            Some("Recent order: filled_paper · PAPER-000000")
        );
    }

    #[test]
    fn summaries_include_account_reservations_when_present() {
        let raw = json!({
            "positions": [],
            "open_orders": [{ "order_id": "PAPER-000000" }],
            "account": {
                "cash": 100000.0,
                "available_cash": 93600.0,
                "equity": 100000.0,
                "realized_pnl": 125.5,
                "total_commission": 12.0,
                "net_pnl": 113.5,
                "gross_exposure": 0.0,
                "projected_gross_exposure": 6400.0,
                "reserved_buy_notional": 6400.0,
                "reserved_sell_notional": 0.0,
                "buying_power": 93600.0
            }
        })
        .to_string();

        assert!(
            account_status(&raw)
                .unwrap()
                .contains("reserved buy 6400.00")
        );
        assert!(
            account_summary(&raw)
                .unwrap()
                .contains("Available 93600.00")
        );
        assert!(account_summary(&raw).unwrap().contains("Realized 125.50"));
        assert!(account_summary(&raw).unwrap().contains("Fees 12.00"));
        assert!(account_summary(&raw).unwrap().contains("Net 113.50"));
        assert!(
            open_orders_summary(&raw)
                .unwrap()
                .contains("Reserved buy 6400.00")
        );
    }

    #[test]
    fn portfolio_refresh_error_marks_summaries_as_failed() {
        let market_data = SwitchableMarketData::default();
        let runtime = neenee_quant::QuantRuntime::with_adapters(
            Arc::new(market_data.clone()),
            Arc::new(neenee_quant::PaperBroker::default()),
        );
        let mut state = AppState::with_runtime(
            runtime,
            "switchable-test",
            "market=switchable-test",
            neenee_quant::QuantConfig::default(),
        )
        .expect("state");
        state.symbol = "AAPL".to_string();
        state.toggle_mode();
        state.submit_order();
        assert!(state.positions_summary.contains("Positions: 1"));

        market_data.fail_quotes();
        state.refresh_positions();

        assert_eq!(state.last_action, "list_positions");
        assert!(state.last_result.contains("Error: quote unavailable"));
        assert_eq!(
            state.risk_status,
            "Portfolio refresh failed: quote unavailable"
        );
        assert_eq!(state.account_summary, "Account: refresh failed");
        assert_eq!(state.positions_summary, "Positions: refresh failed");
        assert_eq!(state.open_orders_summary, "Open orders: refresh failed");
    }

    #[test]
    fn risk_rejection_is_visible_in_result_and_preserves_account_status() {
        let mut state = AppState::new().expect("state");
        state.quantity = "2".to_string();
        state.toggle_mode();
        state.submit_order();
        assert_eq!(state.last_action, "place_order");
        assert!(state.last_result.contains("\"status\": \"rejected_risk\""));
        assert!(
            state
                .last_result
                .contains("\"decision_id\": \"DECISION-000000\"")
        );
        assert!(state.last_result.contains("\"risk_checks\""));
        assert!(state.last_result.contains("order_notional_exceeds_limit"));
        assert!(state.risk_status.contains("cash 100000.00"));
    }
}
