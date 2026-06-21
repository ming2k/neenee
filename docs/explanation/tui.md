# Terminal UI

neenee runs as a full-screen terminal application, not a line-oriented
command loop. This page explains *how* the TUI is built and *why* it can do
things that raw terminal text cannot: live streaming, mouse selection,
modal sheets, and a status surface that never looks frozen. For the
component-by-component lookup reference, see
[Terminal UI reference](../reference/tui/).

## From terminal text to terminal application

A classic CLI is a **line discipline** affair: the kernel line-edits
input, the program writes ordered bytes to `stdout`, the terminal paints
them onto a scrolling grid, and selection copies whatever characters
happen to be on that grid. Output is append-only; interaction is a
question/answer prompt; `Ctrl+C` means `SIGINT`.

The neenee TUI abandons that model for the **full-screen application**
model used by editors and multiplexers:

| Concern | Line-oriented CLI | neenee TUI |
|---------|-------------------|------------|
| Screen | Primary buffer, scrolling history | Alternate screen, restored on exit |
| Input | Kernel line editing, `readline` | Raw bytes read directly, edited in-process |
| Output | Append characters, scroll up | Repaint the whole frame each tick |
| Selection | Grid characters, lost on scroll | Semantic document ranges, stable across redraws |
| `Ctrl+C` | `SIGINT` | Context action: copy → interrupt → close modal → clear → quit |

Every capability below follows from that shift.

## Terminal underpinnings

`crates/neenee-cli/src/tui/mod.rs` puts the terminal into application mode on
startup and undoes it on exit:

```rust
enable_raw_mode()?;
execute!(
    stdout,
    EnterAlternateScreen,
    EnableMouseCapture,
    PushKeyboardEnhancementFlags(KeyboardEnhancementFlags::DISAMBIGUATE_ESCAPE_CODES)
)?;
```

Each call removes one limitation of the line-oriented terminal:

- `enable_raw_mode` disables canonical line editing so the program reads
  key events as they arrive instead of waiting for Enter.
- `EnterAlternateScreen` switches to a private buffer so the transcript never
  pollutes the shell's scrollback and the shell is restored on exit.
- `EnableMouseCapture` delivers selection, click, and wheel events.
- `DISAMBIGUATE_ESCAPE_CODES` requests the Kitty enhanced-keyboard
  protocol so modifier-bearing keys that collide with legacy control
  bytes (notably `Ctrl+M`, which is otherwise indistinguishable from
  Enter) are reported distinctly. crossterm only emits the request when
  the terminal advertises support, so this is a no-op elsewhere.

A signal guard (`spawn_signal_guard`) catches `SIGTERM`, `SIGINT`,
`SIGHUP`, and `SIGQUIT`, then calls `restore_terminal()`. Without it, an
external `pkill neenee` would terminate the process before normal
cleanup, leaving the host terminal stranded in raw mode with mouse
capture on, so every mouse motion would spew SGR escape codes into the
shell.

## Immediate-mode rendering

Rendering is built on [ratatui] using the **immediate-mode** pattern:
the program does not keep a persistent grid buffer that it patches. Each
frame, the entire UI is rebuilt from current state and drawn in one
pass:

```text
sync shared state → terminal.draw(rebuild widgets) → drain input events
```

Because the frame is a pure function of state, anything that changes
state — a streamed token, a permission request, a mouse drag — shows up
on the very next frame with no manual invalidation. That is what makes
the surface feel live rather than printed.

Input events are drained in a batch. The first event blocks for the poll
interval; any further events the terminal has already queued are read
with a zero-timeout poll and share a single redraw. Pasting a paragraph
therefore repaints once instead of once per character.

## Two channels: streaming producer, frame consumer

Provider streaming and rendering run on separate tasks so streaming
speed is never gated by frame rate, and a slow frame never drops tokens.

A background tokio task owns the response receiver and pushes updates
into shared state guarded by `Arc<Mutex<…>>`:

```rust
tokio::spawn(async move {
    while let Some(resp) = rx.recv().await {
        match resp {
            AgentResponse::StreamDelta(delta) => {
                let mut msgs = messages_clone.lock().await;
                if let Some(last) = msgs.last_mut() { last.push_stream(&delta); }
            }
            AgentResponse::ToolCall { id, name, arguments } => { /* … */ }
            AgentResponse::PermissionRequest(request) => { /* … */ }
            // …
        }
    }
});
```

The main loop never holds that lock while rendering. Each frame it takes
a snapshot — `app.messages = runtime.messages.lock().await.clone()` —
and draws against the snapshot. Streaming can therefore update the model
as fast as the network allows while the UI repaints on its own cadence.

