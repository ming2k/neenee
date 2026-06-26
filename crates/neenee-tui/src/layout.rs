//! Geometry primitives: `Rect`, `Margin`, `Constraint`, `Direction`, and
//! `Layout`.
//!
//! These mirror ratatui's layout API surface exactly (same field names, same
//! `Layout::split` semantics) so the migrated widget code needs no geometry
//! changes — only an import path swap from `ratatui::layout` to `neenee_tui`.

use std::cell::RefCell;

/// A rectangular region.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default)]
pub struct Rect {
    pub x: u16,
    pub y: u16,
    pub width: u16,
    pub height: u16,
}

impl Rect {
    pub const fn new(x: u16, y: u16, width: u16, height: u16) -> Self {
        Self {
            x,
            y,
            width,
            height,
        }
    }

    /// Returns a rect that has been shrunk by `margin.horizontal` on each
    /// side and `margin.vertical` on top and bottom.
    pub fn inner(self, margin: Margin) -> Rect {
        if self.width < 2 * margin.horizontal || self.height < 2 * margin.vertical {
            return Rect::new(self.x, self.y, 0, 0);
        }
        Rect::new(
            self.x + margin.horizontal,
            self.y + margin.vertical,
            self.width - 2 * margin.horizontal,
            self.height - 2 * margin.vertical,
        )
    }

    /// Clamp a point to be inside this rect. Used by the app to normalize
    /// mouse coordinates.
    pub fn contains(self, x: u16, y: u16) -> bool {
        x >= self.x && x < self.x + self.width && y >= self.y && y < self.y + self.height
    }

    /// Area (width × height) in cells.
    pub const fn area(self) -> u32 {
        self.width as u32 * self.height as u32
    }
}

/// A margin to apply when computing an inner rect.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct Margin {
    pub horizontal: u16,
    pub vertical: u16,
}

impl Margin {
    pub const fn new(horizontal: u16, vertical: u16) -> Self {
        Self {
            horizontal,
            vertical,
        }
    }
}

/// A layout constraint.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Constraint {
    /// Fill all remaining space.
    Min(u16),
    /// A fixed length.
    Length(u16),
    /// A percentage of the available space (0–100).
    Percentage(u16),
}

/// Layout direction.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum Direction {
    #[default]
    Vertical,
    Horizontal,
}

/// A layout solver: given a `Rect` and a list of `Constraint`s, `split`
/// returns the sub-rects. Mirrors ratatui's `Layout::default().direction()
/// .constraints().split()` API.
///
/// The solver implements the constraint-resolution algorithm ratatui uses:
/// `Length` is fixed; `Percentage` is the given fraction of total; `Min` fills
/// whatever is left. Multiple `Min` constraints split the remainder equally.
#[derive(Debug, Clone, Default)]
pub struct Layout {
    pub direction: Direction,
    pub constraints: Vec<Constraint>,
}

impl Layout {
    /// Create a default layout (vertical, no constraints).
    #[allow(clippy::should_implement_trait)]
    pub fn default() -> Self {
        Self {
            direction: Direction::Vertical,
            constraints: Vec::new(),
        }
    }

    pub fn direction(mut self, dir: Direction) -> Self {
        self.direction = dir;
        self
    }

    pub fn constraints(mut self, cs: impl IntoIterator<Item = Constraint>) -> Self {
        self.constraints = cs.into_iter().collect();
        self
    }

    /// Split `area` into sub-rects according to the constraints.
    pub fn split(self, area: Rect) -> RcRects {
        let n = self.constraints.len();
        if n == 0 || area.width == 0 || area.height == 0 {
            return RcRects {
                rects: RefCell::new(Vec::new()),
            };
        }
        let total = match self.direction {
            Direction::Vertical => area.height,
            Direction::Horizontal => area.width,
        };
        let mut rects = Vec::with_capacity(n);

        // First pass: compute fixed/percentage demands; count Min constraints.
        let mut fixed_sum: u16 = 0;
        let mut min_count: usize = 0;
        let mut demands: Vec<u16> = Vec::with_capacity(n);
        for c in &self.constraints {
            let demand = match c {
                Constraint::Length(l) => *l,
                Constraint::Percentage(p) => {
                    // Round to nearest, like ratatui.
                    ((*p as u32 * total as u32 + 50) / 100) as u16
                }
                Constraint::Min(m) => {
                    min_count += 1;
                    *m
                }
            };
            fixed_sum = fixed_sum.saturating_add(demand);
            demands.push(demand);
        }
        // Distribute remaining space among Min constraints.
        let remaining = total.saturating_sub(fixed_sum);
        let min_extra_each = if min_count > 0 {
            remaining / min_count as u16
        } else {
            0
        };
        let mut min_leftover = if min_count > 0 {
            remaining % min_count as u16
        } else {
            0
        };
        // Build final sizes.
        let mut sizes = Vec::with_capacity(n);
        for (i, c) in self.constraints.iter().enumerate() {
            match c {
                Constraint::Min(_) => {
                    let mut s = demands[i].saturating_add(min_extra_each);
                    if min_leftover > 0 {
                        s = s.saturating_add(1);
                        min_leftover -= 1;
                    }
                    sizes.push(s);
                }
                _ => sizes.push(demands[i]),
            }
        }

        // Position the rects along the relevant axis.
        let mut offset = 0u16;
        for &size in &sizes {
            rects.push(match self.direction {
                Direction::Vertical => Rect::new(area.x, area.y + offset, area.width, size),
                Direction::Horizontal => Rect::new(area.x + offset, area.y, size, area.height),
            });
            offset = offset.saturating_add(size);
        }

        RcRects {
            rects: RefCell::new(rects),
        }
    }
}

/// The result of `Layout::split`. Indexable like `Rc<[Rect]>` in ratatui.
/// Uses a `RefCell<Vec>` so the migration code's `chunks[i]` pattern works
/// without `Rc` plumbing.
pub struct RcRects {
    pub(crate) rects: RefCell<Vec<Rect>>,
}

impl std::ops::Index<usize> for RcRects {
    type Output = Rect;
    fn index(&self, i: usize) -> &Rect {
        // Borrow through a RefCell; return a raw pointer to satisfy &Output.
        // The returned reference is valid as long as the RcRects lives and no
        // other mutable borrow intervenes. In practice the app indexes the
        // split result immediately and never holds the borrow.
        let borrow = self.rects.borrow();
        let ptr = &borrow[i] as *const Rect;
        // SAFETY: the Vec lives for the lifetime of self; the caller does not
        // mutate the RcRects while holding this reference.
        unsafe { &*ptr }
    }
}

impl RcRects {
    pub fn iter(&self) -> std::vec::IntoIter<Rect> {
        self.rects.borrow().clone().into_iter()
    }
    pub fn len(&self) -> usize {
        self.rects.borrow().len()
    }
    pub fn is_empty(&self) -> bool {
        self.rects.borrow().is_empty()
    }
}
