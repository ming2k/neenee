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
| `query` | string | yes | Search query |

The default backend is **Exa** (`provider = "exa"`) with **Parallel** as the
fallback; both are hosted and need an API key. Other backends — `searxng`
(self-hosted, keyless), `tavily` (hosted, needs a key), and `duckduckgo`
(keyless scraping, frequently blocked) — are configurable in `[websearch]`
(`config.toml`).
