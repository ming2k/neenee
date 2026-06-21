# Web tools

Fetch URLs and search the web. Both are `Read`. Source:
`crates/neenee-tools/src/lib.rs`. Provider configuration lives in
`config.toml` under `[websearch]`.

### `webfetch`

| Parameter | Type | Required | Default | Notes |
|-----------|------|----------|---------|-------|
| `url` | string | yes | — | `http` or `https` |
| `raw` | boolean | no | `false` | Skip HTML-to-text |

### `websearch`

| Parameter | Type | Required | Notes |
|-----------|------|----------|-------|
| `query` | string | yes | DuckDuckGo query; no API key |