## The semantic document model

This is the single biggest difference from terminal text. A line-oriented
program emits a string; the terminal wraps it and the user can only copy
the wrapped result. neenee keeps a **structured document** instead.

`crates/neenee-cli/src/tui/document.rs` parses each message with
[pulldown-cmark] into a `Vec<Block>` and tags it with a `MessageKind`:

```rust
pub enum MessageKind {
    Text,
    ToolStep { id, name, arguments, output, expanded, duration_ms, started_at, children },
    Thinking { content, duration_ms, expanded },
}

pub enum Block {
    Text { content },
    Code { language, content },
    Heading { level, content },
    ListItem { content, ordered, depth, checked },
    Quote { content },
    Table { headers, rows, aligns, rendered },
    Rule,
    Break,
}
```

Two properties fall out of this. First, copy returns the **original**
text, not the terminal-wrapped projection of it: a table cell copies as
clean cell text, a code block copies its source. Second, the structure
is addressable — there is a stable notion of "the third block of message
seven" that survives any change of terminal width or scroll position.

## Semantic cursor and the layout map

Addressing is how mouse interaction becomes meaningful. Coordinates live
in the document, not on the screen:

```rust
pub struct SemanticCursor {
    pub message_idx: usize,
    pub block_idx: usize,
    pub byte_offset: usize,
}
```

During each draw, the renderer records where every block lands on the
grid into a `LayoutMap` of `BlockRegion` entries (`message_idx`,
`block_idx`, byte range, screen `Rect`). Tables additionally register
per-cell hit boxes. The map is the bridge in both directions:

- **Draw → screen**: blocks produce `BlockRegion`s as they are painted.
- **Screen → document**: a mouse point is resolved by hit-testing the
  regions back to a `SemanticCursor`.

Because selection is stored as a `SemanticCursor` range rather than as
screen coordinates, a selection stays correct after the terminal is
resized or the content reflows. The renderer can repaint freely; the
selection refers to the document, which is unaffected. `get_selected_text`
then walks the block model to produce copyable text, stripping box-drawing
borders from rendered tables.

## The live state layer

Beyond rendering content, the TUI maintains state whose only purpose is
to communicate that the agent is busy:

- A monotonic `spinner_tick` advances once per frame and drives the
  breathing-dot indicator (a luminance sweep, not a braille spinner), so
  the status bar animates at roughly 10 fps even while the
  harness is waiting on a slow provider.
- An `activity_status` string surfaces the current phase
  (`responding`, `thinking`, `retry 2/4 in 3s`, `awaiting permission`).
- `follow_bottom` keeps the newest content in view while streaming and
  yields to manual control the moment the user scrolls.
- Sticky headers pin an expanded step's header to the top of the
  viewport, and transient toasts report outcomes (`copied`,
  `press Ctrl+C again to exit`).

None of this changes the conversation; all of it prevents the UI from
looking frozen during long network waits.

## Avoiding event-loop stalls

Two slow operations are pushed off the render loop so they can never
freeze it:

- **Clipboard** reads and writes (`arboard`, `wl-copy`, OSC 52) run as
  spawned background tasks. Their results return through channels and are
  applied on a later frame, with a short 16 ms poll while a copy is
  pending so the `copied` toast appears promptly.
- **Provider state** lives behind a mutex that the render loop holds only
  long enough to snapshot, never while drawing or waiting on input.

## How it fits the harness

The TUI is a pure projection of harness state. The background listener
maps each `AgentResponse` variant onto document changes — streaming text
grows the live assistant message, tool calls become collapsible
`ToolStep` steps, reasoning becomes `Thinking` steps, and permission
requests open a modal that resolves a harness waiter. Streaming itself
stays inside the harness; see [Harness architecture](agent-design/harness.md).

## UX design philosophy

The TUI's job is not to look impressive in a screenshot — it is to keep a
long-running, partially-autonomous agent legible. Most visual decisions
follow from three constraints: the agent produces output faster than the
user can read it, many tool calls are noise once they finish, and the
surface must never look frozen during a multi-second provider wait. The
policies that fall out of those constraints are documented here so future
changes do not silently regress them.

### Calm defaults, loud exceptions

