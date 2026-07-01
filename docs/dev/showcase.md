# TUI component showcase

The showcase is a standalone, interactive playground for individual TUI
components — a "Storybook" for modals. It renders a single component in a
real terminal with a live event loop, so you can **see and interact with it**
without running the full agent/session/network stack.

This matters because most modal states are impossible to reach through normal
agent interaction: a permission `confirm-always` sub-step, a session-context
pane with a fully populated snapshot, an activity modal mid-round — these all
require a live agent to be in a specific state that you can't script. The
showcase lets you write fixture data and trigger any state directly.

## Quick start

```sh
# List all available showcases
neenee-code showcase

# Run one
neenee-code showcase question
neenee-code showcase permission
neenee-code showcase session
```

Every showcase responds to `q` or `Ctrl+C` to quit. These always work, even
mid-interaction, so you can never get trapped in a raw-mode terminal.

## Available showcases

| Component | What it exercises | Key bindings |
|-----------|-------------------|--------------|
| `question` | ask\_user modal: single-select (live — highlight is the selection), multi-select (Space toggle), multi-page | `↑↓` `Space` *(multi)* `1-9` `Enter` `Tab`=next fixture |
| `permission` | tool-permission sheet + confirm-always sub-step + Details scroll | `←→` `Enter` `↑↓` `Tab`=next fixture |
| `provider` | `/provider` model picker with filter | `↑↓` type to filter `Enter` |
| `model-editor` | API-key / model-id editor | `Tab` switch field, type to edit |
| `history` | `Ctrl+R` input-history fuzzy search | type to filter `↑↓` |
| `sessions` | session picker | `↑↓` |
| `session` | session-context tabbed modal (Model/MCP/Skills/Permissions/Tools) | `←→`/`Tab` cycle panes `↑↓` |
| `activity` | activity modal (pursuit + tasks + round/turn/status) | `←→`/`Tab` cycle tabs `↑↓` scroll |
| `help` | keybindings help (read-only) | `Esc` quit |
| `toast` | copy / armed toasts | `Tab` next variant |

## How it works

Each showcase is a thin glue layer: it owns a **state struct**, pumps real
crossterm keypresses through a **key handler**, and redraws via the
**production renderer** — the exact same `draw_*` function the real app uses.

```text
                  ┌─────────────────────────────────┐
   real keypress  │  State struct (fixture data)     │  render closure
   ──────────────▶│  + on_key closure (state machine)│──────────────▶ draw_*_modal()
   (crossterm)    │                                  │  (neenee-tui Frame)
                  └─────────────────────────────────┘
```

The shared runner ([`common::run_showcase`](../../crates/neenee-code/src/showcase/common.rs))
owns the terminal lifecycle (raw mode, alternate screen) and the event poll
loop. Every showcase passes it a `&mut State` plus two closures:

- **render closure** `|frame, &state|` — draws the chrome + the modal.
- **on\_key closure** `|&mut state, key|` — updates state from a keypress,
  returns `Continue` or `Exit`.

The `q` / `Ctrl+C` global kill-switch is handled inside the runner, so
individual showcases only handle their own keys.

## File layout

```text
crates/neenee-code/src/showcase/
├── mod.rs        # dispatcher: parses `showcase <component>`, routes
├── common.rs     # shared: terminal setup/teardown + run_showcase() + chrome
├── question.rs   # question modal — uses the MVU QuestionModel
├── permission.rs # permission sheet
└── simple.rs     # provider, model-editor, history, sessions,
                  # session-context, activity, help, toast
```

## Adding a new showcase

The showcase framework is designed so adding a component takes three small
steps. As a worked example, here's how you'd add a showcase for a hypothetical
new `confirmation` modal.

### 1. Write the showcase

Create a new file or add to `simple.rs`. Define a state struct and a `run()`
function:

```rust
use std::io;
use crossterm::event::KeyCode;

use crate::showcase::common::{self, ShowAction};
use crate::tui::render::{Theme, draw_confirmation_modal};

struct State {
    message: String,
    selected: usize, // 0 = OK, 1 = Cancel
}

pub fn run() -> io::Result<()> {
    let theme = Theme::default();
    let mut state = State {
        message: "Delete all temporary files?".into(),
        selected: 0,
    };

    common::run_showcase(
        &mut state,
        |f, s| {
            let title = " confirmation modal · q/Ctrl+C=quit";
            let hint = " ←→ select · Enter confirm · Esc quit ";
            common::draw_with_chrome(f, title, hint, &theme, |f| {
                draw_confirmation_modal(f, &s.message, s.selected, &theme);
            });
        },
        |s, key| match key.code {
            KeyCode::Esc => ShowAction::Exit,
            KeyCode::Left if s.selected > 0 => {
                s.selected -= 1;
                ShowAction::Continue
            }
            KeyCode::Right if s.selected < 1 => {
                s.selected += 1;
                ShowAction::Continue
            }
            KeyCode::Enter => ShowAction::Exit,
            _ => ShowAction::Continue,
        },
    )
}
```

Key points:

- **State struct** — holds all mutable bits. Both closures receive it (render
  gets `&State`, on\_key gets `&mut State`), so don't capture overlapping
  locals from the enclosing scope.
- **`Cell` for renderer-written fields** — if the renderer needs to clamp a
  value and write it back (e.g. `scroll`), wrap it in `Cell<usize>` since the
  render closure only gets `&State`. See `permission.rs` for a working example.
- **Fixture cycling** — if your component has multiple shapes, add a `Tab`
  handler that swaps the fixture, like `question.rs` does.

### 2. Register in the dispatcher

Add a match arm in `showcase/mod.rs`:

```rust
"confirmation" => confirmation::run().map_err(Into::into),
```

And declare the module if it's in its own file:

```rust
mod confirmation;
```

### 3. Verify

```sh
cargo run -p neenee-code -- showcase confirmation
cargo clippy -p neenee-code --tests
```

That's it — the new component is now in the `neenee-code showcase` list.

## The question modal and MVU

The `question` showcase is special: it doesn't just call a renderer, it drives
the [`QuestionModel`](../../crates/neenee-code/src/tui/question_model.rs) state
machine — the same pure `update()` function the production event loop uses.
This is possible because the question modal was refactored to
[Model-View-Update](https://guide.elm-lang.org/architecture/):

- **Model** (`QuestionModel`): the complete state of an open question modal.
- **View**: the production `draw_question_modal` renderer.
- **Update** (`QuestionModel::update_mut`): a pure state transition — feed it
  an action, get back the new state + effects, no I/O.

The showcase calls `model.update_mut(action)` directly, so what you see and
interact with is **exactly** what runs in production. This is the ideal: when a
component has an MVU extraction, the showcase is trivially correct because it
shares the production state machine. For components that still have inline
state in `event_loop.rs` (like permission), the showcase reimplements the key
handling as a faithful copy — correct enough for visual testing, but it can
drift from production if the event-loop arms change.
