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

`crates/neenee-tui/src/lib.rs` puts the terminal into application mode on
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

`crates/neenee-tui/src/document.rs` parses each message with
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
- Sticky headers pin an expanded card's header to the top of the
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
`ToolStep` cards, reasoning becomes `Thinking` cards, and permission
requests open a modal that resolves a harness waiter. Streaming itself
stays inside the harness; see [Harness architecture](harness.md).

## Where the details live

This page is intentionally conceptual. Exact component shapes, key
measurements, the color palette, and the file-to-responsibility table
live in the lookup reference:

- [Terminal UI reference](../reference/tui/) — frame layout, every
  component, color tokens, key measurements, source-file map.
- [Harness architecture](harness.md) — the control plane whose state the
  TUI projects.
- [Request flow](request-flow.md) — how streamed tokens reach the TUI
  over SSE.

[ratatui]: https://ratatui.rs
[pulldown-cmark]: https://docs.rs/pulldown-cmark
