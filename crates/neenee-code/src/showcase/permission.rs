//! Permission sheet showcase — the inline tool-permission prompt.
//!
//! Exercises the sheet across fixtures (bash, edit, mcp) and both modes
//! (normal 4-button, and the confirm-always sub-step). ←→ selects,
//! Enter activates (Details toggles the body; Allow-once / Always land on the
//! confirm-always gate; Reject exits), ↑↓ scrolls the expanded Details body.

use std::cell::{Cell, RefCell};
use std::io;

use crossterm::event::KeyCode;

use neenee_core::PermissionRequest;

use crate::showcase::common::{self, ShowAction, ShowEvent};
use crate::tui::layout::ModalHitMap;
use crate::tui::render::{Theme, draw_permission_sheet};

/// Permission fixtures spanning different tools, scopes, and argument shapes.
fn fixtures() -> Vec<PermissionRequest> {
    vec![
        PermissionRequest {
            id: "p1".into(),
            tool: "bash".into(),
            label: "bash".into(),
            description: "Run a shell command".into(),
            arguments: r#"{"command":"cargo test --package neenee-code"}"#.into(),
            scope: "*".into(),
        },
        PermissionRequest {
            id: "p2".into(),
            tool: "edit".into(),
            label: "edit".into(),
            description: "Edit a file by replacing old_string with new_string".into(),
            arguments: r#"{"path":"src/main.rs","old_string":"fn main()","new_string":"fn main() -> Result<()>"}"#.into(),
            scope: "src/main.rs".into(),
        },
        PermissionRequest {
            id: "p3".into(),
            tool: "mcp__fs__write_file".into(),
            label: "mcp: fs · write_file".into(),
            description: "MCP tool: write_file (filesystem server)".into(),
            arguments: r#"{"path":"/etc/hosts","content":"127.0.0.1 localhost\n"}"#.into(),
            scope: "/etc/hosts".into(),
        },
    ]
}

struct State {
    fx: Vec<PermissionRequest>,
    idx: usize,
    selected: usize, // 0=Allow, 1=Always, 2=Reject, 3=Details
    confirm_always: bool,
    show_details: bool,
    // `scroll` needs interior mutability: the renderer clamps it (reading the
    // old value and returning the new one) while `run_showcase` only hands the
    // render closure a `&State`.
    scroll: Cell<usize>,
    hit_map: RefCell<ModalHitMap>,
}

pub fn run() -> io::Result<()> {
    let fx = fixtures();
    let mut state = State {
        fx,
        idx: 0,
        selected: 0,
        confirm_always: false,
        show_details: false,
        scroll: Cell::new(0),
        hit_map: RefCell::new(ModalHitMap::new()),
    };
    let theme = Theme::default();

    common::run_showcase_events(
        &mut state,
        |f, s| {
            let mode = if s.confirm_always {
                "confirm-always"
            } else {
                "normal"
            };
            let title = format!(
                " permission sheet · fixture {}/{} ({mode}) · Tab=next · q/Ctrl+C=quit",
                s.idx + 1,
                s.fx.len(),
            );
            let hint = " ←→ select · Enter activate · ↑↓ scroll details · Esc back/quit";
            common::draw_with_chrome(f, &title, hint, &theme, |f| {
                // The sheet renders inline into the composer slot (bottom of
                // the screen), not centered. Give it the bottom ~40% as the
                // rect, mirroring how the real app positions it.
                let h = (f.area().height as usize / 2).max(6) as u16;
                let y = f.area().height.saturating_sub(h);
                let rect = neenee_tui::Rect::new(f.area().x, y, f.area().width, h);
                let mut hit_map = s.hit_map.borrow_mut();
                hit_map.clear();
                let clamped = draw_permission_sheet(
                    f,
                    &mut hit_map,
                    &s.fx[s.idx],
                    s.selected,
                    s.confirm_always,
                    s.show_details,
                    s.scroll.get(),
                    rect,
                    &theme,
                );
                s.scroll.set(clamped);
            });
        },
        |s, event| -> ShowAction {
            let key = match event {
                ShowEvent::Click { x, y } => {
                    let hit = { s.hit_map.borrow().permission_action_at(x, y) };
                    if let Some(hit) = hit {
                        s.selected = hit.action_index;
                        return activate(s);
                    }
                    return ShowAction::Continue;
                }
                ShowEvent::ScrollUp => {
                    if s.show_details && s.scroll.get() > 0 {
                        s.scroll.set(s.scroll.get().saturating_sub(1));
                    }
                    return ShowAction::Continue;
                }
                ShowEvent::ScrollDown => {
                    if s.show_details {
                        s.scroll.set(s.scroll.get() + 1);
                    }
                    return ShowAction::Continue;
                }
                ShowEvent::Key(key) => key,
            };
            match key.code {
                KeyCode::Tab => {
                    s.idx = (s.idx + 1) % s.fx.len();
                    s.selected = 0;
                    s.confirm_always = false;
                    s.show_details = false;
                    s.scroll.set(0);
                    ShowAction::Continue
                }
                KeyCode::Esc => {
                    // Esc backs out of the confirm-always gate; otherwise quits.
                    if s.confirm_always {
                        s.confirm_always = false;
                        s.selected = 1;
                        ShowAction::Continue
                    } else {
                        ShowAction::Exit
                    }
                }
                KeyCode::Left | KeyCode::Char('h') => {
                    if s.confirm_always {
                        s.selected = 0;
                    } else if s.selected > 0 {
                        s.selected -= 1;
                    }
                    ShowAction::Continue
                }
                KeyCode::Right | KeyCode::Char('l') => {
                    if s.confirm_always {
                        s.selected = 1;
                    } else if s.selected < 3 {
                        s.selected += 1;
                    }
                    ShowAction::Continue
                }
                KeyCode::Up | KeyCode::Char('k') => {
                    if s.show_details && s.scroll.get() > 0 {
                        s.scroll.set(s.scroll.get().saturating_sub(1));
                    }
                    ShowAction::Continue
                }
                KeyCode::Down | KeyCode::Char('j') => {
                    if s.show_details {
                        s.scroll.set(s.scroll.get() + 1);
                    }
                    ShowAction::Continue
                }
                KeyCode::Enter => activate(s),
                _ => ShowAction::Continue,
            }
        },
    )
}

fn activate(s: &mut State) -> ShowAction {
    if s.confirm_always {
        // 0=Confirm always (exit), 1=Cancel (back to normal).
        if s.selected == 0 {
            ShowAction::Exit
        } else {
            s.confirm_always = false;
            s.selected = 1;
            ShowAction::Continue
        }
    } else {
        match s.selected {
            // Allow once → exit.
            0 => ShowAction::Exit,
            // Always allow → confirm gate.
            1 => {
                s.confirm_always = true;
                s.selected = 0;
                ShowAction::Continue
            }
            // Reject → exit.
            2 => ShowAction::Exit,
            // Details → toggle.
            3 => {
                s.show_details = !s.show_details;
                s.scroll.set(0);
                ShowAction::Continue
            }
            _ => ShowAction::Continue,
        }
    }
}
