# Token accounting

neenee measures context pressure in **tokens** because that is the unit every
model's context window is denominated in. A turn's token count drives the three
context-projection layers — [pruning](context-pruning.md) →
[compaction](context-compaction.md) → overflow recovery — and the live meter in
the TUI's hint bar. Getting that number *approximately* right is what keeps the
agent from silently overflowing the window or, conversely, compacting far too
early.

This page is the single reference for **how neenee counts tokens**: the
two-source model (upstream-reported vs. locally-estimated), the char-class
estimator that backs the local path, the ledger that attributes every token to
its source, and the report modal that surfaces it all to the user. For the
layers that *consume* the count, see the compaction and pruning pages; for the
request flow that carries the usage object, see [Request flow](../request-flow.md).

## The two-source problem

A model provider knows exactly how many tokens a request consumed — it computes
the count while serving the request and returns it in a `usage` object. A
client that wants an accurate picture should use that number. But three things
get in the way of "just use the provider's number":

1. **Not every provider returns usage.** A local relay, a minimal OpenAI-compatible
   server, or a provider that strips the field simply has nothing to report.
2. **The number arrives *after* the request completes.** The harness needs a
   pressure estimate *during* a turn (between tool rounds) to decide whether to
   prune, and at that point the current request's usage is not back yet.
3. **The number is for one request, not the running total.** Each `usage` object
   describes one round-trip's `prompt_tokens` + `completion_tokens`; the
   context pressure is roughly the *next* request's `prompt_tokens`, which is the
   size of the accumulated window.

neenee resolves this with a **layered** policy: prefer the upstream number when
it exists, fall back to a local estimator otherwise, and *attribute* every token
to its source so the user can see which counts are authoritative and which are
guesses.

## The priority chain

At the single booking point (`Agent::book_turn_usage`,
`crates/neenee-agent/src/agent.rs`), each turn's usage is resolved through this
chain, in order:

```
streamed Usage event  ──▶  take_last_usage()  ──▶  estimate_message_tokens()
(OpenAI include_usage,     (non-streaming chat       (local char-class
 Anthropic message_delta)   residual, Gemini)         estimator)
       │                          │                          │
       ▼                          ▼                          ▼
   reported: true            reported: true            reported: false
   (authoritative)           (authoritative)           (heuristic)
```

1. **Streamed `Usage` event.** When streaming, providers that support it emit a
   terminal usage chunk: OpenAI's `include_usage` terminal chunk, Anthropic's
   `message_delta.usage`. These arrive as `ProviderStreamEvent::Usage` and are
   captured into `streamed_usage` as the stream runs.
2. **`Provider::take_last_usage()`.** For non-streaming chat, or when the stream
   did not carry usage, the provider stashes the `usage` object internally and
   hands it out here. This is a *consume-once* drain: the value is cleared after
   reading so it can never be double-counted.
3. **`estimate_message_tokens()` — the local estimator.** The final fallback,
   used whenever the provider reported nothing. Described in detail below.

Whichever source wins, the count is added to the turn's `TokenUsage` and —
crucially — recorded into the **token-source ledger** tagged as *reported* (the
first two) or *estimated* (the third). That tag is what the report modal shows.

### Why "streamed first, then drained"?

The two upstream paths are not mutually exclusive but are ordered for a reason:
a streaming turn that emitted a `Usage` event already holds the authoritative
number in `streamed_usage`, so there is no need to also drain the provider's
stash (which may be empty or stale from a prior request). The `or_else` chain
makes "we already have the streamed number" short-circuit cleanly.

## The local char-class estimator

When no upstream usage is available, neenee estimates locally. The estimator
lives in `crates/neenee-core/src/pressure.rs` (`count_tokens`) and replaces the
old flat `bytes / 4` heuristic that the codebase carried for years.

### Why `bytes / 4` was wrong

The old estimator divided the UTF-8 byte length by four. That ratio is a
reasonable average for **English prose** — English words average about four
characters and BPE merges them into one token. But neenee's conversations are
seldom pure English prose: they are dense with **Chinese/CJK** and **source
code**, and for both `bytes / 4` breaks badly:

