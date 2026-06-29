use std::collections::{BTreeMap, BTreeSet};
use std::fs::OpenOptions;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::Duration;
use std::time::{SystemTime, UNIX_EPOCH};

use serde::{Deserialize, Serialize};
use serde_json::Value;

pub type MarketDataResult<T> = Result<T, String>;

#[derive(Clone)]
pub struct QuantRuntime {
    inner: Arc<RuntimeInner>,
}

struct RuntimeInner {
    market_data: Arc<dyn MarketDataAdapter>,
    broker: Arc<dyn BrokerAdapter>,
}

impl QuantRuntime {
    pub fn new() -> Self {
        Self::with_adapters(
            Arc::new(SyntheticMarketData::default()),
            Arc::new(PaperBroker::default()),
        )
    }

    pub fn with_adapters(
        market_data: Arc<dyn MarketDataAdapter>,
        broker: Arc<dyn BrokerAdapter>,
    ) -> Self {
        Self {
            inner: Arc::new(RuntimeInner {
                market_data,
                broker,
            }),
        }
    }

    pub fn binance_paper() -> Self {
        Self::with_adapters(
            Arc::new(BinanceMarketData::new()),
            Arc::new(PaperBroker::default()),
        )
    }

    pub fn quote(&self, symbol: &str) -> MarketDataResult<Quote> {
        self.inner.market_data.quote(symbol)
    }

    pub fn candles(
        &self,
        symbol: &str,
        interval: &str,
        limit: usize,
    ) -> MarketDataResult<Vec<Candle>> {
        self.inner.market_data.candles(symbol, interval, limit)
    }

    pub fn depth(&self, symbol: &str, limit: usize) -> MarketDataResult<OrderBook> {
        self.inner.market_data.depth(symbol, limit)
    }

    pub fn run_backtest(
        &self,
        symbol: &str,
        strategy: &str,
        initial_capital: f64,
    ) -> MarketDataResult<BacktestReport> {
        self.run_backtest_with_options(
            symbol,
            strategy,
            initial_capital,
            BacktestOptions::default(),
        )
    }

    pub fn run_backtest_with_options(
        &self,
        symbol: &str,
        strategy: &str,
        initial_capital: f64,
        options: BacktestOptions,
    ) -> MarketDataResult<BacktestReport> {
        let capital = if initial_capital.is_finite() && initial_capital > 0.0 {
            initial_capital
        } else {
            100_000.0
        };
        let interval = non_empty_or_default(&options.interval, "1d");
        let limit = options.limit.clamp(1, 1000);
        let candles = self.candles(symbol, &interval, limit)?;
        simulate_backtest(symbol, strategy, capital, options, candles)
    }

    pub fn place_order(&self, req: OrderRequest) -> MarketDataResult<OrderDecision> {
        let quote = self.quote(&req.symbol)?;
        Ok(self.inner.broker.place_order(req, quote))
    }

    pub fn cancel_order(&self, req: CancelOrderRequest) -> MarketDataResult<OrderDecision> {
        Ok(self.inner.broker.cancel_order(req))
    }

    pub fn sync_symbol(&self, symbol: &str) -> MarketDataResult<Vec<OrderDecision>> {
        let quote = self.quote(symbol)?;
        validate_quote(&quote)?;
        Ok(self.inner.broker.apply_quote(quote))
    }

    pub fn sync_portfolio_market(
        &self,
        symbol: Option<&str>,
    ) -> MarketDataResult<Vec<OrderDecision>> {
        let symbols = match symbol.map(str::trim).filter(|s| !s.is_empty()) {
            Some(symbol) => {
                let symbol = normalize_symbol(symbol);
                let portfolio = self.portfolio(Some(&symbol));
                if portfolio.positions.is_empty() && portfolio.open_orders.is_empty() {
                    Vec::new()
                } else {
                    vec![symbol]
                }
            }
            None => {
                let portfolio = self.portfolio(None);
                let mut symbols = BTreeSet::new();
                for position in portfolio.positions {
                    symbols.insert(position.symbol);
                }
                for order in portfolio.open_orders {
                    symbols.insert(order.symbol);
                }
                symbols.into_iter().collect()
            }
        };

        let mut decisions = Vec::new();
        for symbol in symbols {
            decisions.extend(self.sync_symbol(&symbol)?);
        }
        Ok(decisions)
    }

    pub fn portfolio(&self, symbol: Option<&str>) -> PortfolioSnapshot {
        self.inner.broker.portfolio(symbol)
    }
}

fn simulate_backtest(
    symbol: &str,
    strategy: &str,
    capital: f64,
    options: BacktestOptions,
    candles: Vec<Candle>,
) -> MarketDataResult<BacktestReport> {
    if candles.is_empty() {
        return Err("backtest requires at least one candle".to_string());
    }
    let strategy_spec = parse_strategy(strategy)?;
    let source = candles
        .first()
        .map(|c| c.source.clone())
        .unwrap_or_else(|| "unknown".to_string());
    let interval = non_empty_or_default(&options.interval, "1d");
    let mut cash = capital;
    let mut quantity = 0.0_f64;
    let mut trade_log = Vec::new();
    let mut equity_curve = Vec::with_capacity(candles.len());
    let mut warnings = Vec::new();

    if strategy_spec.minimum_candles() > candles.len() {
        warnings.push(format!(
            "strategy needs at least {} candles; received {}",
            strategy_spec.minimum_candles(),
            candles.len()
        ));
    }

    for (index, candle) in candles.iter().enumerate() {
        let price = candle.close;
        if price <= 0.0 || !price.is_finite() {
            return Err(format!("candle {index} has invalid close price {price}"));
        }
        let should_hold = strategy_spec.signal(&candles, index);
        if should_hold && quantity <= f64::EPSILON && cash > f64::EPSILON {
            quantity = cash / price;
            let notional = quantity * price;
            cash = 0.0;
            trade_log.push(BacktestTrade {
                side: OrderSide::Buy,
                index: candle.index,
                price: round_price(price),
                quantity: round_quantity(quantity),
                notional: round_price(notional),
                cash_after: round_price(cash),
                reason: "enter_long".to_string(),
            });
        } else if !should_hold && quantity > f64::EPSILON {
            let notional = quantity * price;
            cash += notional;
            trade_log.push(BacktestTrade {
                side: OrderSide::Sell,
                index: candle.index,
                price: round_price(price),
                quantity: round_quantity(quantity),
                notional: round_price(notional),
                cash_after: round_price(cash),
                reason: "exit_signal".to_string(),
            });
            quantity = 0.0;
        }
        equity_curve.push(BacktestEquityPoint {
            index: candle.index,
            close: round_price(price),
            cash: round_price(cash),
            position_quantity: round_quantity(quantity),
            equity: round_price(cash + quantity * price),
        });
    }

    if quantity > f64::EPSILON {
        let last = &candles[candles.len() - 1];
        let notional = quantity * last.close;
        cash += notional;
        trade_log.push(BacktestTrade {
            side: OrderSide::Sell,
            index: last.index,
            price: round_price(last.close),
            quantity: round_quantity(quantity),
            notional: round_price(notional),
            cash_after: round_price(cash),
            reason: "final_exit".to_string(),
        });
        if let Some(point) = equity_curve.last_mut() {
            point.cash = round_price(cash);
            point.position_quantity = 0.0;
            point.equity = round_price(cash);
        }
    }

    let ending_capital = round_price(cash);
    let total_return_pct = ((ending_capital / capital) - 1.0) * 100.0;
    let annualized_return_pct = annualized_return_pct(total_return_pct, equity_curve.len());
    let max_drawdown_pct = max_drawdown_pct_from_equity(&equity_curve);
    Ok(BacktestReport {
        symbol: normalize_symbol(symbol),
        strategy: strategy.to_string(),
        interval,
        requested_start: options.start,
        requested_end: options.end,
        candles: candles.len(),
        market_data_source: source.clone(),
        initial_capital: round_price(capital),
        ending_capital,
        total_return_pct: round_price(total_return_pct),
        annualized_return_pct: round_price(annualized_return_pct),
        sharpe_ratio: round_price(sharpe_ratio(&equity_curve)),
        max_drawdown_pct: round_price(max_drawdown_pct),
        trades: trade_log.len() as u32,
        trade_log,
        equity_curve,
        warnings,
        engine: format!("backtest-v1/{source}"),
    })
}

impl Default for QuantRuntime {
    fn default() -> Self {
        Self::new()
    }
}

pub trait MarketDataAdapter: Send + Sync {
    fn quote(&self, symbol: &str) -> MarketDataResult<Quote>;
    fn candles(&self, symbol: &str, interval: &str, limit: usize) -> MarketDataResult<Vec<Candle>>;
    fn depth(&self, symbol: &str, limit: usize) -> MarketDataResult<OrderBook>;
}

#[derive(Default)]
pub struct SyntheticMarketData {
    _private: (),
}

impl MarketDataAdapter for SyntheticMarketData {
    fn quote(&self, symbol: &str) -> MarketDataResult<Quote> {
        let price = synthetic_price(symbol);
        Ok(Quote {
            symbol: normalize_symbol(symbol),
            price,
            bid: round_price(price * 0.9995),
            ask: round_price(price * 1.0005),
            timestamp_ms: now_ms(),
            source: "synthetic-paper".to_string(),
        })
    }

    fn candles(&self, symbol: &str, interval: &str, limit: usize) -> MarketDataResult<Vec<Candle>> {
        let limit = limit.clamp(1, 500);
        let mut out = Vec::with_capacity(limit);
        let base = synthetic_price(symbol);
        let symbol_seed = stable_seed(symbol) as f64;
        for i in 0..limit {
            let t = i as f64;
            let drift = 1.0 + ((t - limit as f64 * 0.5) / limit as f64) * 0.035;
            let wave = ((t + symbol_seed % 17.0) * 0.37).sin() * 0.018;
            let open = round_price(base * drift * (1.0 + wave));
            let close = round_price(base * drift * (1.0 + wave * 0.8 + 0.002));
            let high = round_price(open.max(close) * 1.006);
            let low = round_price(open.min(close) * 0.994);
            out.push(Candle {
                symbol: normalize_symbol(symbol),
                interval: interval.to_string(),
                open,
                high,
                low,
                close,
                volume: round_price(1000.0 + (symbol_seed % 250.0) + t * 13.0),
                index: i as u64,
                source: "synthetic-paper".to_string(),
            });
        }
        Ok(out)
    }

    fn depth(&self, symbol: &str, limit: usize) -> MarketDataResult<OrderBook> {
        let limit = limit.clamp(1, 50);
        let quote = self.quote(symbol)?;
        let mut bids = Vec::with_capacity(limit);
        let mut asks = Vec::with_capacity(limit);
        for i in 0..limit {
            let level = (i + 1) as f64;
            bids.push(BookLevel {
                price: round_price(quote.bid * (1.0 - level * 0.0008)),
                quantity: round_price(1.0 + level * 0.25),
            });
            asks.push(BookLevel {
                price: round_price(quote.ask * (1.0 + level * 0.0008)),
                quantity: round_price(1.0 + level * 0.2),
            });
        }
        Ok(OrderBook {
            symbol: quote.symbol,
            bids,
            asks,
            timestamp_ms: quote.timestamp_ms,
            source: quote.source,
        })
    }
}

