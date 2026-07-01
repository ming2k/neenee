# Interrupt semantics

A turn is not one indivisible operation — it is a pipeline with three
distinct phases, and an interrupt (`Esc` / `AgentOp::Interrupt`) means
something different in each. This page is the single reference for *what an
interrupt actually does* at each phase, *what survives in the conversation
context*, and *what it costs*. It is the design rationale behind two
intertwined decisions: neenee always uses streaming, and an interrupt is
treated as a three-phase, billing-aware event rather than a blunt kill.

For the turn lifecycle that the phases below carve up, see
[Turns and rounds](agent-design/turns-and-rounds.md). For how token counts
are normally booked on a *completed* turn, see
[Token accounting](agent-design/token-accounting.md).

## Why streaming (and why it matters here)

Every model request neenee makes is a streaming request (`stream: true`,
SSE). This is not a cosmetic choice for a nicer typing animation; it is the
foundation that makes interrupt semantics tractable. The relevant property
of streaming is this:

> Closing the client side of a streaming connection is a *signal the server
> acts on*. The provider detects the disconnect and stops generating
> within a few tokens. Closing a **non-streaming** request does no such
> thing — the server keeps generating to completion, you just are not there
> to receive it.

This distinction is why the three-phase model below is meaningful at all.
Under a non-streaming transport, "the request was sent" and "the server is
generating" collapse into one phase with no clean boundary, and an early
cancel saves no output tokens because the server finishes the whole
generation regardless. Under streaming, the client's read loop is also the
server's backpressure channel: dropping the stream is a genuine "stop"
instruction, acknowledged (after a small lag) on the server side.

