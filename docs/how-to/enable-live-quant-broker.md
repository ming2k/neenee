# How to enable the live quant broker

Use `live-http` when `neenee-quant` should send orders to a broker gateway
instead of the built-in paper account.

The gateway owns exchange credentials and exchange-specific signing.
`neenee-quant` owns tool admission, local risk checks, audit records, and the
JSON contract described in the
[Configuration Reference](../reference/configuration.md#quant-runtime).

## Configure the gateway

1. Start a broker gateway that implements:

   - `GET /portfolio`
   - `POST /orders`
   - `POST /orders/{order_id}/cancel`

2. Export the live broker settings:

   ```bash
   export NEENEE_QUANT_BROKER=live-http
   export NEENEE_QUANT_LIVE_BROKER_URL=https://broker.example.com
   export NEENEE_QUANT_LIVE_BROKER_TOKEN_ENV=BROKER_TOKEN
   export BROKER_TOKEN=replace-with-a-secret-token
   ```

3. Set conservative risk limits before starting the GUI or agent:

   ```bash
   export NEENEE_QUANT_RISK_MAX_ORDER_NOTIONAL=1000
   export NEENEE_QUANT_RISK_MAX_GROSS_EXPOSURE=5000
   export NEENEE_QUANT_RISK_ALLOW_SHORT_SELLING=false
   export NEENEE_QUANT_AUDIT_LOG="$HOME/.local/state/neenee/quant-audit.jsonl"
   ```

4. Start the quant GUI:

   ```bash
   cargo run -p neenee-quant-gui --features gui
   ```

5. Open the Portfolio view and refresh positions before arming trading.

## Verify the safety path

Submit an order above `NEENEE_QUANT_RISK_MAX_ORDER_NOTIONAL`. The result should
be `rejected_risk`, and the broker gateway should not receive `POST /orders`.

For normal orders, inspect the audit log. Each accepted, rejected, or gateway
failed decision is written as one JSON object per line.