pub trait JsonHttpTransport: Send + Sync {
    fn get_json(&self, url: &str) -> MarketDataResult<Value>;
}

#[derive(Default)]
pub struct ReqwestJsonTransport {
    _private: (),
}

impl JsonHttpTransport for ReqwestJsonTransport {
    fn get_json(&self, url: &str) -> MarketDataResult<Value> {
        let client = reqwest::blocking::Client::builder()
            .timeout(Duration::from_secs(10))
            .build()
            .map_err(|e| format!("HTTP client setup failed: {e}"))?;
        client
            .get(url)
            .send()
            .map_err(|e| format!("HTTP request failed: {e}"))?
            .error_for_status()
            .map_err(|e| format!("HTTP status error: {e}"))?
            .json::<Value>()
            .map_err(|e| format!("Decode JSON failed: {e}"))
    }
}

pub struct BinanceMarketData<T: JsonHttpTransport = ReqwestJsonTransport> {
    base_url: String,
    transport: T,
}

impl BinanceMarketData<ReqwestJsonTransport> {
    pub fn new() -> Self {
        Self::with_transport("https://api.binance.com", ReqwestJsonTransport::default())
    }

    pub fn with_base_url(base_url: impl Into<String>) -> Self {
        Self::with_transport(base_url, ReqwestJsonTransport::default())
    }
}

impl Default for BinanceMarketData<ReqwestJsonTransport> {
    fn default() -> Self {
        Self::new()
    }
}

impl<T: JsonHttpTransport> BinanceMarketData<T> {
    pub fn with_transport(base_url: impl Into<String>, transport: T) -> Self {
        Self {
            base_url: base_url.into().trim_end_matches('/').to_string(),
            transport,
        }
    }

    fn url(&self, path: &str, query: &str) -> String {
        format!("{}{}?{}", self.base_url, path, query)
    }
}

impl<T: JsonHttpTransport> MarketDataAdapter for BinanceMarketData<T> {
    fn quote(&self, symbol: &str) -> MarketDataResult<Quote> {
        let symbol = normalize_symbol(symbol);
        let url = self.url("/api/v3/ticker/bookTicker", &format!("symbol={symbol}"));
        let value = self.transport.get_json(&url)?;
        let bid = parse_json_f64(&value, "bidPrice")?;
        let ask = parse_json_f64(&value, "askPrice")?;
        Ok(Quote {
            symbol: parse_json_string(&value, "symbol").unwrap_or(symbol),
            price: round_price((bid + ask) / 2.0),
            bid: round_price(bid),
            ask: round_price(ask),
            timestamp_ms: now_ms(),
            source: "binance-http".to_string(),
        })
    }

    fn candles(&self, symbol: &str, interval: &str, limit: usize) -> MarketDataResult<Vec<Candle>> {
        let symbol = normalize_symbol(symbol);
        let limit = limit.clamp(1, 1000);
        let url = self.url(
            "/api/v3/klines",
            &format!("symbol={symbol}&interval={interval}&limit={limit}"),
        );
        let value = self.transport.get_json(&url)?;
        let rows = value
            .as_array()
            .ok_or_else(|| "Binance klines response must be an array".to_string())?;
        rows.iter()
            .enumerate()
            .map(|(index, row)| parse_binance_kline(&symbol, interval, index, row))
            .collect()
    }

    fn depth(&self, symbol: &str, limit: usize) -> MarketDataResult<OrderBook> {
        let symbol = normalize_symbol(symbol);
        let limit = limit.clamp(1, 5000);
        let url = self.url("/api/v3/depth", &format!("symbol={symbol}&limit={limit}"));
        let value = self.transport.get_json(&url)?;
        Ok(OrderBook {
            symbol,
            bids: parse_binance_book_side(&value, "bids")?,
            asks: parse_binance_book_side(&value, "asks")?,
            timestamp_ms: now_ms(),
            source: "binance-http".to_string(),
        })
    }
}

pub trait BrokerAdapter: Send + Sync {
    fn place_order(&self, req: OrderRequest, quote: Quote) -> OrderDecision;
    fn cancel_order(&self, req: CancelOrderRequest) -> OrderDecision;
    fn apply_quote(&self, quote: Quote) -> Vec<OrderDecision>;
    fn portfolio(&self, symbol: Option<&str>) -> PortfolioSnapshot;
}

pub struct PaperBroker {
    account: Mutex<PaperAccount>,
    risk: Arc<dyn RiskPolicy>,
    audit: Arc<dyn AuditSink>,
    state_path: Option<PathBuf>,
    commission_bps: f64,
}

impl PaperBroker {
    pub fn new(risk: Arc<dyn RiskPolicy>) -> Self {
        Self::new_with_audit(risk, Arc::new(NoopAuditSink::default()))
    }

    pub fn new_with_starting_cash(risk: Arc<dyn RiskPolicy>, starting_cash: f64) -> Self {
        Self::new_with_audit_and_starting_cash(
            risk,
            Arc::new(NoopAuditSink::default()),
            starting_cash,
        )
    }

    pub fn new_with_audit(risk: Arc<dyn RiskPolicy>, audit: Arc<dyn AuditSink>) -> Self {
        Self::new_with_audit_and_starting_cash(risk, audit, default_paper_starting_cash())
    }

    pub fn new_with_audit_and_starting_cash(
        risk: Arc<dyn RiskPolicy>,
        audit: Arc<dyn AuditSink>,
        starting_cash: f64,
    ) -> Self {
        Self::new_with_audit_starting_cash_and_commission(risk, audit, starting_cash, 0.0)
    }

    pub fn new_with_audit_starting_cash_and_commission(
        risk: Arc<dyn RiskPolicy>,
        audit: Arc<dyn AuditSink>,
        starting_cash: f64,
        commission_bps: f64,
    ) -> Self {
        Self {
            account: Mutex::new(PaperAccount::new(starting_cash)),
            risk,
            audit,
            state_path: None,
            commission_bps: sanitize_commission_bps(commission_bps),
        }
    }

    pub fn new_with_audit_starting_cash_and_state(
        risk: Arc<dyn RiskPolicy>,
        audit: Arc<dyn AuditSink>,
        starting_cash: f64,
        state_path: impl Into<PathBuf>,
    ) -> Result<Self, String> {
        let state_path = state_path.into();
        let account = PaperAccount::load_or_new(&state_path, starting_cash)?;
        Ok(Self {
            account: Mutex::new(account),
            risk,
            audit,
            state_path: Some(state_path),
            commission_bps: 0.0,
        })
    }

    pub fn new_with_audit_starting_cash_state_and_commission(
        risk: Arc<dyn RiskPolicy>,
        audit: Arc<dyn AuditSink>,
        starting_cash: f64,
        state_path: impl Into<PathBuf>,
        commission_bps: f64,
    ) -> Result<Self, String> {
        let state_path = state_path.into();
        let account = PaperAccount::load_or_new(&state_path, starting_cash)?;
        Ok(Self {
            account: Mutex::new(account),
            risk,
            audit,
            state_path: Some(state_path),
            commission_bps: sanitize_commission_bps(commission_bps),
        })
    }

    fn finalize_decision(
        &self,
        account: &mut PaperAccount,
        mut decision: OrderDecision,
    ) -> OrderDecision {
        if let Err(err) = self.audit.record(&decision) {
            decision.audit_error = Some(err);
        }
        account.order_history.push(decision.clone());
        if let Err(err) = self.save_account(account) {
            decision.persistence_error = Some(err);
            if let Some(last) = account.order_history.last_mut() {
                last.persistence_error = decision.persistence_error.clone();
            }
        }
        decision
    }

    fn save_account(&self, account: &PaperAccount) -> Result<(), String> {
        let Some(path) = &self.state_path else {
            return Ok(());
        };
        account.save(path)
    }
}

impl Default for PaperBroker {
    fn default() -> Self {
        Self::new(Arc::new(DefaultRiskPolicy::default()))
    }
}

impl BrokerAdapter for PaperBroker {
    fn place_order(&self, req: OrderRequest, quote: Quote) -> OrderDecision {
        let mut account = self.account.lock().unwrap_or_else(|e| e.into_inner());
        let decision_id = format!("DECISION-{:06}", account.next_decision_id);
        account.next_decision_id += 1;
        let limits = self.risk.limits();
        let execution = match PaperExecution::from_request(&req, &quote) {
            Ok(execution) => execution,
            Err(reason) => {
                let decision = OrderDecision {
                    decision_id,
                    status: "rejected_invalid".to_string(),
                    order: None,
                    rejection_reason: Some(reason),
                    risk_checks: Vec::new(),
                    account: account.summary(&limits, self.commission_bps),
                    audit_error: None,
                    persistence_error: None,
                };
                return self.finalize_decision(&mut account, decision);
            }
        };
        let risk = self.risk.assess(
            &account.snapshot(),
            &req,
            execution.risk_price,
            self.commission_bps,
        );
        if let Some(reason) = risk.rejection_reason {
            let decision = OrderDecision {
                decision_id,
                status: "rejected_risk".to_string(),
                order: None,
                rejection_reason: Some(reason),
                risk_checks: risk.checks,
                account: account.summary(&limits, self.commission_bps),
                audit_error: None,
                persistence_error: None,
            };
            return self.finalize_decision(&mut account, decision);
        }
        let id = account.next_order_id;
        account.next_order_id += 1;
        let order = Order {
            order_id: format!("PAPER-{id:06}"),
            status: execution.status.to_string(),
            symbol: normalize_symbol(&req.symbol),
            side: req.side,
            order_type: req.order_type,
            quantity: req.quantity,
            limit_price: req.price.map(round_price),
            fill_price: execution.fill_price.map(round_price),
            filled_quantity: if execution.fill_price.is_some() {
                req.quantity
            } else {
                0.0
            },
            commission: execution
                .fill_price
                .map(|fill_price| commission_for(fill_price, req.quantity, self.commission_bps))
                .unwrap_or_default(),
            timestamp_ms: now_ms(),
        };
        if order.fill_price.is_some() {
            account.apply_fill(&order);
        } else {
            account.open_orders.push(order.clone());
        }
        let decision = OrderDecision {
            decision_id,
            status: order.status.clone(),
            order: Some(order),
            rejection_reason: None,
            risk_checks: risk.checks,
            account: account.summary(&limits, self.commission_bps),
            audit_error: None,
            persistence_error: None,
        };
        self.finalize_decision(&mut account, decision)
    }