- **CJK is severely under-counted.** A Chinese ideograph is almost always *one
  token* in modern tokenizers (BPE vocabularies are trained on English-dominant
  corpora, so CJK glyphs rarely merge). But UTF-8 encodes one glyph as *three
  bytes*, so `bytes / 4` turns four Chinese characters (`人工智能`, ≈4 tokens)
  into `12 / 4 = 3` — and worse, a longer Chinese sentence can be under-counted
  by 3–4×, so the meter reads "30% full" when the window is actually near
  capacity.
- **Code is unevenly over-counted in spots and under-counted in others.** Code
  is full of brackets, operators, and indentation that BPE tends to split into
  single tokens, making it denser than `bytes / 4` predicts; but long
  identifiers (`getUserSettingsFromDatabase`) merge more than the heuristic
  assumes. The net error is large and unstable.

### The char-class model

The estimator classifies each Unicode scalar into a category and adds a
*fractional* per-character token weight, accumulating in fixed-point integer
math (scaled by 256) and rounding once at the end. It is a single O(n) pass
with no external vocabulary.

| Category | Weight (tokens/char) | Rationale |
|----------|:----:|-----------|
| ASCII letter / digit / whitespace | 0.25 | English baseline: BPE merges ~4 chars/token |
| CJK ideograph, kana, Hangul | 1.0 | Almost one token per glyph |
| CJK + fullwidth punctuation (。，、？！) | 1.0 | Low-frequency, usually its own token |
| Other non-ASCII letters (é, а, λ) | 0.5 | ~2 chars/token, denser than ASCII |
| Code punctuation `(){}[];` `=+-*/` | 1.0 | Dense, rarely merges with neighbors |
| Other ASCII punctuation (`. , " '`) | 0.5 | Merges more than operators, denser than words |

The CJK ranges covered are: CJK Unified Ideographs (+ Extension A/B–F),
Hiragana, Katakana (incl. halfwidth), Hangul Syllables, CJK Radicals, CJK
Compatibility Ideographs, and fullwidth ASCII letters/digits — everything a
modern tokenizer splits per-glyph.

The net effect: a 4-character Chinese phrase now estimates as **4 tokens** (not
1), a line of code estimates higher than prose of the same length, and plain
English stays close to the old `bytes / 4` number.

### Where the estimator is *not* used

The estimator measures the **content the provider will receive** (message
`content` + tool-call names/arguments, recursively including nested envoy
transcripts). It deliberately excludes:

- **`reasoning_content`** (extended thinking) — never sent to the provider, so
  it does not consume the window. *(Note: the TUI's hint-bar meter includes it
  as an intentional upper bound — see "Display vs. runtime" below.)*
- **Framing overhead** — per-message role tags, the chat template the serving
  runtime applies, system-prompt token counts from the provider's tokenizer.
  These are unknowable without the real tokenizer, so the estimator ignores
  them; this is the main reason the upstream number is always preferred when
  available.

## The token-source ledger

To make the reported-vs-estimated distinction observable, neenee keeps a running
`TokenSourceLedger` (`crates/neenee-core/src/token_ledger.rs`):

```
TokenSourceLedger
  └─ (provider_id, model) ─▶ { reported_tokens, estimated_tokens }
```

Every booked turn increments one of the two counters under its `(provider, model)`
key. A session that switches providers or models keeps each one's accuracy
picture separate. The ledger is a thread-safe shared `Arc` held jointly by:

- **The agent** (writer) — `book_turn_usage` records each turn.
- **The TUI** (reader) — the report modal calls `snapshot()` to render.

`snapshot()` returns a `TokenSourceReport`: a sorted list of rows
(provider/model/totals) plus a grand total, with no lock held during render.

## The Token Source report modal

The hint bar's context meter — the `89.2k (8%)` indicator pinned to the
bottom-right — is now **clickable**. Clicking it opens a centered, read-only
**Token Source Report** modal:

