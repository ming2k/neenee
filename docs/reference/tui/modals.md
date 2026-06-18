# Modals

All modals are centered overlays. When a modal is open, the header, input box,
status bar, and hint line are hidden (`chrome_hidden`).

## Overview

| Modal | Trigger | Purpose | Input visible? |
|-------|---------|---------|----------------|
| Models | `Ctrl+M` / `/models` | Select LLM provider preset | No |
| Sessions | `/sessions` / `neenee resume` | Resume a past session | No |
| History search | `Ctrl+R` | Search input history | Yes (search query) |
| Permission | Automatic | Approve write tool calls | No |
| API key | Models modal `k` key | Enter provider API key | Yes (masked) |
| Endpoint | Custom provider flow | Enter OpenAI-compatible URL | Yes |
| Model name | Custom provider flow | Enter model ID | Yes |
| Help | `Ctrl+H` / `/help` | Read-only keybinding reference | No |

## Closing

- `Esc` or `Ctrl+C` closes most modals.
- Permission modal: `Esc` rejects; `Ctrl+C` closes and rejects.
- API-key / Endpoint / Model-name modals: `Ctrl+C` restores the stashed input
  and exits the configuration flow.

## Models modal

Centered list of provider presets. `↑`/`↓` navigate, `Enter` selects, `k`
configures the API key for the highlighted provider.

## Sessions modal

Centered list of past sessions showing overview text, timestamps, and message
count. `↑`/`↓` navigate, `Enter` resumes the selected session.

## History search modal

Filters input history by the current input text. `↑`/`↓` navigate, `Enter`
inserts the selected history entry into the input box.

## Permission modal

Appears when a write-capable tool (Build mode) requires approval. Offers Allow
once / Always allow / Reject. `↑`/`↓` or `←`/`→` navigate, `Enter` confirms.
"Always allow" requires a second confirmation step.

## API key / Endpoint / Model name modals

Sequential input flow for custom provider configuration. These modals borrow the
input line — the user types into the regular input box, and the text is captured
when `Enter` is pressed. The API-key modal masks the display with `•••`.

## Help modal

Read-only keybinding reference. `Esc` or `Enter` closes.

## Source

`draw_models_modal`, `draw_sessions_modal`, `draw_history_modal`,
`draw_permission_sheet`, `draw_api_key_modal`, `draw_solution_input_modal`,
`draw_help_modal` in `render.rs`.