    fn cancel_order(&self, req: CancelOrderRequest) -> OrderDecision {
        let mut account = self.account.lock().unwrap_or_else(|e| e.into_inner());
        let decision_id = format!("DECISION-{:06}", account.next_decision_id);
        account.next_decision_id += 1;
        let limits = self.risk.limits();
        let order_index = account
            .open_orders
            .iter()
            .position(|order| order.order_id == req.order_id);
        let decision = match order_index {
            Some(index) => {
                let mut order = account.open_orders.remove(index);
                order.status = "cancelled_paper".to_string();
                OrderDecision {
                    decision_id,
                    status: "cancelled_paper".to_string(),
                    order: Some(order),
                    rejection_reason: None,
                    risk_checks: Vec::new(),
                    account: account.summary(&limits, self.commission_bps),
                    audit_error: None,
                    persistence_error: None,
                }
            }
            None => OrderDecision {
                decision_id,
                status: "rejected_cancel".to_string(),
                order: None,
                rejection_reason: Some(format!("open order '{}' not found", req.order_id)),
                risk_checks: Vec::new(),
                account: account.summary(&limits, self.commission_bps),
                audit_error: None,
                persistence_error: None,
            },
        };
        self.finalize_decision(&mut account, decision)
    }

    fn apply_quote(&self, quote: Quote) -> Vec<OrderDecision> {
        let mut account = self.account.lock().unwrap_or_else(|e| e.into_inner());
        account.mark_to_market(&quote);

        let mut remaining = Vec::with_capacity(account.open_orders.len());
        let mut candidate_orders = Vec::new();
        for order in account.open_orders.drain(..) {
            if order.symbol == quote.symbol && order_is_marketable(&order, &quote) {
                candidate_orders.push(order);
            } else {
                remaining.push(order);
            }
        }
        account.open_orders = remaining;

        let mut decisions = Vec::new();
        for mut order in candidate_orders {
            let decision_id = format!("DECISION-{:06}", account.next_decision_id);
            account.next_decision_id += 1;
            let limits = self.risk.limits();
            let fill_price = quote_fill_price(order.side, &quote);
            let req = OrderRequest {
                symbol: order.symbol.clone(),
                side: order.side,
                order_type: order.order_type,
                quantity: order.quantity,
                price: order.limit_price,
            };
            let risk = self
                .risk
                .assess(&account.snapshot(), &req, fill_price, self.commission_bps);
            let decision = if let Some(reason) = risk.rejection_reason {
                order.status = "rejected_risk".to_string();
                OrderDecision {
                    decision_id,
                    status: "rejected_risk".to_string(),
                    order: Some(order),
                    rejection_reason: Some(reason),
                    risk_checks: risk.checks,
                    account: account.summary(&limits, self.commission_bps),
                    audit_error: None,
                    persistence_error: None,
                }
            } else {
                order.status = "filled_paper".to_string();
                order.fill_price = Some(round_price(fill_price));
                order.filled_quantity = order.quantity;
                order.commission = commission_for(fill_price, order.quantity, self.commission_bps);
                order.timestamp_ms = now_ms();
                account.apply_fill(&order);
                account.mark_to_market(&quote);
                OrderDecision {
                    decision_id,
                    status: "filled_paper".to_string(),
                    order: Some(order),
                    rejection_reason: None,
                    risk_checks: risk.checks,
                    account: account.summary(&limits, self.commission_bps),
                    audit_error: None,
                    persistence_error: None,
                }
            };
            decisions.push(self.finalize_decision(&mut account, decision));
        }

        decisions
    }

    fn portfolio(&self, symbol: Option<&str>) -> PortfolioSnapshot {
        let account = self.account.lock().unwrap_or_else(|e| e.into_inner());
        let limits = self.risk.limits();
        let filter = symbol.map(normalize_symbol);
        let positions = account
            .positions
            .values()
            .filter(|p| {
                filter
                    .as_ref()
                    .map(|symbol| p.symbol == *symbol)
                    .unwrap_or(true)
            })
            .cloned()
            .collect();
        let open_orders = account
            .open_orders
            .iter()
            .filter(|order| {
                filter
                    .as_ref()
                    .map(|symbol| order.symbol == *symbol)
                    .unwrap_or(true)
            })
            .cloned()
            .collect();
        PortfolioSnapshot {
            positions,
            open_orders,
            account_mode: "paper".to_string(),
            account: account.summary(&limits, self.commission_bps),
            risk_limits: limits,
            order_history: account.order_history.clone(),
        }
    }
}

fn validate_quote(quote: &Quote) -> MarketDataResult<()> {
    if quote.price <= 0.0
        || quote.bid <= 0.0
        || quote.ask <= 0.0
        || !quote.price.is_finite()
        || !quote.bid.is_finite()
        || !quote.ask.is_finite()
        || quote.bid > quote.ask
    {
        return Err("invalid_market_quote".to_string());
    }
    Ok(())
}

fn order_is_marketable(order: &Order, quote: &Quote) -> bool {
    if order.order_type != OrderType::Limit {
        return false;
    }
    let Some(limit_price) = order.limit_price else {
        return false;
    };
    match order.side {
        OrderSide::Buy => limit_price >= quote.ask,
        OrderSide::Sell => limit_price <= quote.bid,
    }
}

fn quote_fill_price(side: OrderSide, quote: &Quote) -> f64 {
    match side {
        OrderSide::Buy => quote.ask,
        OrderSide::Sell => quote.bid,
    }
}

fn sanitize_commission_bps(value: f64) -> f64 {
    if value.is_finite() && value > 0.0 {
        value
    } else {
        0.0
    }
}

fn commission_for(price: f64, quantity: f64, commission_bps: f64) -> f64 {
    if commission_bps <= 0.0 {
        return 0.0;
    }
    round_price(price * quantity * commission_bps / 10_000.0)
}

pub trait RiskPolicy: Send + Sync {
    fn limits(&self) -> RiskLimits;
    fn assess(
        &self,
        account: &PaperAccountSnapshot,
        req: &OrderRequest,
        fill_price: f64,
        commission_bps: f64,
    ) -> RiskAssessment;
}

pub trait AuditSink: Send + Sync {
    fn record(&self, decision: &OrderDecision) -> Result<(), String>;
}

#[derive(Default)]
pub struct NoopAuditSink {
    _private: (),
}

impl AuditSink for NoopAuditSink {
    fn record(&self, _decision: &OrderDecision) -> Result<(), String> {
        Ok(())
    }
}

pub struct JsonlAuditSink {
    path: PathBuf,
    lock: Mutex<()>,
}

impl JsonlAuditSink {
    pub fn new(path: impl Into<PathBuf>) -> Self {
        Self {
            path: path.into(),
            lock: Mutex::new(()),
        }
    }

    pub fn path(&self) -> &Path {
        &self.path
    }
}

impl AuditSink for JsonlAuditSink {
    fn record(&self, decision: &OrderDecision) -> Result<(), String> {
        let _guard = self.lock.lock().unwrap_or_else(|e| e.into_inner());
        if let Some(parent) = self.path.parent()
            && !parent.as_os_str().is_empty()
        {
            std::fs::create_dir_all(parent)
                .map_err(|e| format!("create audit log directory failed: {e}"))?;
        }
        let mut file = OpenOptions::new()
            .create(true)
            .append(true)
            .open(&self.path)
            .map_err(|e| format!("open audit log failed: {e}"))?;
        serde_json::to_writer(&mut file, decision)
            .map_err(|e| format!("serialize audit decision failed: {e}"))?;
        file.write_all(b"\n")
            .map_err(|e| format!("write audit newline failed: {e}"))?;
        file.flush()
            .map_err(|e| format!("flush audit log failed: {e}"))
    }
}

#[derive(Default)]
pub struct DefaultRiskPolicy {
    limits: RiskLimits,
}

impl DefaultRiskPolicy {
    pub fn new(limits: RiskLimits) -> Self {
        Self { limits }
    }
}

impl RiskPolicy for DefaultRiskPolicy {
    fn limits(&self) -> RiskLimits {
        self.limits.clone()
    }

    fn assess(
        &self,
        account: &PaperAccountSnapshot,
        req: &OrderRequest,
        fill_price: f64,
        commission_bps: f64,
    ) -> RiskAssessment {
        let mut checks = Vec::new();
        let mut rejection_reason = None;
        if fill_price <= 0.0 || !fill_price.is_finite() {
            checks.push(RiskCheck::failed(
                "valid_fill_price",
                "fill price must be positive",
            ));
            return RiskAssessment {
                checks,
                rejection_reason: Some("invalid_fill_price".to_string()),
            };
        }
        checks.push(RiskCheck::passed("valid_fill_price"));
        let notional = round_price(req.quantity * fill_price);
        if notional > self.limits.max_order_notional {
            let reason = format!(
                "order_notional_exceeds_limit: {notional} > {}",
                self.limits.max_order_notional
            );
            checks.push(RiskCheck::failed("max_order_notional", &reason));
            rejection_reason.get_or_insert(reason);
        } else {
            checks.push(RiskCheck::passed("max_order_notional"));
        }
        let symbol = normalize_symbol(&req.symbol);
        let current_position = account.positions.iter().find(|p| p.symbol == symbol);
        let held = current_position.map(|p| p.quantity).unwrap_or(0.0);
        match req.side {
            OrderSide::Buy => {
                let commission = commission_for(fill_price, req.quantity, commission_bps);
                let required_cash = round_price(notional + commission);
                let available_cash = round_price(
                    account.cash
                        - reserved_buy_notional(account)
                        - reserved_buy_commission(account, commission_bps),
                );
                if required_cash > available_cash {
                    let reason = format!(
                        "insufficient_cash: required {required_cash}, available {}",
                        round_price(available_cash)
                    );
                    checks.push(RiskCheck::failed("sufficient_cash", &reason));
                    rejection_reason.get_or_insert(reason);
                } else {
                    checks.push(RiskCheck::passed("sufficient_cash"));
                }
            }
            OrderSide::Sell => {
                let available_inventory =
                    round_quantity(held.max(0.0) - reserved_sell_quantity(account, &symbol));
                if !self.limits.allow_short_selling && available_inventory < req.quantity {
                    let reason = format!(
                        "short_selling_disabled: requested {}, held {}",
                        req.quantity,
                        round_quantity(available_inventory)
                    );
                    checks.push(RiskCheck::failed("inventory_available", &reason));
                    rejection_reason.get_or_insert(reason);
                } else {
                    checks.push(RiskCheck::passed("inventory_available"));
                }
            }
        }
        let projected = projected_gross_exposure(account, req, fill_price);
        if projected > self.limits.max_gross_exposure {
            let reason = format!(
                "gross_exposure_exceeds_limit: {projected} > {}",
                self.limits.max_gross_exposure
            );
            checks.push(RiskCheck::failed("max_gross_exposure", &reason));
            rejection_reason.get_or_insert(reason);
        } else {
            checks.push(RiskCheck::passed("max_gross_exposure"));
        }
        RiskAssessment {
            checks,
            rejection_reason,
        }
    }
}