```
┌─ Token Source Report ─────────────────────────────────────┐
│ Provider / Model          Reported    Estimated    % Real │
│ ──────────────────────────────────────────────────────────│
│ openai · gpt-4o              12.3k          0       100%  │
│ gemini · gemini-2.5           8.1k          0       100%  │
│ kimi · k2                        0        2.4k         0% │
│ ──────────────────────────────────────────────────────────│
│ Total                        20.4k        2.4k        89% │
│                                                            │
│ Reported = authoritative counts from the provider's usage │
│ Estimated = local char-class heuristic (provider reported  │
│             no usage).                                     │
│                                              Esc close     │
└────────────────────────────────────────────────────────────┘
```

The report answers two questions at a glance:

- **"How accurate is my context meter?"** — the `% Real` column. `100%` means
  every token for that model came from the provider; `0%` means it was all
  estimated (e.g. a local relay that strips `usage`). The grand total's
  percentage is the session-wide accuracy.
- **"Which of my providers actually report usage?"** — the row breakdown makes
  it obvious which models are measured and which are guessed, so a user
  debugging a premature-overflow or never-compacts issue knows whether to look
  at the estimator or at the provider.

The percentage colors signal accuracy at a glance: green when fully reported,
yellow when mixed, muted/red when all estimated.

## Display vs. runtime: two numbers, on purpose

There are two token-counting paths, and they intentionally disagree slightly:

| Path | Includes reasoning? | Used for |
|------|:---:|----------|
| **Runtime** (`estimate_tokens` / `book_turn_usage`) | No | Pruning & compaction threshold decisions |
| **Display** (hint-bar `estimate_context_tokens`) | Yes | The on-screen meter the user glances at |

The runtime path excludes `reasoning_content` because that text is never sent to
the provider — it consumes zero window. The display path includes it because the
meter is a *user-facing* signal of "how much is in my transcript", and reasoning
blocks are visible bulk the user might want to be aware of. So the displayed
number is an intentional **upper bound**; the decision number is the precise
one. The Token Source report reflects the runtime (decision) accounting.

## How each provider surfaces usage

The upstream path only works because each provider adapter now actually parses
the `usage` object it previously discarded:

| Provider | Non-streaming | Streaming |
|----------|---------------|-----------|
| **Anthropic** (`anthropic_compat.rs`) | top-level `usage.input_tokens` / `output_tokens` | `message_delta` event's cumulative `usage` → `Usage` event |
| **OpenAI-compat** (`openai_compat.rs`) | top-level `usage.{prompt,completion,total}_tokens` | requests `stream_options.include_usage`; terminal chunk's `usage` → `Usage` event |
| **Gemini** (`gemini.rs`) | `usageMetadata.{prompt,candidates,total}TokenCount` | *(same non-streaming path)* |

Each adapter implements `Provider::usage_supported() -> true` and stashes the
parsed `TokenUsage` in an internal `Mutex`, drained by `take_last_usage()`. The
`ProxyProvider` (which fronts the hot-swappable provider) delegates both methods
to the live inner provider so attribution tracks the active model even after a
mid-session `/provider` switch.

Providers that never report usage (test doubles, a relay that strips the field)
keep the trait's default: `usage_supported() -> false`, `take_last_usage() ->
None`. The booking chain falls straight through to the estimator, and the ledger
records those turns as estimated — exactly as the report shows.

## The `CHARS_PER_TOKEN` constant, and why it still exists

`pressure::CHARS_PER_TOKEN = 4` remains in the codebase, which can look
contradictory after reading the above. It is retained because the **reverse**
direction — converting a *token* budget into a *character* budget — still uses
it, deliberately:

- `summary_char_budget` turns a target token count into a max character budget
  for a compaction summary.
- `prune_protect_chars` turns a protect-token budget into characters to shield.

In that direction (tokens → characters) the flat ratio is a **safe over-estimate
of characters**: it gives the summarizer more room and the protector a wider
shield, which are both conservative. The char-class estimator is only better
than `bytes / 4` in the forward direction (text → tokens); for budget sizing the
flat constant is fine and changing it would shift every compaction threshold. So
the constant lives on as a one-way conversion factor, clearly distinct from the
estimator.