This is also why neenee's [Token accounting](agent-design/token-accounting.md)
can under-report on an interrupted turn — the `usage` chunk that carries the
authoritative count is the *last* SSE event, emitted only after generation
completes, and an interrupt never reaches it. See [Interrupted turns and the
token ledger](#interrupted-turns-and-the-token-ledger) below.

## The three phases

A turn flows through three phases in strict order. An interrupt is
interpreted according to **which phase the turn is in at the instant `Esc`
fires**. The table below is the executive summary; each phase is then
explained in detail.

| Phase | What is happening | What an interrupt does | Context effect | Billing |
|-------|-------------------|------------------------|----------------|---------|
| **1. In-flight, pre-response** | Request sent; no bytes back yet | Cancel the request, **unsend** the user message | None — message returns to the input for re-editing; context reverted to pre-send | Input tokens of the *cancelled* request may still bill (request already left); but no assistant message, no output tokens |
| **2. Local, pre-remote** | Response streaming in; TUI rendering deltas | Drop the stream, discard the partial text | Partial assistant text is **dropped** (never pushed, never persisted); no marker inserted | Input tokens bill; output tokens generated so far bill; generation stops within a few tokens |
| **3. Remote / tool** | Assistant message committed; tools executing | Cancel tools, emit `ToolCancelled`, do not append results | Committed assistant message **stays**; tool results are **not** appended; round is not persisted | Input + committed output tokens bill; cancelled tools' side effects are best-effort stopped |

The naming is deliberate. **Local** vs **remote** refers to *where the
interruptible work has moved to*: in Phase 2 the work is the remote model
generation being rendered locally; in Phase 3 the work has moved fully onto
the server / tool side (a tool call is "remote" work driven by a committed
assistant decision). Phase 1 is the window where nothing remote has happened
*that we have evidence of* yet, so it is reversible.

### Phase 1 — In-flight, pre-response

This is the window between `provider.stream_chat_events(messages)` being
called and the first SSE event arriving. The harness races the request
against the cancel token:

```rust
let mut stream = tokio::select! {
    biased;
    _ = cancel.cancelled() => return Err(HarnessError::Interrupted),
    result = tokio::time::timeout(
        STREAM_IDLE_TIMEOUT,
        self.provider.stream_chat_events(messages.clone()),
    ) => match result { /* ... */ },
};
```

If `Esc` lands in this window the turn aborts *before the stream object
exists*, so nothing has been rendered and the cancellation is clean. This
window exists by construction: the `select!` is `biased` so the cancel arm
wins ties, and `STREAM_IDLE_TIMEOUT` bounds how long the harness will wait
for the provider to even open the connection.

**Design (implemented unsend):** because no evidence of a response has
reached the client, this phase is the only one where the turn is *truly
reversible* at the conversation layer. An interrupt here is treated as an
**unsend** rather than an abort.

The mechanism (`execute_turn` in `orchestration.rs`): on the error path, if
the result is `Err(Interrupted)` **and** the turn's `streamed_text` flag is
still `false` **and** no tool has run (`tool_activity` still `false`), the
harness pops the user message back out of `turn_history`, reverts the session
store with `replace_messages`, emits a `TurnEvent::UnsentInput { prompt,
images }`, and returns `Ok(false)` instead of propagating the error. The
`streamed_text` / `tool_activity` guards are what distinguish Phase 1 from
Phases 2/3: they are the cross-thread aggregates of "did any model output or
tool execution happen this turn", and they remain `false` exactly through the
Phase-1 window.

The TUI's response listener pops the matching user message from the
transcript and forwards the prompt via a one-shot signal
(`unsent_input_signal`) to the event loop, which restores it into the input
box — text and pasted images — so the user can re-edit and re-submit. The
conversation context ends up identical to the pre-send state. Hidden control
prompts (pursuit continuation, verify nudge) are not unsentable: they are
harness-internal and are never surfaced as editable user input.

This is billing-clean at the conversation layer (no assistant message enters
history, so no future turn re-sends phantom output, and the local token
ledger records zero), with one caveat: the HTTP request was already on the
wire, so the provider may still charge the *input* tokens for the cancelled
request. Phase 1 unsend saves you a bad conversation turn and all output
tokens, but it cannot un-send the network packet. See
[Billing reality](#billing-reality).

### Phase 2 — Local (stream rendering)

The stream is open and `ProviderStreamEvent` deltas are arriving. The
rendering loop tracks two sentinels — `emitted_text` and
`emitted_reasoning` — that record whether *anything* has been shown to the
UI yet:

```rust
loop {
    tokio::select! {
        biased;
        _ = cancel.cancelled() => return Err(HarnessError::Interrupted),
        event = tokio::time::timeout(STREAM_IDLE_TIMEOUT, stream.next()) => {
            // accumulate into `content` / `reasoning_content` / `calls`,
            // set emitted_text = true on first TextDelta, etc.
        }
    }
}
// ...only reachable if the loop completed normally:
self.book_turn_usage(&mut state, &response, streamed_usage.take());
messages.push(response.clone());
```

If `Esc` lands here, the `biased` cancel arm fires `return Err(Interrupted)`
**before** `messages.push(response)` and **before** `book_turn_usage`. The
consequences are precise and important:

- The accumulated `content`, `reasoning_content`, and `tool_calls` strings
  are **dropped on the floor**. They live only in local stack variables and
  are never converted into a `Message`.
- No assistant message enters `messages`, so it never enters `turn_history`,
  so it is never persisted and never sent in any future request's context.
- `book_turn_usage` is never called, so the token ledger records **zero**
  for this turn locally.
- On the wire, the SSE connection is dropped (the `stream` binding goes out
  of scope), which the provider treats as a stop signal.

**No marker is inserted.** neenee does *not* inject a "the previous response
was interrupted" note, system reminder, or `[ANSWER NO LONGER NEEDED]`
placeholder into the context. From the next turn's point of view, the model
never replied — the conversation simply ends with the user's prompt. The
only place an `[Interrupted]` string appears is as an ephemeral TUI render
signal (`TurnEvent::Text`), which is never persisted. This is a deliberate
choice: a marker would be an extra injected user/system turn that costs
tokens and can itself confuse the model, and the absence of a reply already
conveys "cut short" well enough.

### Phase 3 — Remote / tool execution

The assistant message was fully received and has been pushed to `messages`
at `:1696`. The harness is now in `dispatch_tool_calls`, running one or more
tools. This is "remote" in the sense that the work is now server/tool-side
and driven by a *committed* assistant decision that is already in history.

An interrupt here cannot undo the committed assistant message — it is
already in `messages` and (via the mid-turn save point) may already be on
disk. Instead the harness:

1. Emits a terminal `AgentEvent::ToolCancelled` for each in-flight tool, so
   the UI shows the cancellation rather than leaving tools hanging.
2. Does **not** append the `Role::Tool` result messages for the cancelled
   tools.
3. Does not persist the round via `append_round`.

The committed assistant message stays. The next turn therefore sees an
assistant turn that issued tool calls whose results never came back — which
is exactly the state provider sanitizers are built to clean up at
serialization time (see [Request flow](request-flow.md)): OpenAI-compatible
endpoints strip unanswered `tool_calls` (and drop the assistant message if
it becomes empty); Anthropic strips unanswered `tool_use` blocks. So the
history is self-healing across a Phase 3 interrupt: the committed message
may look incomplete in the local `Vec<Message>`, but it is reshaped into a
wire-valid form before any provider sees it.

## Interrupted turns and the token ledger

Because `book_turn_usage` sits after the streaming loop, an interrupt in
Phase 1 or Phase 2 means the turn is recorded as **zero tokens** locally —
the authoritative `usage` chunk never arrived and the local estimator never
ran on a finalized message. This is by design (there is no finalized message
to estimate), but it produces a known divergence:

> **The local token ledger under-counts interrupted turns; the real bill is
> higher than the ledger shows.**

Phase 3 interrupts are the exception: if the assistant message was pushed
before the interrupt, its usage *may* already have been captured if the
provider reported it mid-stream (`streamed_usage`), but typically the
`usage` event is the terminal one that got cut off, so Phase 3 is also
usually under-counted. The [Token accounting](agent-design/token-accounting.md)
page covers the reported-vs-estimated distinction for *completed* turns;
interrupted turns are simply un-reported, and there is no client-side way to
recover the exact number.

## Billing reality

An interrupt optimizes the *conversation* and the *local accounting*, not
the *invoice*. Three layers, three different truths:

**1. neenee's local ledger** — records zero for an interrupted turn (Phase 1
and 2) because `book_turn_usage` never runs. This is the number shown in the
UI. It is a lower bound.

**2. The provider's real invoice** — is computed server-side from what the
model actually processed and produced, independent of whether the client
read the result. Three rules govern it:

- **Input (prompt) tokens always bill.** The request left your machine; the
  provider parsed and embedded the entire prompt. Escaping cannot recall it.
  This is the dominant cost for long-context turns and is unaffected by how
  early you interrupt.
- **Output tokens generated up to the disconnect bill.** Generation is
  pipelined: the model produces tokens into a server-side buffer slightly
  ahead of what is on the wire. The tokens generated during this "detection
  lag" — a handful, typically — are produced and billed even though the
  client never rendered them.
- **Tokens never generated do not bill.** This is the whole point of
  streaming for interrupts: once the provider registers the dropped
  connection it halts generation within a few tokens, so the large body of
  output that *would* have been produced is never generated and never
  charged.

**3. Prompt caching** — adds a wrinkle. On Anthropic (and OpenAI's
automatic caching), the input tokens billed on the interrupted turn may
include cache-write (`cache_creation_input_tokens`) cost; the *next* request
with the same prefix then hits cache-read pricing (cheaper). So an early
interrupt on a fresh large context is disproportionately expensive relative
to its (zero) local output, but it primes the cache for the retried turn.
See [Token accounting](agent-design/token-accounting.md) for how neenee
tracks cache tokens when they *are* reported.

The unavoidable conclusion: **Escaping saves output tokens (the bigger the
would-be response, the more it saves) but cannot save input tokens, and the
savings ratio is worst exactly when input dominates** — long context, short
desired answer. Cost control therefore has two levers, and the interrupt is
only the second one:

- **Primary (input):** keep the context small — pruning, compaction,
  disabling unused tools, a shorter system prompt. This is where the money
  is on long sessions.
- **Secondary (output):** interrupt early when a response is clearly going
  wrong. Under streaming this genuinely stops generation and saves the
  un-generated output; under a non-streaming transport it would save
  nothing, which is the concrete reason neenee is streaming-only.

## Why no "interrupted" marker in context

A natural alternative to the current design is to record the fact of an
interrupt in the context so the model "knows" its previous turn was cut
short — e.g. append a system or user message like `"[The previous response
was interrupted by the user.]"`. neenee does not do this, for three
reasons:

1. **It costs tokens every time.** Every interrupt would inject a permanent
   message into the rolling context, inflating input cost on every
   subsequent turn — the exact cost the interrupt was meant to avoid.
2. **The omission is already informative.** A conversation that ends with a
   user prompt and no assistant reply reads, to the model, exactly like the
   start of a fresh reply to that prompt. There is no ambiguity to resolve.
3. **Markers can steer the model in unwanted ways.** A "you were
   interrupted" note invites the model to apologize, resume, or
   second-guess, which is rarely the desired behavior when the user
   interrupted because the answer was bad.

The trade-off is accepted consciously: if a future use case needs the model
to be aware of a partial answer (for example, to explicitly resume it), the
clean insertion point is in `execute_turn` (`orchestration.rs`) just after
the Phase-1 unsend check, where a marker message could be pushed into
`turn_history` before the write-back. The current design leaves that hook
unused.

## Summary

- neenee is streaming-only because streaming is what makes an interrupt a
  real "stop" signal to the provider rather than a dropped result.
- An interrupt is interpreted by phase: **Phase 1** (pre-response) is
  reversible at the conversation layer and **unsends** the user message back
  to the input box (gated on `streamed_text` + `tool_activity` both false);
  **Phase 2** (local rendering) drops the partial assistant text with no
  marker; **Phase 3** (tool execution) keeps the committed assistant message
  but drops the tool results, and provider serialization self-heals the
  dangling tool calls.
- Interrupted turns record **zero** in the local token ledger but are
  **not free** on the real invoice: input always bills, a few output
  tokens bill, the bulk of un-generated output does not.
- No "interrupted" marker is injected into context — omission is cheaper,
  clearer, and avoids steering the model.