#[derive(Debug, Clone, Serialize)]
pub struct Quote {
    pub symbol: String,
    pub price: f64,
    pub bid: f64,
    pub ask: f64,
    pub timestamp_ms: u128,
    pub source: String,
}

#[derive(Debug, Clone, Serialize)]
pub struct Candle {
    pub symbol: String,
    pub interval: String,
    pub open: f64,
    pub high: f64,
    pub low: f64,
    pub close: f64,
    pub volume: f64,
    pub index: u64,
    pub source: String,
}

#[derive(Debug, Clone, Serialize)]
pub struct BookLevel {
    pub price: f64,
    pub quantity: f64,
}

#[derive(Debug, Clone, Serialize)]
pub struct OrderBook {
    pub symbol: String,
    pub bids: Vec<BookLevel>,
    pub asks: Vec<BookLevel>,
    pub timestamp_ms: u128,
    pub source: String,
}

#[derive(Debug, Clone, Serialize)]
pub struct BacktestReport {
    pub symbol: String,
    pub strategy: String,
    pub interval: String,
    pub requested_start: Option<String>,
    pub requested_end: Option<String>,
    pub candles: usize,
    pub market_data_source: String,
    pub initial_capital: f64,
    pub ending_capital: f64,
    pub total_return_pct: f64,
    pub annualized_return_pct: f64,
    pub sharpe_ratio: f64,
    pub max_drawdown_pct: f64,
    pub trades: u32,
    pub trade_log: Vec<BacktestTrade>,
    pub equity_curve: Vec<BacktestEquityPoint>,
    pub warnings: Vec<String>,
    pub engine: String,
}

#[derive(Debug, Clone)]
pub struct BacktestOptions {
    pub interval: String,
    pub limit: usize,
    pub start: Option<String>,
    pub end: Option<String>,
}

impl Default for BacktestOptions {
    fn default() -> Self {
        Self {
            interval: "1d".to_string(),
            limit: 365,
            start: None,
            end: None,
        }
    }
}

#[derive(Debug, Clone, Serialize)]
pub struct BacktestTrade {
    pub side: OrderSide,
    pub index: u64,
    pub price: f64,
    pub quantity: f64,
    pub notional: f64,
    pub cash_after: f64,
    pub reason: String,
}