The default weight for a collapsed step is the muted foreground tone, not
the primary one. A finished `Ok` tool step is therefore visually inert —
it earns attention only when the user opens it or rests the pointer on it.
Loudness is reserved for the lifecycles the user actually has to act on or
acknowledge: a call still `Running`, one that `Failed`, or one that was
`Denied` permission. Those carry a steady accent that wins outright over
the weight channel, so they stay visible even when collapsed and idle.
This separation is enforced as an invariant of the
[step state machine](../reference/tui/step-state.md): accent overrides
weight, never the reverse.

### Disclosure is the user's, not the lifecycle's

The `user_pinned` flag on every step is the single boundary between
automatic and manual disclosure. Lifecycle transitions may set the
default — `Failed` expands, `Running` collapses, reasoning expands while
streaming — but the moment the user manually toggles a step, the flag goes
true and later transitions no-op. There is no "auto-collapse what the user
was just reading" path; a finished reasoning trace is left exactly where
the user had it. This is what prevents the historical class of bug where
"the model finished thinking and yanked away the content I was reading."

### Two focus zones instead of two modes

The application deliberately does not have a `vi`-style modal-vs-insert
split. Instead there are two **focus zones** — Compose (the input box) and
Browse (the transcript) — and the same key means one thing per zone:
`Enter` sends in Compose and toggles in Browse, `↑`/`↓` move the caret in
Compose and walk steps in Browse. `Ctrl+B` is the explicit zone toggle and
the hint bar carries a `[ COMPOSE ]` / `[ BROWSE ]` pill. The benefit is
that keyboard navigation never silently changes meaning, and the user never
has to remember "which mode am I in" to predict what a key will do.

### A breathing dot, not a spinner

The activity indicator is a single dot whose luminance sweeps between the
summary background and the status accent at roughly 10 fps, not a braille
`⠋⠙⠹` cycle. Braille spinners read as "the program is computing" and are
easy to mistake for an unresponsive loop; a slow luminance sweep reads as
"the program is waiting" — which is almost always the truth during a
provider round trip.

The breathing dot is also the **single** motion anchor in the TUI: every
other running indicator (tool-step summary, reasoning marker, goal bar)
holds a steady accent, never a luminance sweep. Concentrating all of the
motion budget in one place preserves the dot's role as a peripheral
"system is alive" cue — if every component breathed in unison, the dot
would lose its isolation and stop functioning as an anchor. Per-step
liveness is carried by hue (`info` while running, `error_fg` on failure,
`text_muted` when cancelled) and by marker shape (`●` while a trace is
streaming, `+`/`-` once it finishes). See
[ADR-0008](../adr/0008-single-breathing-anchor.md).

### Sticky headers instead of scroll anchoring

When an expanded step's body scrolls past the top of the viewport, the
step's header is re-rendered pinned to the top row of the transcript area
with a `-` marker.
The alternative — anchor the scroll position so the header stays in view
— would fight the user the moment they explicitly scrolled away from it.
Pinning preserves the user's scroll intent while still answering "which
step am I looking at the body of?" The same machinery keeps the header
anchored to its row on toggle, so expanding a step does not push its own
header off-screen.

### Selection is a document range, not a screen rectangle

Mouse selection is stored as a `SemanticCursor` range (`message_idx`,
`block_idx`, byte range), never as terminal row/column coordinates. The
consequence the user notices is that a selection survives a terminal
resize, a reflow, or any number of redraws: the renderer repaints freely,
the selection refers to the document model, and `get_selected_text` walks
the block model to produce clean copyable text — stripping box-drawing
borders from rendered tables, restoring the original code-block source
rather than the wrapped projection. This is the single biggest difference
from line-oriented terminal text.

### Never block the frame

The render loop is independent of provider speed and clipboard round-trip
time by construction. The mechanism — spawned tasks for clipboard I/O, a
snapshot-only mutex for provider state, a short poll while a copy is
pending — is documented under [Avoiding event-loop stalls](#avoiding-event-loop-stalls);
the design intent is simply that the cursor must keep blinking and the
activity indicator must keep sweeping during a multi-second network wait.

## Where the details live

This page is intentionally conceptual. Exact component shapes, key
measurements, the color palette, and the file-to-responsibility table
live in the lookup reference:

- [Terminal UI reference](../reference/tui/) — frame layout, every
  component, color tokens, key measurements, source-file map.
- [Step state machine](../reference/tui/step-state.md) — the formal
  state diagrams for the disclosure / interaction / lifecycle axes.
- [Harness architecture](agent-design/harness.md) — the control plane whose state the
  TUI projects.
- [Request flow](request-flow.md) — how streamed tokens reach the TUI
  over SSE.

[ratatui]: https://ratatui.rs
[pulldown-cmark]: https://docs.rs/pulldown-cmark