#[derive(Debug, Clone, Serialize)]
pub struct BacktestEquityPoint {
    pub index: u64,
    pub close: f64,
    pub cash: f64,
    pub position_quantity: f64,
    pub equity: f64,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum OrderSide {
    Buy,
    Sell,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum OrderType {
    Market,
    Limit,
}

#[derive(Debug, Clone)]
pub struct OrderRequest {
    pub symbol: String,
    pub side: OrderSide,
    pub order_type: OrderType,
    pub quantity: f64,
    pub price: Option<f64>,
}

#[derive(Debug, Clone)]
pub struct CancelOrderRequest {
    pub order_id: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Order {
    pub order_id: String,
    pub status: String,
    pub symbol: String,
    pub side: OrderSide,
    #[serde(rename = "type")]
    pub order_type: OrderType,
    pub quantity: f64,
    pub limit_price: Option<f64>,
    pub fill_price: Option<f64>,
    pub filled_quantity: f64,
    #[serde(default)]
    pub commission: f64,
    pub timestamp_ms: u128,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct OrderDecision {
    pub decision_id: String,
    pub status: String,
    pub order: Option<Order>,
    pub rejection_reason: Option<String>,
    pub risk_checks: Vec<RiskCheck>,
    pub account: AccountSummary,
    pub audit_error: Option<String>,
    pub persistence_error: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RiskCheck {
    pub name: String,
    pub passed: bool,
    pub message: String,
}

impl RiskCheck {
    pub fn passed(name: &str) -> Self {
        Self {
            name: name.to_string(),
            passed: true,
            message: "ok".to_string(),
        }
    }

    pub fn failed(name: &str, message: &str) -> Self {
        Self {
            name: name.to_string(),
            passed: false,
            message: message.to_string(),
        }
    }
}

pub struct RiskAssessment {
    pub checks: Vec<RiskCheck>,
    pub rejection_reason: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Position {
    pub symbol: String,
    pub quantity: f64,
    pub average_price: f64,
    pub market_price: f64,
    pub market_value: f64,
    pub unrealized_pnl: f64,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PortfolioSnapshot {
    pub positions: Vec<Position>,
    pub open_orders: Vec<Order>,
    pub account_mode: String,
    pub account: AccountSummary,
    pub risk_limits: RiskLimits,
    pub order_history: Vec<OrderDecision>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AccountSummary {
    pub currency: String,
    pub cash: f64,
    #[serde(default)]
    pub available_cash: f64,
    pub equity: f64,
    #[serde(default)]
    pub realized_pnl: f64,
    #[serde(default)]
    pub total_commission: f64,
    #[serde(default)]
    pub net_pnl: f64,
    pub gross_exposure: f64,
    #[serde(default)]
    pub projected_gross_exposure: f64,
    #[serde(default)]
    pub reserved_buy_notional: f64,
    #[serde(default)]
    pub reserved_buy_commission: f64,
    #[serde(default)]
    pub reserved_sell_notional: f64,
    pub buying_power: f64,
}

#[derive(Debug, Clone)]
pub struct PaperAccountSnapshot {
    pub cash: f64,
    pub positions: Vec<Position>,
    pub open_orders: Vec<Order>,
    pub gross_exposure: f64,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RiskLimits {
    pub max_order_notional: f64,
    pub max_gross_exposure: f64,
    pub allow_short_selling: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct PaperAccount {
    next_decision_id: u64,
    next_order_id: u64,
    cash: f64,
    #[serde(default)]
    realized_pnl: f64,
    #[serde(default)]
    total_commission: f64,
    positions: BTreeMap<String, Position>,
    open_orders: Vec<Order>,
    order_history: Vec<OrderDecision>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct PaperStateFile {
    version: u32,
    account: PaperAccount,
}

const PAPER_STATE_VERSION: u32 = 1;

struct PaperExecution {
    status: &'static str,
    risk_price: f64,
    fill_price: Option<f64>,
}

impl PaperExecution {
    fn from_request(req: &OrderRequest, quote: &Quote) -> Result<Self, String> {
        if quote.bid <= 0.0 || quote.ask <= 0.0 || quote.bid > quote.ask {
            return Err("invalid_market_quote".to_string());
        }
        match req.order_type {
            OrderType::Market => Ok(Self {
                status: "filled_paper",
                risk_price: match req.side {
                    OrderSide::Buy => quote.ask,
                    OrderSide::Sell => quote.bid,
                },
                fill_price: Some(match req.side {
                    OrderSide::Buy => quote.ask,
                    OrderSide::Sell => quote.bid,
                }),
            }),
            OrderType::Limit => {
                let limit_price = req
                    .price
                    .ok_or_else(|| "limit order requires price".to_string())?;
                if limit_price <= 0.0 || !limit_price.is_finite() {
                    return Err("limit price must be a positive finite number".to_string());
                }
                let marketable = match req.side {
                    OrderSide::Buy => limit_price >= quote.ask,
                    OrderSide::Sell => limit_price <= quote.bid,
                };
                if marketable {
                    Ok(Self {
                        status: "filled_paper",
                        risk_price: limit_price,
                        fill_price: Some(match req.side {
                            OrderSide::Buy => quote.ask,
                            OrderSide::Sell => quote.bid,
                        }),
                    })
                } else {
                    Ok(Self {
                        status: "open_paper",
                        risk_price: limit_price,
                        fill_price: None,
                    })
                }
            }
        }
    }
}

impl Default for RiskLimits {
    fn default() -> Self {
        Self {
            max_order_notional: 50_000.0,
            max_gross_exposure: 100_000.0,
            allow_short_selling: false,
        }
    }
}

impl Default for PaperAccount {
    fn default() -> Self {
        Self::new(default_paper_starting_cash())
    }
}

impl PaperAccount {
    fn new(starting_cash: f64) -> Self {
        Self {
            next_decision_id: 0,
            next_order_id: 0,
            cash: round_price(starting_cash),
            realized_pnl: 0.0,
            total_commission: 0.0,
            positions: BTreeMap::new(),
            open_orders: Vec::new(),
            order_history: Vec::new(),
        }
    }

    fn load_or_new(path: &Path, starting_cash: f64) -> Result<Self, String> {
        if !path.exists() {
            return Ok(Self::new(starting_cash));
        }
        let raw = std::fs::read_to_string(path)
            .map_err(|e| format!("read paper state {} failed: {e}", path.display()))?;
        let value = serde_json::from_str::<serde_json::Value>(&raw)
            .map_err(|e| format!("parse paper state {} failed: {e}", path.display()))?;
        if value.get("version").is_some() || value.get("account").is_some() {
            let state: PaperStateFile = serde_json::from_value(value)
                .map_err(|e| format!("parse paper state {} failed: {e}", path.display()))?;
            if state.version != PAPER_STATE_VERSION {
                return Err(format!(
                    "unsupported paper state version {} in {} (supported: {})",
                    state.version,
                    path.display(),
                    PAPER_STATE_VERSION
                ));
            }
            Ok(state.account)
        } else {
            serde_json::from_value(value)
                .map_err(|e| format!("parse legacy paper state {} failed: {e}", path.display()))
        }
    }

    fn save(&self, path: &Path) -> Result<(), String> {
        if let Some(parent) = path.parent()
            && !parent.as_os_str().is_empty()
        {
            std::fs::create_dir_all(parent)
                .map_err(|e| format!("create paper state directory failed: {e}"))?;
        }
        let tmp_path = path.with_extension(format!(
            "{}tmp",
            path.extension()
                .and_then(|ext| ext.to_str())
                .map(|ext| format!("{ext}."))
                .unwrap_or_default()
        ));
        let raw = serde_json::to_vec_pretty(&PaperStateFile {
            version: PAPER_STATE_VERSION,
            account: self.clone(),
        })
        .map_err(|e| format!("serialize paper state failed: {e}"))?;
        std::fs::write(&tmp_path, raw)
            .map_err(|e| format!("write paper state {} failed: {e}", tmp_path.display()))?;
        std::fs::rename(&tmp_path, path)
            .map_err(|e| format!("replace paper state {} failed: {e}", path.display()))
    }

    fn apply_fill(&mut self, order: &Order) {
        let Some(fill_price) = order.fill_price else {
            return;
        };
        let notional = round_price(fill_price * order.filled_quantity);
        self.total_commission = round_price(self.total_commission + order.commission);
        match order.side {
            OrderSide::Buy => self.cash = round_price(self.cash - notional - order.commission),
            OrderSide::Sell => self.cash = round_price(self.cash + notional - order.commission),
        }
        let signed_qty = match order.side {
            OrderSide::Buy => order.filled_quantity,
            OrderSide::Sell => -order.filled_quantity,
        };
        let entry = self
            .positions
            .entry(order.symbol.clone())
            .or_insert(Position {
                symbol: order.symbol.clone(),
                quantity: 0.0,
                average_price: fill_price,
                market_price: fill_price,
                market_value: 0.0,
                unrealized_pnl: 0.0,
            });
        let next_qty = entry.quantity + signed_qty;
        if entry.quantity.signum() != signed_qty.signum() && entry.quantity != 0.0 {
            let closed_qty = entry.quantity.abs().min(signed_qty.abs());
            let realized = if entry.quantity > 0.0 {
                (fill_price - entry.average_price) * closed_qty
            } else {
                (entry.average_price - fill_price) * closed_qty
            };
            self.realized_pnl = round_price(self.realized_pnl + realized);
        }
        if next_qty.abs() < f64::EPSILON {
            self.positions.remove(&order.symbol);
            return;
        }
        let old_qty = entry.quantity;
        if old_qty.signum() == signed_qty.signum() || old_qty == 0.0 {
            let old_notional = entry.average_price * entry.quantity.abs();
            let fill_notional = fill_price * signed_qty.abs();
            entry.average_price =
                round_price((old_notional + fill_notional) / next_qty.abs().max(f64::EPSILON));
        } else if next_qty.signum() != old_qty.signum() {
            entry.average_price = fill_price;
        }
        entry.quantity = round_price(next_qty);
        entry.market_price = fill_price;
        entry.market_value = round_price(entry.quantity * entry.market_price);
        entry.unrealized_pnl =
            round_price((entry.market_price - entry.average_price) * entry.quantity);
    }

    fn mark_to_market(&mut self, quote: &Quote) {
        if let Some(position) = self.positions.get_mut(&quote.symbol) {
            position.market_price = round_price(quote.price);
            position.market_value = round_price(position.quantity * position.market_price);
            position.unrealized_pnl =
                round_price((position.market_price - position.average_price) * position.quantity);
        }
    }

    fn summary(&self, risk: &RiskLimits, commission_bps: f64) -> AccountSummary {
        let gross_exposure = self.gross_exposure();
        let reserved_buy_notional = reserved_buy_notional_for_orders(&self.open_orders);
        let reserved_buy_commission =
            reserved_buy_commission_for_orders(&self.open_orders, commission_bps);
        let reserved_sell_notional = reserved_sell_notional_for_orders(&self.open_orders);
        let available_cash = (self.cash - reserved_buy_notional - reserved_buy_commission).max(0.0);
        let projected_gross_exposure =
            projected_gross_exposure_for_open_orders(&self.positions, &self.open_orders);
        let exposure_capacity = (risk.max_gross_exposure - projected_gross_exposure).max(0.0);
        AccountSummary {
            currency: "USD".to_string(),
            cash: round_price(self.cash),
            available_cash: round_price(available_cash),
            equity: round_price(self.cash + self.net_liquidation_value()),
            realized_pnl: round_price(self.realized_pnl),
            total_commission: round_price(self.total_commission),
            net_pnl: round_price(self.realized_pnl - self.total_commission),
            gross_exposure: round_price(gross_exposure),
            projected_gross_exposure,
            reserved_buy_notional,
            reserved_buy_commission,
            reserved_sell_notional,
            buying_power: round_price(available_cash.min(exposure_capacity)),
        }
    }

    fn gross_exposure(&self) -> f64 {
        round_price(
            self.positions
                .values()
                .map(|p| p.market_value.abs())
                .sum::<f64>(),
        )
    }

    fn net_liquidation_value(&self) -> f64 {
        round_price(self.positions.values().map(|p| p.market_value).sum::<f64>())
    }

    fn snapshot(&self) -> PaperAccountSnapshot {
        PaperAccountSnapshot {
            cash: round_price(self.cash),
            positions: self.positions.values().cloned().collect(),
            open_orders: self.open_orders.clone(),
            gross_exposure: self.gross_exposure(),
        }
    }
}

fn reserved_buy_notional(account: &PaperAccountSnapshot) -> f64 {
    reserved_buy_notional_for_orders(&account.open_orders)
}

fn reserved_buy_commission(account: &PaperAccountSnapshot, commission_bps: f64) -> f64 {
    reserved_buy_commission_for_orders(&account.open_orders, commission_bps)
}

fn reserved_buy_notional_for_orders(open_orders: &[Order]) -> f64 {
    round_price(
        open_orders
            .iter()
            .filter(|order| order.side == OrderSide::Buy)
            .filter_map(|order| {
                order
                    .limit_price
                    .map(|limit_price| limit_price * order.quantity)
            })
            .sum::<f64>(),
    )
}

fn reserved_buy_commission_for_orders(open_orders: &[Order], commission_bps: f64) -> f64 {
    round_price(
        open_orders
            .iter()
            .filter(|order| order.side == OrderSide::Buy)
            .filter_map(|order| {
                order
                    .limit_price
                    .map(|limit_price| commission_for(limit_price, order.quantity, commission_bps))
            })
            .sum::<f64>(),
    )
}

fn reserved_sell_notional_for_orders(open_orders: &[Order]) -> f64 {
    round_price(
        open_orders
            .iter()
            .filter(|order| order.side == OrderSide::Sell)
            .filter_map(|order| {
                order
                    .limit_price
                    .map(|limit_price| limit_price * order.quantity)
            })
            .sum::<f64>(),
    )
}

fn reserved_sell_quantity(account: &PaperAccountSnapshot, symbol: &str) -> f64 {
    round_quantity(
        account
            .open_orders
            .iter()
            .filter(|order| order.symbol == symbol && order.side == OrderSide::Sell)
            .map(|order| order.quantity)
            .sum::<f64>(),
    )
}

fn projected_gross_exposure(
    account: &PaperAccountSnapshot,
    req: &OrderRequest,
    fill_price: f64,
) -> f64 {
    let mut projected =
        projected_positions_after_open_orders(&account.positions, &account.open_orders);
    apply_projected_delta(
        &mut projected,
        &req.symbol,
        req.side,
        req.quantity,
        fill_price,
    );
    round_price(
        projected
            .values()
            .map(|(quantity, price)| (quantity * price).abs())
            .sum::<f64>(),
    )
}

fn projected_gross_exposure_for_open_orders(
    positions: &BTreeMap<String, Position>,
    open_orders: &[Order],
) -> f64 {
    let positions: Vec<Position> = positions.values().cloned().collect();
    let projected = projected_positions_after_open_orders(&positions, open_orders);
    round_price(
        projected
            .values()
            .map(|(quantity, price)| (quantity * price).abs())
            .sum::<f64>(),
    )
}

fn projected_positions_after_open_orders(
    positions: &[Position],
    open_orders: &[Order],
) -> BTreeMap<String, (f64, f64)> {
    let mut projected: BTreeMap<String, (f64, f64)> = positions
        .iter()
        .map(|position| {
            (
                position.symbol.clone(),
                (position.quantity, position.market_price),
            )
        })
        .collect();
    for order in open_orders {
        let price = order.limit_price.or(order.fill_price).unwrap_or_default();
        apply_projected_delta(
            &mut projected,
            &order.symbol,
            order.side,
            order.quantity,
            price,
        );
    }
    projected
}

fn apply_projected_delta(
    projected: &mut BTreeMap<String, (f64, f64)>,
    symbol: &str,
    side: OrderSide,
    quantity: f64,
    price: f64,
) {
    let entry = projected
        .entry(normalize_symbol(symbol))
        .or_insert((0.0, price));
    let signed_qty = match side {
        OrderSide::Buy => quantity,
        OrderSide::Sell => -quantity,
    };
    entry.0 = round_quantity(entry.0 + signed_qty);
    entry.1 = price;
}

fn normalize_symbol(symbol: &str) -> String {
    symbol.trim().to_uppercase()
}

fn synthetic_price(symbol: &str) -> f64 {
    match normalize_symbol(symbol).as_str() {
        "BTCUSDT" | "BTC-USD" => 65_000.0,
        "ETHUSDT" | "ETH-USD" => 3_500.0,
        "AAPL" => 190.0,
        "MSFT" => 430.0,
        other => {
            let seed = stable_seed(other);
            round_price(25.0 + (seed % 50_000) as f64 / 17.0)
        }
    }
}

fn stable_seed(text: &str) -> u64 {
    text.bytes().fold(0xcbf2_9ce4_8422_2325, |acc, b| {
        acc.wrapping_mul(0x100_0000_01b3).wrapping_add(b as u64)
    })
}

fn parse_json_string(value: &Value, key: &str) -> Option<String> {
    value.get(key)?.as_str().map(str::to_string)
}

fn parse_json_f64(value: &Value, key: &str) -> MarketDataResult<f64> {
    parse_value_f64(
        value
            .get(key)
            .ok_or_else(|| format!("Missing JSON field '{key}'"))?,
        key,
    )
}

fn parse_value_f64(value: &Value, label: &str) -> MarketDataResult<f64> {
    if let Some(number) = value.as_f64() {
        return Ok(number);
    }
    value
        .as_str()
        .ok_or_else(|| format!("JSON field '{label}' must be a string or number"))?
        .parse::<f64>()
        .map_err(|e| format!("JSON field '{label}' is not numeric: {e}"))
}

fn parse_binance_kline(
    symbol: &str,
    interval: &str,
    index: usize,
    row: &Value,
) -> MarketDataResult<Candle> {
    let values = row
        .as_array()
        .ok_or_else(|| "Binance kline row must be an array".to_string())?;
    let get = |idx: usize, name: &str| -> MarketDataResult<f64> {
        parse_value_f64(
            values
                .get(idx)
                .ok_or_else(|| format!("Missing Binance kline field '{name}'"))?,
            name,
        )
    };
    Ok(Candle {
        symbol: symbol.to_string(),
        interval: interval.to_string(),
        open: round_price(get(1, "open")?),
        high: round_price(get(2, "high")?),
        low: round_price(get(3, "low")?),
        close: round_price(get(4, "close")?),
        volume: round_price(get(5, "volume")?),
        index: index as u64,
        source: "binance-http".to_string(),
    })
}

fn parse_binance_book_side(value: &Value, key: &str) -> MarketDataResult<Vec<BookLevel>> {
    let rows = value
        .get(key)
        .and_then(Value::as_array)
        .ok_or_else(|| format!("Binance depth response missing array '{key}'"))?;
    rows.iter()
        .map(|row| {
            let values = row
                .as_array()
                .ok_or_else(|| format!("Binance depth '{key}' row must be an array"))?;
            Ok(BookLevel {
                price: round_price(parse_value_f64(
                    values
                        .first()
                        .ok_or_else(|| format!("Missing Binance depth '{key}' price"))?,
                    "price",
                )?),
                quantity: round_price(parse_value_f64(
                    values
                        .get(1)
                        .ok_or_else(|| format!("Missing Binance depth '{key}' quantity"))?,
                    "quantity",
                )?),
            })
        })
        .collect()
}

#[derive(Debug, Clone, Copy)]
enum StrategySpec {
    BuyAndHold,
    Momentum { lookback: usize },
    SmaCross { fast: usize, slow: usize },
}

impl StrategySpec {
    fn minimum_candles(self) -> usize {
        match self {
            StrategySpec::BuyAndHold => 1,
            StrategySpec::Momentum { lookback } => lookback + 1,
            StrategySpec::SmaCross { slow, .. } => slow,
        }
    }

    fn signal(self, candles: &[Candle], index: usize) -> bool {
        match self {
            StrategySpec::BuyAndHold => true,
            StrategySpec::Momentum { lookback } => {
                index >= lookback && candles[index].close > candles[index - lookback].close
            }
            StrategySpec::SmaCross { fast, slow } => {
                if index + 1 < slow {
                    return false;
                }
                simple_moving_average(candles, index, fast)
                    > simple_moving_average(candles, index, slow)
            }
        }
    }
}

fn parse_strategy(raw: &str) -> MarketDataResult<StrategySpec> {
    let normalized = raw.trim().to_ascii_lowercase().replace([' ', '-'], "_");
    if matches!(
        normalized.as_str(),
        "buy_hold" | "buy_and_hold" | "buy_hold()" | "buy_and_hold()"
    ) {
        return Ok(StrategySpec::BuyAndHold);
    }
    if normalized == "momentum" {
        return Ok(StrategySpec::Momentum { lookback: 20 });
    }
    if let Some(args) = call_args(&normalized, "momentum") {
        let lookback = parse_positive_usize(args, "momentum lookback")?;
        return Ok(StrategySpec::Momentum { lookback });
    }
    if let Some(args) = call_args(&normalized, "sma_cross") {
        let parts: Vec<&str> = args.split(',').collect();
        if parts.len() != 2 {
            return Err("sma_cross expects two windows, e.g. sma_cross(50,200)".to_string());
        }
        let fast = parse_positive_usize(parts[0], "sma fast window")?;
        let slow = parse_positive_usize(parts[1], "sma slow window")?;
        if fast >= slow {
            return Err("sma_cross fast window must be smaller than slow window".to_string());
        }
        return Ok(StrategySpec::SmaCross { fast, slow });
    }
    Err(format!(
        "unsupported strategy '{raw}' (supported: buy_hold, momentum(N), sma_cross(F,S))"
    ))
}

fn call_args<'a>(text: &'a str, name: &str) -> Option<&'a str> {
    let prefix = format!("{name}(");
    text.strip_prefix(&prefix)?.strip_suffix(')')
}

fn parse_positive_usize(raw: &str, label: &str) -> MarketDataResult<usize> {
    let value = raw
        .parse::<usize>()
        .map_err(|e| format!("{label} must be an integer: {e}"))?;
    if value == 0 {
        Err(format!("{label} must be positive"))
    } else {
        Ok(value)
    }
}

fn simple_moving_average(candles: &[Candle], index: usize, window: usize) -> f64 {
    let start = index + 1 - window;
    candles[start..=index].iter().map(|c| c.close).sum::<f64>() / window as f64
}

fn annualized_return_pct(total_return_pct: f64, periods: usize) -> f64 {
    if periods < 2 {
        return total_return_pct;
    }
    ((1.0 + total_return_pct / 100.0).powf(365.0 / periods as f64) - 1.0) * 100.0
}

fn sharpe_ratio(equity_curve: &[BacktestEquityPoint]) -> f64 {
    let returns: Vec<f64> = equity_curve
        .windows(2)
        .filter_map(|window| {
            let previous = window[0].equity;
            let current = window[1].equity;
            (previous > 0.0).then_some((current / previous) - 1.0)
        })
        .collect();
    if returns.len() < 2 {
        return 0.0;
    }
    let mean = returns.iter().sum::<f64>() / returns.len() as f64;
    let variance = returns
        .iter()
        .map(|value| (value - mean).powi(2))
        .sum::<f64>()
        / returns.len() as f64;
    let stddev = variance.sqrt();
    if stddev <= f64::EPSILON {
        0.0
    } else {
        mean / stddev * 252.0_f64.sqrt()
    }
}

fn max_drawdown_pct_from_equity(equity_curve: &[BacktestEquityPoint]) -> f64 {
    let mut peak = 0.0_f64;
    let mut max_drawdown = 0.0_f64;
    for point in equity_curve {
        peak = peak.max(point.equity);
        if peak > 0.0 {
            max_drawdown = max_drawdown.min(((point.equity / peak) - 1.0) * 100.0);
        }
    }
    max_drawdown
}

fn non_empty_or_default(value: &str, default: &str) -> String {
    let trimmed = value.trim();
    if trimmed.is_empty() {
        default.to_string()
    } else {
        trimmed.to_string()
    }
}

fn round_price(value: f64) -> f64 {
    (value * 100.0).round() / 100.0
}

fn round_quantity(value: f64) -> f64 {
    (value * 100_000_000.0).round() / 100_000_000.0
}

pub fn default_paper_starting_cash() -> f64 {
    100_000.0
}

fn now_ms() -> u128 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis())
        .unwrap_or_default()
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    use std::sync::atomic::{AtomicU64, Ordering};

    static TEST_ID: AtomicU64 = AtomicU64::new(0);

    struct FixedMarketData;

    impl MarketDataAdapter for FixedMarketData {
        fn quote(&self, symbol: &str) -> MarketDataResult<Quote> {
            Ok(Quote {
                symbol: normalize_symbol(symbol),
                price: 42.0,
                bid: 41.5,
                ask: 42.5,
                timestamp_ms: 1,
                source: "fixed-test".to_string(),
            })
        }

        fn candles(
            &self,
            symbol: &str,
            interval: &str,
            _limit: usize,
        ) -> MarketDataResult<Vec<Candle>> {
            Ok(vec![Candle {
                symbol: normalize_symbol(symbol),
                interval: interval.to_string(),
                open: 40.0,
                high: 44.0,
                low: 39.0,
                close: 42.0,
                volume: 10.0,
                index: 0,
                source: "fixed-test".to_string(),
            }])
        }

        fn depth(&self, symbol: &str, _limit: usize) -> MarketDataResult<OrderBook> {
            Ok(OrderBook {
                symbol: normalize_symbol(symbol),
                bids: vec![BookLevel {
                    price: 41.5,
                    quantity: 1.0,
                }],
                asks: vec![BookLevel {
                    price: 42.5,
                    quantity: 1.0,
                }],
                timestamp_ms: 1,
                source: "fixed-test".to_string(),
            })
        }
    }

    #[derive(Clone)]
    struct MutableMarketData {
        quote: Arc<Mutex<Quote>>,
    }

    impl MutableMarketData {
        fn new(symbol: &str, price: f64, bid: f64, ask: f64) -> Self {
            Self {
                quote: Arc::new(Mutex::new(Quote {
                    symbol: normalize_symbol(symbol),
                    price,
                    bid,
                    ask,
                    timestamp_ms: 1,
                    source: "mutable-test".to_string(),
                })),
            }
        }

        fn set_quote(&self, symbol: &str, price: f64, bid: f64, ask: f64) {
            *self.quote.lock().unwrap_or_else(|e| e.into_inner()) = Quote {
                symbol: normalize_symbol(symbol),
                price,
                bid,
                ask,
                timestamp_ms: 2,
                source: "mutable-test".to_string(),
            };
        }
    }

    impl MarketDataAdapter for MutableMarketData {
        fn quote(&self, symbol: &str) -> MarketDataResult<Quote> {
            let mut quote = self.quote.lock().unwrap_or_else(|e| e.into_inner()).clone();
            quote.symbol = normalize_symbol(symbol);
            Ok(quote)
        }

        fn candles(
            &self,
            symbol: &str,
            interval: &str,
            _limit: usize,
        ) -> MarketDataResult<Vec<Candle>> {
            let quote = self.quote(symbol)?;
            Ok(vec![Candle {
                symbol: quote.symbol,
                interval: interval.to_string(),
                open: quote.price,
                high: quote.price,
                low: quote.price,
                close: quote.price,
                volume: 1.0,
                index: 0,
                source: quote.source,
            }])
        }

        fn depth(&self, symbol: &str, _limit: usize) -> MarketDataResult<OrderBook> {
            let quote = self.quote(symbol)?;
            Ok(OrderBook {
                symbol: quote.symbol,
                bids: vec![BookLevel {
                    price: quote.bid,
                    quantity: 1.0,
                }],
                asks: vec![BookLevel {
                    price: quote.ask,
                    quantity: 1.0,
                }],
                timestamp_ms: quote.timestamp_ms,
                source: quote.source,
            })
        }
    }

    struct FailingMarketData;

    impl MarketDataAdapter for FailingMarketData {
        fn quote(&self, _symbol: &str) -> MarketDataResult<Quote> {
            Err("quote unavailable".to_string())
        }

        fn candles(
            &self,
            _symbol: &str,
            _interval: &str,
            _limit: usize,
        ) -> MarketDataResult<Vec<Candle>> {
            Err("candles unavailable".to_string())
        }

        fn depth(&self, _symbol: &str, _limit: usize) -> MarketDataResult<OrderBook> {
            Err("depth unavailable".to_string())
        }
    }

    struct FakeHttpTransport {
        quote: Value,
        klines: Value,
        depth: Value,
    }

    impl JsonHttpTransport for FakeHttpTransport {
        fn get_json(&self, url: &str) -> MarketDataResult<Value> {
            if url.contains("/ticker/bookTicker") {
                Ok(self.quote.clone())
            } else if url.contains("/klines") {
                Ok(self.klines.clone())
            } else if url.contains("/depth") {
                Ok(self.depth.clone())
            } else {
                Err(format!("unexpected url: {url}"))
            }
        }
    }

    struct RejectAllRisk;

    impl RiskPolicy for RejectAllRisk {
        fn limits(&self) -> RiskLimits {
            RiskLimits {
                max_order_notional: 1.0,
                max_gross_exposure: 1.0,
                allow_short_selling: false,
            }
        }

        fn assess(
            &self,
            _account: &PaperAccountSnapshot,
            _req: &OrderRequest,
            _fill_price: f64,
            _commission_bps: f64,
        ) -> RiskAssessment {
            RiskAssessment {
                checks: vec![RiskCheck::failed("test_policy", "blocked by test policy")],
                rejection_reason: Some("blocked_by_test_policy".to_string()),
            }
        }
    }

    struct FailingAuditSink;

    impl AuditSink for FailingAuditSink {
        fn record(&self, _decision: &OrderDecision) -> Result<(), String> {
            Err("audit sink unavailable".to_string())
        }
    }

    #[test]
    fn runtime_accepts_custom_market_data_adapter() {
        let runtime = QuantRuntime::with_adapters(
            Arc::new(FixedMarketData),
            Arc::new(PaperBroker::default()),
        );

        let quote = runtime.quote("test").expect("quote");

        assert_eq!(quote.symbol, "TEST");
        assert_eq!(quote.price, 42.0);
        assert_eq!(quote.source, "fixed-test");
    }

    #[test]
    fn runtime_propagates_market_data_adapter_errors() {
        let runtime = QuantRuntime::with_adapters(
            Arc::new(FailingMarketData),
            Arc::new(PaperBroker::default()),
        );

        let err = runtime.quote("BTCUSDT").expect_err("quote should fail");
        assert_eq!(err, "quote unavailable");

        let err = runtime
            .run_backtest("BTCUSDT", "sma_cross(50,200)", 100_000.0)
            .expect_err("backtest should fail without candles");
        assert_eq!(err, "candles unavailable");

        let err = runtime
            .place_order(OrderRequest {
                symbol: "BTCUSDT".to_string(),
                side: OrderSide::Buy,
                order_type: OrderType::Market,
                quantity: 0.1,
                price: None,
            })
            .expect_err("market order should fail without quote");
        assert_eq!(err, "quote unavailable");
    }

    #[test]
    fn runtime_sync_marks_positions_to_latest_quote() {
        let market = MutableMarketData::new("AAPL", 100.0, 99.5, 100.5);
        let runtime =
            QuantRuntime::with_adapters(Arc::new(market.clone()), Arc::new(PaperBroker::default()));

        let decision = runtime
            .place_order(OrderRequest {
                symbol: "AAPL".to_string(),
                side: OrderSide::Buy,
                order_type: OrderType::Market,
                quantity: 10.0,
                price: None,
            })
            .expect("market buy fills");
        assert_eq!(decision.status, "filled_paper");

        market.set_quote("AAPL", 110.0, 109.5, 110.5);
        let decisions = runtime.sync_symbol("AAPL").expect("sync ok");
        assert!(decisions.is_empty());
        let portfolio = runtime.portfolio(Some("AAPL"));
        assert_eq!(portfolio.positions[0].market_price, 110.0);
        assert_eq!(portfolio.positions[0].market_value, 1100.0);
        assert_eq!(portfolio.positions[0].unrealized_pnl, 95.0);
        assert_eq!(portfolio.account.equity, 100095.0);
    }

    #[test]
    fn runtime_sync_fills_open_limit_order_when_quote_crosses() {
        let market = MutableMarketData::new("AAPL", 100.0, 99.5, 100.5);
        let runtime =
            QuantRuntime::with_adapters(Arc::new(market.clone()), Arc::new(PaperBroker::default()));

        let decision = runtime
            .place_order(OrderRequest {
                symbol: "AAPL".to_string(),
                side: OrderSide::Buy,
                order_type: OrderType::Limit,
                quantity: 10.0,
                price: Some(99.0),
            })
            .expect("limit accepted");
        assert_eq!(decision.status, "open_paper");
        assert_eq!(runtime.portfolio(Some("AAPL")).open_orders.len(), 1);

        market.set_quote("AAPL", 98.5, 98.0, 99.0);
        let decisions = runtime.sync_symbol("AAPL").expect("sync ok");
        assert_eq!(decisions.len(), 1);
        assert_eq!(decisions[0].status, "filled_paper");
        assert_eq!(
            decisions[0]
                .order
                .as_ref()
                .and_then(|order| order.fill_price),
            Some(99.0)
        );

        let portfolio = runtime.portfolio(Some("AAPL"));
        assert!(portfolio.open_orders.is_empty());
        assert_eq!(portfolio.positions[0].quantity, 10.0);
        assert_eq!(portfolio.positions[0].average_price, 99.0);
        assert_eq!(portfolio.positions[0].market_price, 98.5);
        assert_eq!(portfolio.order_history[0].status, "open_paper");
        assert_eq!(portfolio.order_history[1].status, "filled_paper");
    }

    #[test]
    fn open_buy_orders_reserve_buying_power_and_available_cash() {
        let runtime = QuantRuntime::with_adapters(
            Arc::new(FixedMarketData),
            Arc::new(PaperBroker::new_with_starting_cash(
                Arc::new(DefaultRiskPolicy::default()),
                100.0,
            )),
        );

        let first = runtime
            .place_order(OrderRequest {
                symbol: "AAPL".to_string(),
                side: OrderSide::Buy,
                order_type: OrderType::Limit,
                quantity: 2.0,
                price: Some(40.0),
            })
            .expect("first limit accepted");
        assert_eq!(first.status, "open_paper");
        assert_eq!(first.account.cash, 100.0);
        assert_eq!(first.account.available_cash, 20.0);
        assert_eq!(first.account.reserved_buy_notional, 80.0);
        assert_eq!(first.account.reserved_sell_notional, 0.0);
        assert_eq!(first.account.projected_gross_exposure, 80.0);
        assert_eq!(first.account.buying_power, 20.0);

        let second = runtime
            .place_order(OrderRequest {
                symbol: "AAPL".to_string(),
                side: OrderSide::Buy,
                order_type: OrderType::Limit,
                quantity: 1.0,
                price: Some(30.0),
            })
            .expect("risk rejection is structured");
        assert_eq!(second.status, "rejected_risk");
        assert!(
            second
                .rejection_reason
                .as_deref()
                .unwrap_or_default()
                .contains("insufficient_cash: required 30, available 20")
        );
    }

    #[test]
    fn account_summary_deserializes_legacy_order_history_without_reservation_fields() {
        let decision: OrderDecision = serde_json::from_value(json!({
            "decision_id": "DECISION-000000",
            "status": "filled_paper",
            "order": null,
            "rejection_reason": null,
            "risk_checks": [],
            "account": {
                "currency": "USD",
                "cash": 1000.0,
                "equity": 1000.0,
                "gross_exposure": 0.0,
                "buying_power": 1000.0
            },
            "audit_error": null,
            "persistence_error": null
        }))
        .expect("legacy account summary");

        assert_eq!(decision.account.cash, 1000.0);
        assert_eq!(decision.account.available_cash, 0.0);
        assert_eq!(decision.account.realized_pnl, 0.0);
        assert_eq!(decision.account.total_commission, 0.0);
        assert_eq!(decision.account.net_pnl, 0.0);
        assert_eq!(decision.account.projected_gross_exposure, 0.0);
        assert_eq!(decision.account.reserved_buy_notional, 0.0);
        assert_eq!(decision.account.reserved_buy_commission, 0.0);
        assert_eq!(decision.account.reserved_sell_notional, 0.0);
    }

    #[test]
    fn order_deserializes_legacy_json_without_commission() {
        let order: Order = serde_json::from_value(json!({
            "order_id": "PAPER-000000",
            "status": "filled_paper",
            "symbol": "AAPL",
            "side": "buy",
            "type": "market",
            "quantity": 1.0,
            "limit_price": null,
            "fill_price": 42.5,
            "filled_quantity": 1.0,
            "timestamp_ms": 1
        }))
        .expect("legacy order");

        assert_eq!(order.commission, 0.0);
    }

    #[test]
    fn paper_broker_applies_commission_to_cash_orders_and_net_pnl() {
        let runtime = QuantRuntime::with_adapters(
            Arc::new(FixedMarketData),
            Arc::new(PaperBroker::new_with_audit_starting_cash_and_commission(
                Arc::new(DefaultRiskPolicy::new(RiskLimits {
                    max_order_notional: 10_000.0,
                    max_gross_exposure: 10_000.0,
                    allow_short_selling: false,
                })),
                Arc::new(NoopAuditSink::default()),
                1_000.0,
                100.0,
            )),
        );

        let buy = runtime
            .place_order(OrderRequest {
                symbol: "AAPL".to_string(),
                side: OrderSide::Buy,
                order_type: OrderType::Market,
                quantity: 1.0,
                price: None,
            })
            .expect("buy fills");
        assert_eq!(buy.order.as_ref().unwrap().commission, 0.43);
        assert_eq!(buy.account.cash, 957.07);
        assert_eq!(buy.account.total_commission, 0.43);
        assert_eq!(buy.account.net_pnl, -0.43);

        let sell = runtime
            .place_order(OrderRequest {
                symbol: "AAPL".to_string(),
                side: OrderSide::Sell,
                order_type: OrderType::Market,
                quantity: 1.0,
                price: None,
            })
            .expect("sell fills");
        assert_eq!(sell.order.as_ref().unwrap().commission, 0.42);
        assert_eq!(sell.account.cash, 998.15);
        assert_eq!(sell.account.realized_pnl, -1.0);
        assert_eq!(sell.account.total_commission, 0.85);
        assert_eq!(sell.account.net_pnl, -1.85);
    }

    #[test]
    fn paper_broker_tracks_realized_pnl_when_long_position_is_closed() {
        let market = MutableMarketData::new("AAPL", 100.0, 99.5, 100.5);
        let runtime =
            QuantRuntime::with_adapters(Arc::new(market.clone()), Arc::new(PaperBroker::default()));

        runtime
            .place_order(OrderRequest {
                symbol: "AAPL".to_string(),
                side: OrderSide::Buy,
                order_type: OrderType::Market,
                quantity: 10.0,
                price: None,
            })
            .expect("buy fills");
        market.set_quote("AAPL", 110.0, 109.5, 110.5);
        let sell = runtime
            .place_order(OrderRequest {
                symbol: "AAPL".to_string(),
                side: OrderSide::Sell,
                order_type: OrderType::Market,
                quantity: 4.0,
                price: None,
            })
            .expect("sell fills");

        assert_eq!(sell.status, "filled_paper");
        assert_eq!(sell.account.realized_pnl, 36.0);
        let portfolio = runtime.portfolio(Some("AAPL"));
        assert_eq!(portfolio.positions[0].quantity, 6.0);
        assert_eq!(portfolio.account.realized_pnl, 36.0);
    }

    #[test]
    fn paper_broker_tracks_realized_pnl_when_short_position_is_covered() {
        let market = MutableMarketData::new("AAPL", 100.0, 99.5, 100.5);
        let runtime = QuantRuntime::with_adapters(
            Arc::new(market.clone()),
            Arc::new(PaperBroker::new(Arc::new(DefaultRiskPolicy::new(
                RiskLimits {
                    max_order_notional: 10_000.0,
                    max_gross_exposure: 10_000.0,
                    allow_short_selling: true,
                },
            )))),
        );

        runtime
            .place_order(OrderRequest {
                symbol: "AAPL".to_string(),
                side: OrderSide::Sell,
                order_type: OrderType::Market,
                quantity: 10.0,
                price: None,
            })
            .expect("short sell fills");
        market.set_quote("AAPL", 90.0, 89.5, 90.5);
        let cover = runtime
            .place_order(OrderRequest {
                symbol: "AAPL".to_string(),
                side: OrderSide::Buy,
                order_type: OrderType::Market,
                quantity: 4.0,
                price: None,
            })
            .expect("cover fills");

        assert_eq!(cover.status, "filled_paper");
        assert_eq!(cover.account.realized_pnl, 36.0);
        let portfolio = runtime.portfolio(Some("AAPL"));
        assert_eq!(portfolio.positions[0].quantity, -6.0);
        assert_eq!(portfolio.account.realized_pnl, 36.0);
    }

    #[test]
    fn open_sell_orders_reserve_inventory_when_short_selling_is_disabled() {
        let runtime = QuantRuntime::with_adapters(
            Arc::new(FixedMarketData),
            Arc::new(PaperBroker::new_with_starting_cash(
                Arc::new(DefaultRiskPolicy::default()),
                1_000.0,
            )),
        );

        runtime
            .place_order(OrderRequest {
                symbol: "AAPL".to_string(),
                side: OrderSide::Buy,
                order_type: OrderType::Market,
                quantity: 10.0,
                price: None,
            })
            .expect("buy fills");
        let first_sell = runtime
            .place_order(OrderRequest {
                symbol: "AAPL".to_string(),
                side: OrderSide::Sell,
                order_type: OrderType::Limit,
                quantity: 6.0,
                price: Some(45.0),
            })
            .expect("sell limit accepted");
        assert_eq!(first_sell.status, "open_paper");
        assert_eq!(first_sell.account.reserved_sell_notional, 270.0);
        assert_eq!(first_sell.account.projected_gross_exposure, 180.0);

        let second_sell = runtime
            .place_order(OrderRequest {
                symbol: "AAPL".to_string(),
                side: OrderSide::Sell,
                order_type: OrderType::Limit,
                quantity: 5.0,
                price: Some(46.0),
            })
            .expect("risk rejection is structured");
        assert_eq!(second_sell.status, "rejected_risk");
        assert!(
            second_sell
                .rejection_reason
                .as_deref()
                .unwrap_or_default()
                .contains("short_selling_disabled: requested 5, held 4")
        );
    }

    #[test]
    fn open_orders_count_toward_projected_gross_exposure_limit() {
        let runtime = QuantRuntime::with_adapters(
            Arc::new(FixedMarketData),
            Arc::new(PaperBroker::new_with_starting_cash(
                Arc::new(DefaultRiskPolicy::new(RiskLimits {
                    max_order_notional: 1_000.0,
                    max_gross_exposure: 100.0,
                    allow_short_selling: false,
                })),
                1_000.0,
            )),
        );

        let first = runtime
            .place_order(OrderRequest {
                symbol: "AAPL".to_string(),
                side: OrderSide::Buy,
                order_type: OrderType::Limit,
                quantity: 2.0,
                price: Some(40.0),
            })
            .expect("first limit accepted");
        assert_eq!(first.status, "open_paper");

        let second = runtime
            .place_order(OrderRequest {
                symbol: "MSFT".to_string(),
                side: OrderSide::Buy,
                order_type: OrderType::Limit,
                quantity: 1.0,
                price: Some(30.0),
            })
            .expect("risk rejection is structured");
        assert_eq!(second.status, "rejected_risk");
        assert!(
            second
                .rejection_reason
                .as_deref()
                .unwrap_or_default()
                .contains("gross_exposure_exceeds_limit: 110 > 100")
        );
    }

    #[test]
    fn binance_market_data_parses_quote_candles_and_depth() {
        let adapter = BinanceMarketData::with_transport(
            "https://example.test",
            FakeHttpTransport {
                quote: json!({
                    "symbol": "BTCUSDT",
                    "bidPrice": "64999.00",
                    "askPrice": "65001.00"
                }),
                klines: json!([[
                    1710000000000i64,
                    "64000.0",
                    "65100.0",
                    "63900.0",
                    "65000.0",
                    "123.4"
                ]]),
                depth: json!({
                    "bids": [["64999.0", "1.25"]],
                    "asks": [["65001.0", "1.10"]]
                }),
            },
        );

        let quote = adapter.quote("btcusdt").expect("quote");
        assert_eq!(quote.symbol, "BTCUSDT");
        assert_eq!(quote.price, 65000.0);
        assert_eq!(quote.source, "binance-http");

        let candles = adapter.candles("btcusdt", "1m", 1).expect("candles");
        assert_eq!(candles[0].close, 65000.0);
        assert_eq!(candles[0].volume, 123.4);

        let depth = adapter.depth("btcusdt", 5).expect("depth");
        assert_eq!(depth.bids[0].price, 64999.0);
        assert_eq!(depth.asks[0].quantity, 1.1);
    }

    #[test]
    fn binance_market_data_reports_malformed_payloads() {
        let adapter = BinanceMarketData::with_transport(
            "https://example.test",
            FakeHttpTransport {
                quote: json!({"symbol": "BTCUSDT", "bidPrice": "bad", "askPrice": "65001.00"}),
                klines: json!({}),
                depth: json!({}),
            },
        );

        let err = adapter.quote("btcusdt").expect_err("bad quote");
        assert!(err.contains("bidPrice"), "err: {err}");

        let err = adapter.candles("btcusdt", "1m", 1).expect_err("bad klines");
        assert!(err.contains("array"), "err: {err}");

        let err = adapter.depth("btcusdt", 5).expect_err("bad depth");
        assert!(err.contains("bids"), "err: {err}");
    }

    #[test]
    fn paper_broker_accepts_custom_risk_policy() {
        let runtime = QuantRuntime::with_adapters(
            Arc::new(SyntheticMarketData::default()),
            Arc::new(PaperBroker::new(Arc::new(RejectAllRisk))),
        );

        let decision = runtime
            .place_order(OrderRequest {
                symbol: "AAPL".to_string(),
                side: OrderSide::Buy,
                order_type: OrderType::Market,
                quantity: 1.0,
                price: None,
            })
            .expect("quote available");

        assert_eq!(decision.status, "rejected_risk");
        assert_eq!(
            decision.rejection_reason.as_deref(),
            Some("blocked_by_test_policy")
        );
        assert_eq!(decision.risk_checks[0].name, "test_policy");
    }

    #[test]
    fn short_selling_still_respects_projected_gross_exposure_limit() {
        let runtime = QuantRuntime::with_adapters(
            Arc::new(FixedMarketData),
            Arc::new(PaperBroker::new(Arc::new(DefaultRiskPolicy::new(
                RiskLimits {
                    max_order_notional: 10_000.0,
                    max_gross_exposure: 100.0,
                    allow_short_selling: true,
                },
            )))),
        );

        let decision = runtime
            .place_order(OrderRequest {
                symbol: "AAPL".to_string(),
                side: OrderSide::Sell,
                order_type: OrderType::Market,
                quantity: 3.0,
                price: None,
            })
            .expect("quote available");

        assert_eq!(decision.status, "rejected_risk");
        assert!(
            decision
                .rejection_reason
                .as_deref()
                .unwrap_or_default()
                .contains("gross_exposure_exceeds_limit")
        );
        assert!(
            decision
                .risk_checks
                .iter()
                .any(|check| check.name == "max_gross_exposure" && !check.passed)
        );
    }

    #[test]
    fn paper_broker_resets_average_price_when_position_flips_direction() {
        let runtime = QuantRuntime::with_adapters(
            Arc::new(MutableMarketData::new("AAPL", 100.0, 99.5, 100.5)),
            Arc::new(PaperBroker::new(Arc::new(DefaultRiskPolicy::new(
                RiskLimits {
                    max_order_notional: 10_000.0,
                    max_gross_exposure: 10_000.0,
                    allow_short_selling: true,
                },
            )))),
        );

        let short = runtime
            .place_order(OrderRequest {
                symbol: "AAPL".to_string(),
                side: OrderSide::Sell,
                order_type: OrderType::Market,
                quantity: 2.0,
                price: None,
            })
            .expect("short sell fills");
        assert_eq!(short.status, "filled_paper");

        let cover_and_flip = runtime
            .place_order(OrderRequest {
                symbol: "AAPL".to_string(),
                side: OrderSide::Buy,
                order_type: OrderType::Market,
                quantity: 3.0,
                price: None,
            })
            .expect("buy fills");
        assert_eq!(cover_and_flip.status, "filled_paper");

        let portfolio = runtime.portfolio(Some("AAPL"));
        assert_eq!(portfolio.positions[0].quantity, 1.0);
        assert_eq!(portfolio.positions[0].average_price, 100.5);
        assert_eq!(portfolio.positions[0].market_price, 100.5);
    }

    #[test]
    fn jsonl_audit_sink_records_filled_and_rejected_decisions() {
        let path = std::env::temp_dir().join(format!(
            "neenee-quant-audit-{}-{}.jsonl",
            std::process::id(),
            TEST_ID.fetch_add(1, Ordering::Relaxed)
        ));
        let runtime = QuantRuntime::with_adapters(
            Arc::new(SyntheticMarketData::default()),
            Arc::new(PaperBroker::new_with_audit(
                Arc::new(DefaultRiskPolicy::default()),
                Arc::new(JsonlAuditSink::new(&path)),
            )),
        );

        runtime
            .place_order(OrderRequest {
                symbol: "AAPL".to_string(),
                side: OrderSide::Buy,
                order_type: OrderType::Market,
                quantity: 1.0,
                price: None,
            })
            .expect("filled");
        runtime
            .place_order(OrderRequest {
                symbol: "BTCUSDT".to_string(),
                side: OrderSide::Buy,
                order_type: OrderType::Market,
                quantity: 2.0,
                price: None,
            })
            .expect("risk rejection");

        let log = std::fs::read_to_string(&path).expect("audit log readable");
        let lines: Vec<&str> = log.lines().collect();
        assert_eq!(lines.len(), 2, "log: {log}");
        assert!(lines[0].contains("\"status\":\"filled_paper\""));
        assert!(lines[1].contains("\"status\":\"rejected_risk\""));
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn audit_sink_errors_are_visible_in_decisions_and_history() {
        let runtime = QuantRuntime::with_adapters(
            Arc::new(SyntheticMarketData::default()),
            Arc::new(PaperBroker::new_with_audit(
                Arc::new(DefaultRiskPolicy::default()),
                Arc::new(FailingAuditSink),
            )),
        );

        let decision = runtime
            .place_order(OrderRequest {
                symbol: "AAPL".to_string(),
                side: OrderSide::Buy,
                order_type: OrderType::Market,
                quantity: 1.0,
                price: None,
            })
            .expect("filled despite audit error");
        assert_eq!(
            decision.audit_error.as_deref(),
            Some("audit sink unavailable")
        );

        let history = runtime.portfolio(None).order_history;
        assert_eq!(
            history[0].audit_error.as_deref(),
            Some("audit sink unavailable")
        );
    }
}
