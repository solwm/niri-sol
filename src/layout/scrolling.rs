//! Master-stack tiling engine.
//!
//! This file replaces the original scrollable-tiling engine with a master-stack layout while
//! preserving the outward API so the rest of niri keeps compiling. Many of the more elaborate
//! behaviours (animations, interactive resize, gestures, fullscreen, rendering details) are
//! stubbed with `todo!()` for now and will be implemented incrementally.

use std::rc::Rc;
use std::time::Duration;

use sol_config::{PresetSize, Struts};
use sol_ipc::{ColumnDisplay, SizeChange, WindowLayout};
use smithay::backend::renderer::gles::GlesRenderer;
use smithay::utils::{Logical, Point, Rectangle, Scale, Serial, Size};

use super::closing_window::{ClosingWindow, ClosingWindowRenderElement};
use super::monitor::InsertPosition;
use super::tile::{Tile, TileRenderElement};
use super::workspace::InteractiveResize;
use super::{HitType, LayoutElement, Options, RemovedTile};
use crate::animation::{Animation, Clock};
use crate::layout::SizingMode;
use crate::niri_render_elements;
use crate::render_helpers::renderer::NiriRenderer;
use crate::render_helpers::xray::{Xray, XrayPos};
use crate::render_helpers::RenderCtx;
use crate::utils::transaction::{Transaction, TransactionBlocker};
use crate::utils::ResizeEdge;
use crate::window::ResolvedWindowRules;

/// Master-stack space for windows.
#[derive(Debug)]
pub struct ScrollingSpace<W: LayoutElement> {
    /// The master column (left side, full height). `None` if the workspace is empty.
    master: Option<Column<W>>,

    /// The stack columns (right side, vertically tiled).
    stack: Vec<Column<W>>,

    /// What is currently focused.
    focus: Focus,

    /// Stack index that had focus before we last moved left to master.
    /// Restored when the user moves right again from master, so the cursor
    /// returns to the same row in the stack instead of always landing on
    /// the top one. Cleared / clamped when the stack changes shape.
    last_stack_idx: Option<usize>,

    /// Master width as a proportion of the working area (0.0..=1.0).
    master_ratio: f64,

    /// Ongoing interactive resize.
    interactive_resize: Option<InteractiveResize<W>>,

    /// Windows in the closing animation.
    closing_windows: Vec<ClosingWindow>,

    /// View size for this space.
    view_size: Size<f64, Logical>,

    /// Working area for this space.
    working_area: Rectangle<f64, Logical>,

    /// Working area for this space excluding struts.
    parent_area: Rectangle<f64, Logical>,

    /// Scale of the output the space is on.
    scale: f64,

    /// Clock for driving animations.
    clock: Clock,

    /// Configurable properties of the layout.
    options: Rc<Options>,

    /// Always Static(0.0) for the master-stack engine; kept for API compatibility.
    view_offset: ViewOffset,
}

niri_render_elements! {
    ScrollingSpaceRenderElement<R> => {
        Tile = TileRenderElement<R>,
        ClosingWindow = ClosingWindowRenderElement,
    }
}

#[derive(Debug, Clone, Copy)]
enum Focus {
    Empty,
    Master,
    Stack(usize),
}

#[derive(Debug)]
pub(super) enum ViewOffset {
    Static(f64),
}

#[derive(Debug)]
pub(super) struct ViewGesture {}

#[derive(Debug)]
pub struct Column<W: LayoutElement> {
    /// The single tile owned by this column.
    tile: Tile<W>,
    /// Desired width (kept for API compatibility).
    width: ColumnWidth,
    /// Whether this column should ignore its width and span the entire view width.
    is_full_width: bool,
    /// Pending fullscreen state of the contained tile.
    is_pending_fullscreen: bool,
    /// Pending maximized state of the contained tile.
    is_pending_maximized: bool,
    /// Latest known view size for this column's workspace.
    view_size: Size<f64, Logical>,
    /// Latest known working area for this column's workspace.
    working_area: Rectangle<f64, Logical>,
    /// Working area excluding struts.
    parent_area: Rectangle<f64, Logical>,
    /// Scale of the output.
    scale: f64,
    /// Clock for driving animations.
    clock: Clock,
    /// Configurable properties of the layout.
    options: Rc<Options>,
}

/// Width of a column.
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum ColumnWidth {
    /// Proportion of the current view width.
    Proportion(f64),
    /// Fixed width in logical pixels.
    Fixed(f64),
}

/// Height of a window in a column.
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum WindowHeight {
    Auto { weight: f64 },
    Fixed(f64),
}

/// Horizontal direction for an operation.
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum ScrollDirection {
    Left,
    Right,
}

impl<W: LayoutElement> ScrollingSpace<W> {
    pub fn new(
        view_size: Size<f64, Logical>,
        parent_area: Rectangle<f64, Logical>,
        scale: f64,
        clock: Clock,
        options: Rc<Options>,
    ) -> Self {
        let working_area = compute_working_area(parent_area, scale, options.layout.struts);

        Self {
            master: None,
            stack: Vec::new(),
            focus: Focus::Empty,
            last_stack_idx: None,
            master_ratio: 0.5,
            interactive_resize: None,
            closing_windows: Vec::new(),
            view_size,
            working_area,
            parent_area,
            scale,
            clock,
            options,
            view_offset: ViewOffset::Static(0.0),
        }
    }

    pub fn update_config(
        &mut self,
        view_size: Size<f64, Logical>,
        parent_area: Rectangle<f64, Logical>,
        scale: f64,
        options: Rc<Options>,
    ) {
        let working_area = compute_working_area(parent_area, scale, options.layout.struts);

        if let Some(master) = &mut self.master {
            master.update_config(view_size, working_area, parent_area, scale, options.clone());
        }
        for col in &mut self.stack {
            col.update_config(view_size, working_area, parent_area, scale, options.clone());
        }

        self.view_size = view_size;
        self.working_area = working_area;
        self.parent_area = parent_area;
        self.scale = scale;
        self.options = options;

        self.update_tile_sizes();
    }

    pub fn update_shaders(&mut self) {
        if let Some(m) = &mut self.master {
            m.update_shaders();
        }
        for col in &mut self.stack {
            col.update_shaders();
        }
    }

    pub fn advance_animations(&mut self) {
        if let Some(m) = &mut self.master {
            m.advance_animations();
        }
        for col in &mut self.stack {
            col.advance_animations();
        }
        self.closing_windows.retain_mut(|closing| {
            closing.advance_animations();
            closing.are_animations_ongoing()
        });
    }

    pub fn are_animations_ongoing(&self) -> bool {
        self.master
            .as_ref()
            .is_some_and(Column::are_animations_ongoing)
            || self.stack.iter().any(Column::are_animations_ongoing)
            || !self.closing_windows.is_empty()
    }

    pub fn are_transitions_ongoing(&self) -> bool {
        self.master
            .as_ref()
            .is_some_and(Column::are_transitions_ongoing)
            || self.stack.iter().any(Column::are_transitions_ongoing)
            || !self.closing_windows.is_empty()
    }

    pub fn update_render_elements(&mut self, is_active: bool) {
        let focused_idx = self.active_column_idx_internal();

        // Takeover: a single column is pending fullscreen/maximized — it fills the relevant
        // area (view for fullscreen, working area for maximized) and other tiles are hidden.
        if let Some((take_idx, area)) = self.takeover() {
            if let Some(col) = self.column_mut_by_unified_idx(take_idx) {
                let col_active = is_active && Some(take_idx) == focused_idx;
                col.update_render_elements(col_active, area);
            }
            return;
        }

        let layout = self.column_layout();
        for ((unified_idx, pos, size), col) in
            layout.into_iter().zip(self.columns_mut())
        {
            let col_active = is_active && Some(unified_idx) == focused_idx;
            let view_rect = Rectangle::new(pos, size);
            col.update_render_elements(col_active, view_rect);
        }
    }

    /// If a tile is pending fullscreen/maximized, return its unified idx and the rect it
    /// should occupy. Fullscreen → entire view; maximized → working area.
    fn takeover(&self) -> Option<(usize, Rectangle<f64, Logical>)> {
        for (idx, col) in self.columns().enumerate() {
            if col.is_pending_fullscreen {
                return Some((idx, Rectangle::from_size(self.view_size)));
            }
            if col.is_pending_maximized {
                return Some((idx, self.working_area));
            }
        }
        None
    }

    /// Apply the master-stack layout: ask each tile to size itself to its slot in the layout.
    fn update_tile_sizes(&mut self) {
        let layout = self.column_layout();
        for ((_, _, size), col) in layout.into_iter().zip(self.columns_mut()) {
            if col.is_pending_fullscreen || col.is_pending_maximized {
                continue;
            }
            col.tile.request_tile_size(size, false, None);
        }
    }

    pub fn tiles(&self) -> impl Iterator<Item = &Tile<W>> + '_ {
        self.master
            .iter()
            .map(|c| &c.tile)
            .chain(self.stack.iter().map(|c| &c.tile))
    }

    pub fn tiles_mut(&mut self) -> impl Iterator<Item = &mut Tile<W>> + '_ {
        self.master
            .iter_mut()
            .map(|c| &mut c.tile)
            .chain(self.stack.iter_mut().map(|c| &mut c.tile))
    }

    pub fn is_empty(&self) -> bool {
        self.master.is_none() && self.stack.is_empty()
    }

    fn focused_column(&self) -> Option<&Column<W>> {
        match self.focus {
            Focus::Empty => None,
            Focus::Master => self.master.as_ref(),
            Focus::Stack(idx) => self.stack.get(idx),
        }
    }

    fn focused_column_mut(&mut self) -> Option<&mut Column<W>> {
        match self.focus {
            Focus::Empty => None,
            Focus::Master => self.master.as_mut(),
            Focus::Stack(idx) => self.stack.get_mut(idx),
        }
    }

    pub fn active_window(&self) -> Option<&W> {
        self.focused_column().map(|c| c.tile.window())
    }

    pub fn active_window_mut(&mut self) -> Option<&mut W> {
        self.focused_column_mut().map(|c| c.tile.window_mut())
    }

    pub fn active_tile_mut(&mut self) -> Option<&mut Tile<W>> {
        self.focused_column_mut().map(|c| &mut c.tile)
    }

    pub fn is_active_pending_fullscreen(&self) -> bool {
        self.focused_column()
            .is_some_and(|c| c.is_pending_fullscreen)
    }

    pub fn new_window_toplevel_bounds(&self, _rules: &ResolvedWindowRules) -> Size<i32, Logical> {
        let gaps = self.options.layout.gaps;
        Size::from((
            f64::max(self.working_area.size.w - gaps * 2., 1.),
            f64::max(self.working_area.size.h - gaps * 2., 1.),
        ))
        .to_i32_floor()
    }

    pub fn new_window_size(
        &self,
        _width: Option<PresetSize>,
        _height: Option<PresetSize>,
        _rules: &ResolvedWindowRules,
    ) -> Size<i32, Logical> {
        // Predict the layout slot the new window will land in. If master is empty, the new
        // window becomes master and gets the full working area (no stack to share with). Otherwise
        // it joins the stack and shares the right column with the existing stack tiles.
        let work = self.working_area;
        let (w, h) = if self.master.is_none() {
            (work.size.w, work.size.h)
        } else {
            let new_stack_count = self.stack.len() + 1;
            (
                work.size.w * (1.0 - self.master_ratio),
                work.size.h / new_stack_count as f64,
            )
        };
        Size::from((f64::max(w, 1.), f64::max(h, 1.))).to_i32_floor()
    }

    pub fn is_centering_focused_column(&self) -> bool {
        false
    }

    pub(super) fn insert_position(&self, _pos: Point<f64, Logical>) -> InsertPosition {
        if self.master.is_none() {
            InsertPosition::NewColumn(0)
        } else {
            InsertPosition::NewColumn(self.stack.len() + 1)
        }
    }

    pub fn add_tile(
        &mut self,
        _col_idx: Option<usize>,
        tile: Tile<W>,
        activate: bool,
        width: ColumnWidth,
        is_full_width: bool,
        _anim_config: Option<sol_config::Animation>,
    ) {
        let column = Column::new_with_tile(
            tile,
            self.view_size,
            self.working_area,
            self.parent_area,
            self.scale,
            width,
            is_full_width,
        );
        self.add_column(None, column, activate, None);
    }

    pub fn add_tile_to_column(
        &mut self,
        _col_idx: usize,
        _tile_idx: Option<usize>,
        tile: Tile<W>,
        activate: bool,
    ) {
        // master-stack: each column holds one tile, so adding into a column degrades to adding
        // a new column (push to stack or place into master).
        let column = Column::new_with_tile(
            tile,
            self.view_size,
            self.working_area,
            self.parent_area,
            self.scale,
            ColumnWidth::Proportion(self.master_ratio),
            false,
        );
        self.add_column(None, column, activate, None);
    }

    pub fn add_tile_right_of(
        &mut self,
        _right_of: &W::Id,
        tile: Tile<W>,
        activate: bool,
        width: ColumnWidth,
        is_full_width: bool,
    ) {
        self.add_tile(None, tile, activate, width, is_full_width, None);
    }

    pub fn add_column(
        &mut self,
        _idx: Option<usize>,
        mut column: Column<W>,
        activate: bool,
        _anim_config: Option<sol_config::Animation>,
    ) {
        // A new window arriving forces the workspace back into normal
        // tiling: any column that was in fullscreen / maximized takeover
        // drops the flag, so the new window doesn't appear hidden
        // underneath the takeover and the layout returns to master+stack.
        let wa_size = self.working_area.size;
        if let Some(m) = &mut self.master {
            let _ = m.set_fullscreen(false);
            let _ = m.set_maximized(false, wa_size);
        }
        for col in &mut self.stack {
            let _ = col.set_fullscreen(false);
            let _ = col.set_maximized(false, wa_size);
        }

        column.update_config(
            self.view_size,
            self.working_area,
            self.parent_area,
            self.scale,
            self.options.clone(),
        );

        if self.master.is_none() {
            self.master = Some(column);
            if activate || matches!(self.focus, Focus::Empty) {
                self.focus = Focus::Master;
            }
        } else {
            let new_idx = self.stack.len();
            self.stack.push(column);
            if activate {
                self.focus = Focus::Stack(new_idx);
            } else if matches!(self.focus, Focus::Empty) {
                self.focus = Focus::Master;
            }
        }

        self.update_tile_sizes();
    }

    pub fn remove_active_tile(&mut self, transaction: Transaction) -> Option<RemovedTile<W>> {
        match self.focus {
            Focus::Empty => None,
            Focus::Master => {
                let id = self.master.as_ref()?.tile.window().id().clone();
                Some(self.remove_tile(&id, transaction))
            }
            Focus::Stack(idx) => {
                let id = self.stack.get(idx)?.tile.window().id().clone();
                Some(self.remove_tile(&id, transaction))
            }
        }
    }

    pub fn remove_tile(&mut self, window: &W::Id, transaction: Transaction) -> RemovedTile<W> {
        // Find the column index in the unified order: 0 = master, 1.. = stack.
        let unified_idx = self
            .columns()
            .position(|col| col.tile.window().id() == window)
            .expect("remove_tile: window not found");
        self.remove_tile_by_idx(unified_idx, 0, transaction, None)
    }

    pub fn remove_tile_by_idx(
        &mut self,
        column_idx: usize,
        _tile_idx: usize,
        _transaction: Transaction,
        _anim_config: Option<sol_config::Animation>,
    ) -> RemovedTile<W> {
        let column = self.take_column_at(column_idx);
        super::RemovedTile {
            // SAFETY: cannot construct RemovedTile from outside layout/, but we are inside it.
            // We assemble it by exposing visible fields through helper.
            tile: column.tile,
            width: column.width,
            is_full_width: column.is_full_width,
            is_floating: false,
        }
    }

    /// Removes the column at unified index (0 = master, 1.. = stack[idx-1]) and fixes focus.
    fn take_column_at(&mut self, column_idx: usize) -> Column<W> {
        // Snapshot every surviving column's pre-removal slot so we can
        // spring them into their new slots after the layout reshuffles.
        // Without this, the surviving tiles snap instantly while the
        // closing-window snapshot fades out at the old position —
        // visually disconnected.
        let pre: Vec<(W::Id, Point<f64, Logical>)> = self
            .column_layout()
            .into_iter()
            .filter(|(uidx, _, _)| *uidx != column_idx)
            .filter_map(|(uidx, pos, _)| {
                self.column_by_unified_idx(uidx)
                    .map(|col| (col.tile.window().id().clone(), pos))
            })
            .collect();

        let removed = if column_idx == 0 {
            // Master removal: promote stack[0] if any, else master becomes None.
            let removed = self.master.take().expect("master expected");
            if !self.stack.is_empty() {
                self.master = Some(self.stack.remove(0));
            }
            self.update_focus_after_removal(0);
            removed
        } else {
            let removed = self.stack.remove(column_idx - 1);
            self.update_focus_after_removal(column_idx);
            removed
        };

        self.update_tile_sizes();

        // Spring each surviving tile from its old slot to its new slot
        // — same `tile_movement` config the master↔stack swap uses, so
        // both kinds of layout mutation feel consistent.
        let anim_cfg = self.options.animations.tile_movement.0;
        let post_layout = self.column_layout();
        for (id, old_pos) in pre {
            let new_pos = post_layout.iter().find_map(|(uidx, pos, _)| {
                self.column_by_unified_idx(*uidx)
                    .and_then(|c| (c.tile.window().id() == &id).then_some(*pos))
            });
            let Some(new_pos) = new_pos else {
                continue;
            };
            let delta = old_pos - new_pos;
            if delta.x.abs() < 0.5 && delta.y.abs() < 0.5 {
                continue;
            }
            if let Some(col) = self.find_column_by_id_mut(&id) {
                if delta.x.abs() >= 0.5 {
                    col.tile.animate_move_x_from_with_config(delta.x, anim_cfg);
                }
                if delta.y.abs() >= 0.5 {
                    col.tile.animate_move_y_from_with_config(delta.y, anim_cfg);
                }
            }
        }

        removed
    }

    fn update_focus_after_removal(&mut self, removed_idx: usize) {
        // Re-target focus so it stays valid.
        if self.master.is_none() && self.stack.is_empty() {
            self.focus = Focus::Empty;
            return;
        }
        match self.focus {
            Focus::Empty => {
                if self.master.is_some() {
                    self.focus = Focus::Master;
                } else if !self.stack.is_empty() {
                    self.focus = Focus::Stack(0);
                }
            }
            Focus::Master => {
                if self.master.is_none() {
                    if !self.stack.is_empty() {
                        self.focus = Focus::Stack(0);
                    } else {
                        self.focus = Focus::Empty;
                    }
                }
            }
            Focus::Stack(idx) => {
                // After removal, indices shift if removed_idx <= idx.
                let stack_removed_idx = removed_idx.saturating_sub(1);
                if removed_idx == 0 {
                    // Master was removed; stack[0] became master, all stack indices shifted down.
                    if idx == 0 {
                        self.focus = Focus::Master;
                    } else {
                        let new_idx = idx - 1;
                        if new_idx < self.stack.len() {
                            self.focus = Focus::Stack(new_idx);
                        } else if !self.stack.is_empty() {
                            self.focus = Focus::Stack(self.stack.len() - 1);
                        } else {
                            self.focus = Focus::Master;
                        }
                    }
                } else if stack_removed_idx == idx {
                    if idx > 0 {
                        self.focus = Focus::Stack(idx - 1);
                    } else if !self.stack.is_empty() {
                        self.focus = Focus::Stack(0);
                    } else if self.master.is_some() {
                        self.focus = Focus::Master;
                    } else {
                        self.focus = Focus::Empty;
                    }
                } else if stack_removed_idx < idx {
                    self.focus = Focus::Stack(idx - 1);
                }
            }
        }
    }

    pub fn remove_active_column(&mut self) -> Option<Column<W>> {
        match self.focus {
            Focus::Empty => None,
            Focus::Master => Some(self.take_column_at(0)),
            Focus::Stack(idx) => Some(self.take_column_at(idx + 1)),
        }
    }

    pub fn remove_column_by_idx(
        &mut self,
        column_idx: usize,
        _anim_config: Option<sol_config::Animation>,
    ) -> Column<W> {
        self.take_column_at(column_idx)
    }

    pub fn update_window(&mut self, window: &W::Id, serial: Option<Serial>) {
        // Propagate the commit/update through the tile so sizing_mode and pending flags
        // advance after configure acks.
        let Some(col) = self
            .master
            .iter_mut()
            .chain(self.stack.iter_mut())
            .find(|c| c.tile.window().id() == window)
        else {
            return;
        };

        if let Some(serial) = serial {
            col.tile.window_mut().on_commit(serial);
        }
        col.tile.update_window();
    }

    pub fn scroll_amount_to_activate(&self, _window: &W::Id) -> f64 {
        0.0
    }

    pub fn activate_window(&mut self, window: &W::Id) -> bool {
        if let Some(master) = &self.master {
            if master.tile.window().id() == window {
                self.focus = Focus::Master;
                return true;
            }
        }
        for (i, col) in self.stack.iter().enumerate() {
            if col.tile.window().id() == window {
                self.focus = Focus::Stack(i);
                return true;
            }
        }
        false
    }

    pub fn start_close_animation_for_window(
        &mut self,
        renderer: &mut GlesRenderer,
        window: &W::Id,
        blocker: TransactionBlocker,
    ) {
        // Find the closing tile's slot + size, take its unmap snapshot, and
        // spawn a `ClosingWindow` (fade-out + zoom-out) at that slot. The
        // tile itself will be removed from the layout separately; the
        // closing-window element keeps drawing the snapshot in place while
        // the rest of the layout reflows around the vacated slot.
        let Some((tile, tile_pos)) = self
            .tiles_with_render_positions_mut(false)
            .find(|(t, _)| t.window().id() == window)
        else {
            return;
        };
        let Some(snapshot) = tile.take_unmap_snapshot() else {
            return;
        };
        let tile_size = tile.tile_size();

        let anim = Animation::new(
            self.clock.clone(),
            0.,
            1.,
            0.,
            self.options.animations.window_close.anim,
        );

        let blocker = if self.options.disable_transactions {
            TransactionBlocker::completed()
        } else {
            blocker
        };

        let scale = Scale::from(self.scale);
        match ClosingWindow::new(renderer, snapshot, scale, tile_size, tile_pos, blocker, anim) {
            Ok(closing) => self.closing_windows.push(closing),
            Err(err) => warn!("error creating a closing window animation: {err:?}"),
        }
    }

    pub fn start_open_animation(&mut self, id: &W::Id) -> bool {
        if let Some(m) = &mut self.master {
            if m.start_open_animation(id) {
                return true;
            }
        }
        for col in &mut self.stack {
            if col.start_open_animation(id) {
                return true;
            }
        }
        false
    }

    pub fn focus_left(&mut self) -> bool {
        match self.focus {
            Focus::Stack(idx) if self.master.is_some() => {
                // Remember where we were in the stack so a later
                // `focus_right` from master returns to the same row.
                self.last_stack_idx = Some(idx);
                self.focus = Focus::Master;
                true
            }
            _ => false,
        }
    }

    pub fn focus_right(&mut self) -> bool {
        match self.focus {
            Focus::Master if !self.stack.is_empty() => {
                // Restore the remembered stack index when valid; fall
                // back to the top row otherwise.
                let idx = self
                    .last_stack_idx
                    .filter(|i| *i < self.stack.len())
                    .unwrap_or(0);
                self.focus = Focus::Stack(idx);
                true
            }
            _ => false,
        }
    }

    pub fn focus_column_first(&mut self) {
        if self.master.is_some() {
            self.focus = Focus::Master;
        }
    }

    pub fn focus_column_last(&mut self) {
        if !self.stack.is_empty() {
            self.focus = Focus::Stack(self.stack.len() - 1);
        } else if self.master.is_some() {
            self.focus = Focus::Master;
        }
    }

    pub fn focus_column(&mut self, index: usize) {
        if index == 0 {
            if self.master.is_some() {
                self.focus = Focus::Master;
            }
        } else if index - 1 < self.stack.len() {
            self.focus = Focus::Stack(index - 1);
        }
    }

    pub fn focus_window_in_column(&mut self, _index: u8) {
        // Each column holds at most one window in the master-stack model.
    }

    pub fn focus_down(&mut self) -> bool {
        match self.focus {
            Focus::Stack(idx) if idx + 1 < self.stack.len() => {
                self.focus = Focus::Stack(idx + 1);
                true
            }
            Focus::Master if !self.stack.is_empty() => {
                self.focus = Focus::Stack(0);
                true
            }
            _ => false,
        }
    }

    pub fn focus_up(&mut self) -> bool {
        match self.focus {
            Focus::Stack(idx) if idx > 0 => {
                self.focus = Focus::Stack(idx - 1);
                true
            }
            _ => false,
        }
    }

    pub fn focus_down_or_left(&mut self) {
        if !self.focus_down() {
            self.focus_left();
        }
    }

    pub fn focus_down_or_right(&mut self) {
        if !self.focus_down() {
            self.focus_right();
        }
    }

    pub fn focus_up_or_left(&mut self) {
        if !self.focus_up() {
            self.focus_left();
        }
    }

    pub fn focus_up_or_right(&mut self) {
        if !self.focus_up() {
            self.focus_right();
        }
    }

    pub fn focus_top(&mut self) {
        if let Focus::Stack(_) = self.focus { self.focus = Focus::Stack(0) }
    }

    pub fn focus_bottom(&mut self) {
        if !self.stack.is_empty() {
            self.focus = Focus::Stack(self.stack.len() - 1);
        }
    }

    pub fn move_column_to_index(&mut self, _index: usize) {
        // No-op: master-stack has fixed order.
    }

    pub fn move_left(&mut self) -> bool {
        match self.focus {
            Focus::Stack(idx) => {
                // Promote stack[idx] to master; demote current master to
                // the same stack slot the promoted column vacated (NOT
                // the top of the stack). Keeps the swap symmetric:
                // doing move_left → move_right returns to the same
                // arrangement.
                let promoted = self.stack.remove(idx);
                if let Some(old_master) = self.master.take() {
                    self.stack.insert(idx, old_master);
                }
                self.master = Some(promoted);
                self.focus = Focus::Master;
                // Remember the slot so a later focus_right /
                // move_right from master returns here.
                self.last_stack_idx = Some(idx);
                self.update_tile_sizes();
                true
            }
            _ => false,
        }
    }

    /// Spring-animated variant of `move_left`. The swap happens instantly at
    /// the layout level; each tile whose slot changed gets a `MoveAnimation`
    /// driving its render offset from the old slot to the new one, tuned by
    /// `animations.tile_movement`. The renderer/xray params are unused by
    /// the spring path (no snapshot baking) but the signature is preserved
    /// so callers up the stack don't need to change.
    pub fn move_left_animated(
        &mut self,
        _renderer: &mut GlesRenderer,
        _xray: Option<&mut Xray>,
        _xray_has_blocked_out_layers: bool,
        _xray_pos: XrayPos,
    ) -> bool {
        self.swap_with_spring(|s| s.move_left())
    }

    /// Spring-animated variant of `move_right`.
    pub fn move_right_animated(
        &mut self,
        _renderer: &mut GlesRenderer,
        _xray: Option<&mut Xray>,
        _xray_has_blocked_out_layers: bool,
        _xray_pos: XrayPos,
    ) -> bool {
        self.swap_with_spring(|s| s.move_right())
    }

    /// Spring-animated variant of `move_up`.
    pub fn move_up_animated(
        &mut self,
        _renderer: &mut GlesRenderer,
        _xray: Option<&mut Xray>,
        _xray_has_blocked_out_layers: bool,
        _xray_pos: XrayPos,
    ) -> bool {
        self.swap_with_spring(|s| s.move_up())
    }

    /// Spring-animated variant of `move_down`.
    pub fn move_down_animated(
        &mut self,
        _renderer: &mut GlesRenderer,
        _xray: Option<&mut Xray>,
        _xray_has_blocked_out_layers: bool,
        _xray_pos: XrayPos,
    ) -> bool {
        self.swap_with_spring(|s| s.move_down())
    }

    /// Shared implementation: capture pre-swap positions → run `mutate` →
    /// diff layout → for each tile whose slot changed, kick off a spring
    /// animation on its render offset so it visibly slides from the old
    /// slot to the new one.
    fn swap_with_spring(&mut self, mutate: impl FnOnce(&mut Self) -> bool) -> bool {
        // Snapshot positions before the swap.
        let pre_layout = self.column_layout();
        let mut pre: Vec<(W::Id, Point<f64, Logical>)> = Vec::with_capacity(pre_layout.len());
        for (uidx, pos, _) in &pre_layout {
            if let Some(col) = self.column_by_unified_idx(*uidx) {
                pre.push((col.tile.window().id().clone(), *pos));
            }
        }

        if !mutate(self) {
            return false;
        }

        // Diff against post-swap positions; animate the displacement.
        let post_layout = self.column_layout();
        let anim_cfg = self.options.animations.tile_movement.0;
        for (id, old_pos) in pre {
            let new_pos = post_layout.iter().find_map(|(uidx, pos, _)| {
                self.column_by_unified_idx(*uidx)
                    .and_then(|c| (c.tile.window().id() == &id).then_some(*pos))
            });
            let Some(new_pos) = new_pos else {
                continue;
            };
            let delta = old_pos - new_pos;
            if delta.x.abs() < 0.5 && delta.y.abs() < 0.5 {
                continue;
            }
            if let Some(col) = self.find_column_by_id_mut(&id) {
                if delta.x.abs() >= 0.5 {
                    col.tile.animate_move_x_from_with_config(delta.x, anim_cfg);
                }
                if delta.y.abs() >= 0.5 {
                    col.tile.animate_move_y_from_with_config(delta.y, anim_cfg);
                }
            }
        }

        true
    }

    fn find_column_by_id_mut(&mut self, id: &W::Id) -> Option<&mut Column<W>> {
        if let Some(m) = self.master.as_mut() {
            if m.tile.window().id() == id {
                return Some(m);
            }
        }
        self.stack.iter_mut().find(|c| c.tile.window().id() == id)
    }

    pub fn move_right(&mut self) -> bool {
        match self.focus {
            Focus::Master => {
                if self.stack.is_empty() {
                    return false;
                }
                // Swap with the previously-focused stack row when we know
                // it (the user just came from there via focus_left); else
                // fall back to the top of the stack. Clamped because the
                // stack may have shrunk since `last_stack_idx` was set.
                let idx = self
                    .last_stack_idx
                    .filter(|i| *i < self.stack.len())
                    .unwrap_or(0);
                let Some(old_master) = self.master.take() else {
                    return false;
                };
                let new_master = self.stack.remove(idx);
                self.master = Some(new_master);
                self.stack.insert(idx, old_master);
                self.focus = Focus::Stack(idx);
                self.update_tile_sizes();
                true
            }
            _ => false,
        }
    }

    pub fn move_column_to_first(&mut self) {
        if matches!(self.focus, Focus::Stack(_)) {
            self.move_left();
        }
    }

    pub fn move_column_to_last(&mut self) {
        if let Focus::Stack(idx) = self.focus {
            if idx + 1 < self.stack.len() {
                let col = self.stack.remove(idx);
                self.stack.push(col);
                self.focus = Focus::Stack(self.stack.len() - 1);
                // Stack reordering keeps each slot the same size; no need to resize.
            }
        }
    }

    pub fn move_down(&mut self) -> bool {
        match self.focus {
            Focus::Stack(idx) if idx + 1 < self.stack.len() => {
                self.stack.swap(idx, idx + 1);
                self.focus = Focus::Stack(idx + 1);
                true
            }
            _ => false,
        }
    }

    pub fn move_up(&mut self) -> bool {
        match self.focus {
            Focus::Stack(idx) if idx > 0 => {
                self.stack.swap(idx, idx - 1);
                self.focus = Focus::Stack(idx - 1);
                true
            }
            _ => false,
        }
    }

    pub fn consume_or_expel_window_left(&mut self, _window: Option<&W::Id>) {
        todo!("master-stack: consume_or_expel_window_left")
    }

    pub fn consume_or_expel_window_right(&mut self, _window: Option<&W::Id>) {
        todo!("master-stack: consume_or_expel_window_right")
    }

    pub fn consume_into_column(&mut self) {
        todo!("master-stack: consume_into_column")
    }

    pub fn expel_from_column(&mut self) {
        todo!("master-stack: expel_from_column")
    }

    pub fn swap_window_in_direction(&mut self, direction: ScrollDirection) {
        match direction {
            ScrollDirection::Left => {
                self.move_left();
            }
            ScrollDirection::Right => {
                self.move_right();
            }
        }
    }

    pub fn toggle_column_tabbed_display(&mut self) {
        // No tabbed display in master-stack.
    }

    pub fn set_column_display(&mut self, _display: ColumnDisplay) {
        // No-op.
    }

    pub fn center_column(&mut self) {
        // No-op: there is no view offset to center.
    }

    pub fn center_window(&mut self, _window: Option<&W::Id>) {
        // No-op.
    }

    pub fn center_visible_columns(&mut self) {
        // No-op.
    }

    pub fn view_pos(&self) -> f64 {
        0.0
    }

    pub fn target_view_pos(&self) -> f64 {
        0.0
    }

    pub fn columns(&self) -> impl Iterator<Item = &Column<W>> {
        self.master.iter().chain(self.stack.iter())
    }

    fn columns_mut(&mut self) -> impl Iterator<Item = &mut Column<W>> {
        self.master.iter_mut().chain(self.stack.iter_mut())
    }

    /// Returns the laid-out (x,y) origin and size for each column, with gaps applied as
    /// outer margins (around the working area) and inner spacing (between tiles).
    fn column_layout(&self) -> Vec<(usize, Point<f64, Logical>, Size<f64, Logical>)> {
        let mut out = Vec::new();
        let work = self.working_area;
        let g = self.options.layout.gaps;
        let n_stack = self.stack.len();

        let col_y = work.loc.y + g;
        let col_h = f64::max(work.size.h - 2.0 * g, 1.0);

        if n_stack == 0 {
            if self.master.is_some() {
                out.push((
                    0,
                    Point::from((work.loc.x + g, col_y)),
                    Size::from((f64::max(work.size.w - 2.0 * g, 1.0), col_h)),
                ));
            }
            return out;
        }

        // master + stack: 2 outer gaps + 1 between-column gap.
        let avail_w = f64::max(work.size.w - 3.0 * g, 1.0);
        let master_w = f64::max(avail_w * self.master_ratio, 1.0);
        let stack_w = f64::max(avail_w - master_w, 1.0);

        let master_x = work.loc.x + g;
        let stack_x = work.loc.x + g + master_w + g;

        if self.master.is_some() {
            out.push((
                0,
                Point::from((master_x, col_y)),
                Size::from((master_w, col_h)),
            ));
        }

        // Each stack tile plus (n_stack - 1) inner vertical gaps inside col_h.
        let avail_h = f64::max(col_h - (n_stack as f64 - 1.0) * g, 1.0);
        let stack_h = avail_h / n_stack as f64;
        for i in 0..n_stack {
            let y = col_y + (i as f64) * (stack_h + g);
            out.push((
                i + 1,
                Point::from((stack_x, y)),
                Size::from((stack_w, stack_h)),
            ));
        }
        out
    }

    pub fn tiles_with_render_positions(
        &self,
    ) -> impl Iterator<Item = (&Tile<W>, Point<f64, Logical>, bool)> {
        let layout = self.column_layout();
        let mut it = self.columns();
        layout.into_iter().filter_map(move |(_, pos, _)| {
            let col = it.next()?;
            Some((&col.tile, pos, true))
        })
    }

    pub fn tiles_with_render_positions_mut(
        &mut self,
        _round: bool,
    ) -> impl Iterator<Item = (&mut Tile<W>, Point<f64, Logical>)> {
        let layout = self.column_layout();
        let mut it = self.columns_mut();
        layout.into_iter().filter_map(move |(_, pos, _)| {
            let col = it.next()?;
            Some((&mut col.tile, pos))
        })
    }

    pub fn tiles_with_ipc_layouts(&self) -> impl Iterator<Item = (&Tile<W>, WindowLayout)> {
        self.columns().enumerate().map(|(col_idx, col)| {
            let layout = WindowLayout {
                pos_in_scrolling_layout: Some((col_idx + 1, 1)),
                ..col.tile.ipc_layout_template()
            };
            (&col.tile, layout)
        })
    }

    pub(super) fn insert_hint_area(
        &self,
        _position: InsertPosition,
    ) -> Option<Rectangle<f64, Logical>> {
        None
    }

    pub fn active_window_visual_rectangle(&self) -> Option<Rectangle<f64, Logical>> {
        let layout = self.column_layout();
        let active_idx = self.active_column_idx_internal()?;
        let (_, pos, size) = layout.into_iter().find(|(i, _, _)| *i == active_idx)?;
        Some(Rectangle::new(pos, size))
    }

    fn active_column_idx_internal(&self) -> Option<usize> {
        match self.focus {
            Focus::Empty => None,
            Focus::Master => Some(0),
            Focus::Stack(idx) => Some(idx + 1),
        }
    }

    pub fn popup_target_rect(&self, id: &W::Id) -> Option<Rectangle<f64, Logical>> {
        let layout = self.column_layout();
        for (i, col) in self.columns().enumerate() {
            if col.tile.window().id() == id {
                let (_, pos, size) = layout.iter().find(|(j, _, _)| *j == i)?;
                return Some(Rectangle::new(*pos, *size));
            }
        }
        None
    }

    pub fn toggle_width(&mut self, _forwards: bool) {
        // No-op.
    }

    pub fn toggle_full_width(&mut self) {
        // No-op.
    }

    pub fn set_window_width(&mut self, _window: Option<&W::Id>, _change: SizeChange) {
        // No-op.
    }

    /// Adjust the master-pane width as a fraction of the working area,
    /// clamped to a comfortable range. Springs every visible tile from
    /// its pre-change slot to the new layout — same `tile_movement`
    /// config the swap + close paths use, so the resize-mode nudge
    /// feels consistent with the rest of the layout's motion.
    pub fn nudge_master_ratio(&mut self, delta: f64) {
        let new_ratio = (self.master_ratio + delta).clamp(0.15, 0.85);
        if (new_ratio - self.master_ratio).abs() < 1e-6 {
            return;
        }

        // Snapshot every column's pre-nudge slot.
        let pre: Vec<(W::Id, Point<f64, Logical>)> = self
            .column_layout()
            .into_iter()
            .filter_map(|(uidx, pos, _)| {
                self.column_by_unified_idx(uidx)
                    .map(|col| (col.tile.window().id().clone(), pos))
            })
            .collect();

        self.master_ratio = new_ratio;
        self.update_tile_sizes();

        let anim_cfg = self.options.animations.tile_movement.0;
        let post_layout = self.column_layout();
        for (id, old_pos) in pre {
            let new_pos = post_layout.iter().find_map(|(uidx, pos, _)| {
                self.column_by_unified_idx(*uidx)
                    .and_then(|c| (c.tile.window().id() == &id).then_some(*pos))
            });
            let Some(new_pos) = new_pos else {
                continue;
            };
            let delta = old_pos - new_pos;
            if delta.x.abs() < 0.5 && delta.y.abs() < 0.5 {
                continue;
            }
            if let Some(col) = self.find_column_by_id_mut(&id) {
                if delta.x.abs() >= 0.5 {
                    col.tile.animate_move_x_from_with_config(delta.x, anim_cfg);
                }
                if delta.y.abs() >= 0.5 {
                    col.tile.animate_move_y_from_with_config(delta.y, anim_cfg);
                }
            }
        }
    }

    pub fn set_window_height(&mut self, _window: Option<&W::Id>, _change: SizeChange) {
        // No-op.
    }

    pub fn reset_window_height(&mut self, _window: Option<&W::Id>) {
        // No-op.
    }

    pub fn toggle_window_width(&mut self, _window: Option<&W::Id>, _forwards: bool) {
        // No-op.
    }

    pub fn toggle_window_height(&mut self, _window: Option<&W::Id>, _forwards: bool) {
        // No-op.
    }

    pub fn expand_column_to_available_width(&mut self) {
        // No-op.
    }

    pub fn set_fullscreen(&mut self, window: &W::Id, is_fullscreen: bool) -> bool {
        let Some(unified_idx) = self
            .columns()
            .position(|col| col.tile.window().id() == window)
        else {
            return false;
        };

        let working_area = self.working_area;
        let col = self
            .column_mut_by_unified_idx(unified_idx)
            .expect("column index must be valid");
        let changed = col.set_fullscreen(is_fullscreen);

        // If we just left fullscreen/maximized, the master-stack slot size needs to be requested.
        if !is_fullscreen {
            self.update_tile_sizes();
        }
        let _ = working_area;
        changed
    }

    pub fn set_maximized(&mut self, window: &W::Id, maximize: bool) -> bool {
        let Some(unified_idx) = self
            .columns()
            .position(|col| col.tile.window().id() == window)
        else {
            return false;
        };

        let working_area = self.working_area;
        let col = self
            .column_mut_by_unified_idx(unified_idx)
            .expect("column index must be valid");
        let changed = col.set_maximized(maximize, working_area.size);

        if !maximize {
            self.update_tile_sizes();
        }
        changed
    }

    fn column_mut_by_unified_idx(&mut self, unified_idx: usize) -> Option<&mut Column<W>> {
        if unified_idx == 0 {
            self.master.as_mut()
        } else {
            self.stack.get_mut(unified_idx - 1)
        }
    }

    pub fn render_above_top_layer(&self) -> bool {
        // When a column is in fullscreen takeover (the CTRL+Tab path —
        // covering the whole output at view_size), render it *above*
        // the wlr-layer Top layer so the waybar / other Top-layer
        // surfaces don't punch through the fullscreen tile.
        // `is_pending_maximized` keeps the working_area takeover that
        // already sits below the waybar, so it's not promoted.
        if let Some(m) = &self.master {
            if m.is_pending_fullscreen() {
                return true;
            }
        }
        self.stack.iter().any(|col| col.is_pending_fullscreen())
    }

    pub fn render<R: NiriRenderer>(
        &self,
        mut ctx: RenderCtx<R>,
        xray_pos: XrayPos,
        focus_ring: bool,
        push: &mut dyn FnMut(ScrollingSpaceRenderElement<R>),
    ) {
        let focused_idx = self.active_column_idx_internal();

        if let Some((take_idx, area)) = self.takeover() {
            if let Some(col) = self.column_by_unified_idx(take_idx) {
                let is_active = Some(take_idx) == focused_idx;
                let draw_focus_ring = focus_ring && is_active;
                // Offset xray_pos so the blur samples wallpaper *behind* this tile
                // rather than at the workspace's origin. See the matching comment
                // in `floating.rs`.
                let xray_pos = xray_pos.offset(area.loc);
                col.tile.render(
                    ctx.r(),
                    area.loc,
                    xray_pos,
                    draw_focus_ring,
                    is_active,
                    &mut |elem| push(ScrollingSpaceRenderElement::Tile(elem)),
                );
            }
            return;
        }

        // Fade-out + zoom-out snapshots of tiles that just unmapped.
        // niri's render pipeline draws back-to-front: elements pushed
        // first end up *on top*. Push closing-window snapshots before
        // the live tiles so they sit visibly on top during the brief
        // animation window (matches floating.rs's pattern).
        let view_rect = Rectangle::from_size(self.view_size);
        let scale = Scale::from(self.scale);
        for closing in self.closing_windows.iter().rev() {
            let elem = closing.render(ctx.as_gles(), view_rect, scale);
            push(ScrollingSpaceRenderElement::ClosingWindow(elem));
        }

        let layout = self.column_layout();
        for (unified_idx, pos, _size) in layout {
            let Some(col) = self.column_by_unified_idx(unified_idx) else {
                continue;
            };
            // Layout position plus any in-flight move-animation offset — this
            // is what drives the spring-based slide when columns swap slots.
            let pos = pos + col.tile.render_offset();
            let is_active = Some(unified_idx) == focused_idx;
            let draw_focus_ring = focus_ring && is_active;
            // Offset xray_pos by the tile's position within the workspace.
            let xray_pos = xray_pos.offset(pos);
            col.tile.render(
                ctx.r(),
                pos,
                xray_pos,
                draw_focus_ring,
                is_active,
                &mut |elem| push(ScrollingSpaceRenderElement::Tile(elem)),
            );
        }
    }

    pub fn window_under(&self, pos: Point<f64, Logical>) -> Option<(&W, HitType)> {
        // Takeover: only the maximized/fullscreen tile is hit-testable.
        if let Some((take_idx, area)) = self.takeover() {
            let col = self.column_by_unified_idx(take_idx)?;
            return HitType::hit_tile(&col.tile, area.loc, pos);
        }

        let layout = self.column_layout();
        for (unified_idx, tile_pos, _size) in layout {
            let Some(col) = self.column_by_unified_idx(unified_idx) else {
                continue;
            };
            if let Some(hit) = HitType::hit_tile(&col.tile, tile_pos, pos) {
                return Some(hit);
            }
        }
        None
    }

    fn column_by_unified_idx(&self, unified_idx: usize) -> Option<&Column<W>> {
        if unified_idx == 0 {
            self.master.as_ref()
        } else {
            self.stack.get(unified_idx - 1)
        }
    }

    pub fn view_offset_gesture_begin(&mut self, _is_touchpad: bool) {
        // No-op.
    }

    pub fn dnd_scroll_gesture_begin(&mut self) {
        // No-op.
    }

    pub fn view_offset_gesture_update(
        &mut self,
        _delta_x: f64,
        _timestamp: Duration,
        _is_touchpad: bool,
    ) -> Option<bool> {
        None
    }

    pub fn dnd_scroll_gesture_scroll(&mut self, _delta: f64) -> bool {
        false
    }

    pub fn view_offset_gesture_end(&mut self, _is_touchpad: Option<bool>) -> bool {
        false
    }

    pub fn dnd_scroll_gesture_end(&mut self) {
        // No-op.
    }

    pub fn interactive_resize_begin(&mut self, _window: W::Id, _edges: ResizeEdge) -> bool {
        // Master-stack v1: no interactive edge-drag resize. Returning false signals niri
        // didn't start a resize, so update/end won't fire.
        false
    }

    pub fn interactive_resize_update(
        &mut self,
        _window: &W::Id,
        _delta: Point<f64, Logical>,
    ) -> bool {
        false
    }

    pub fn interactive_resize_end(&mut self, _window: Option<&W::Id>) {
        // No-op: no resize to end.
    }

    pub fn refresh(&mut self, is_active: bool, is_focused: bool) {
        // Without send_pending_configure() on each window, request_tile_size's pending state
        // never becomes an actual Wayland configure event — the client stays stuck at its first
        // configure and never resizes when the layout changes.
        let active_unified_idx = self.active_column_idx_internal();
        let working_area_size = self.working_area.size;

        for (col_idx, col) in self
            .master
            .iter_mut()
            .chain(self.stack.iter_mut())
            .enumerate()
        {
            let win = col.tile.window_mut();
            win.set_floating(false);
            win.set_active_in_column(true); // single-tile columns
            let activated = is_active && is_focused && Some(col_idx) == active_unified_idx;
            win.set_activated(activated);
            win.set_interactive_resize(None);
            win.set_bounds(working_area_size.to_i32_round());

            win.send_pending_configure();
            win.refresh();
        }
    }

    #[cfg(test)]
    pub fn view_size(&self) -> Size<f64, Logical> {
        self.view_size
    }

    #[cfg(test)]
    pub fn parent_area(&self) -> Rectangle<f64, Logical> {
        self.parent_area
    }

    #[cfg(test)]
    pub fn clock(&self) -> &Clock {
        &self.clock
    }

    #[cfg(test)]
    pub fn options(&self) -> &Rc<Options> {
        &self.options
    }

    #[cfg(test)]
    pub fn active_column_idx(&self) -> usize {
        self.active_column_idx_internal().unwrap_or(0)
    }

    #[cfg(test)]
    pub(super) fn view_offset(&self) -> &ViewOffset {
        &self.view_offset
    }

    #[cfg(test)]
    pub fn verify_invariants(&self) {
        assert!(self.view_size.w > 0.);
        assert!(self.view_size.h > 0.);
        assert!(self.scale > 0.);
        assert!(self.scale.is_finite());
        assert_eq!(
            self.working_area,
            compute_working_area(self.parent_area, self.scale, self.options.layout.struts)
        );

        match self.focus {
            Focus::Empty => assert!(self.master.is_none() && self.stack.is_empty()),
            Focus::Master => assert!(self.master.is_some()),
            Focus::Stack(idx) => assert!(idx < self.stack.len()),
        }
    }
}

impl ViewOffset {
    pub fn current(&self) -> f64 {
        match self {
            ViewOffset::Static(v) => *v,
        }
    }

    pub fn target(&self) -> f64 {
        match self {
            ViewOffset::Static(v) => *v,
        }
    }

    pub fn is_static(&self) -> bool {
        true
    }

    pub fn is_gesture(&self) -> bool {
        false
    }

    pub fn is_dnd_scroll(&self) -> bool {
        false
    }

    pub fn is_animation_ongoing(&self) -> bool {
        false
    }

    pub fn offset(&mut self, _delta: f64) {}

    pub fn cancel_gesture(&mut self) {}

    pub fn stop_anim_and_gesture(&mut self) {}
}

impl From<PresetSize> for ColumnWidth {
    fn from(value: PresetSize) -> Self {
        match value {
            PresetSize::Proportion(p) => Self::Proportion(p.clamp(0., 10000.)),
            PresetSize::Fixed(f) => Self::Fixed(f64::from(f.clamp(1, 100000))),
        }
    }
}

impl<W: LayoutElement> Column<W> {
    #[allow(clippy::too_many_arguments)]
    fn new_with_tile(
        tile: Tile<W>,
        view_size: Size<f64, Logical>,
        working_area: Rectangle<f64, Logical>,
        parent_area: Rectangle<f64, Logical>,
        scale: f64,
        width: ColumnWidth,
        is_full_width: bool,
    ) -> Self {
        let options = tile.options.clone();
        let clock = tile.clock.clone();
        let pending_sizing_mode = tile.window().pending_sizing_mode();

        let mut rv = Self {
            tile,
            width,
            is_full_width,
            is_pending_fullscreen: false,
            is_pending_maximized: false,
            view_size,
            working_area,
            parent_area,
            scale,
            clock,
            options,
        };

        match pending_sizing_mode {
            SizingMode::Normal => (),
            SizingMode::Maximized => {
                rv.set_maximized(true, working_area.size);
            }
            SizingMode::Fullscreen => {
                rv.set_fullscreen(true);
            }
        }

        rv
    }

    fn update_config(
        &mut self,
        view_size: Size<f64, Logical>,
        working_area: Rectangle<f64, Logical>,
        parent_area: Rectangle<f64, Logical>,
        scale: f64,
        options: Rc<Options>,
    ) {
        self.tile.update_config(view_size, scale, options.clone());
        self.view_size = view_size;
        self.working_area = working_area;
        self.parent_area = parent_area;
        self.scale = scale;
        self.options = options;
    }

    pub fn update_shaders(&mut self) {
        self.tile.update_shaders();
    }

    pub fn advance_animations(&mut self) {
        self.tile.advance_animations();
    }

    pub fn are_animations_ongoing(&self) -> bool {
        self.tile.are_animations_ongoing()
    }

    pub fn are_transitions_ongoing(&self) -> bool {
        self.tile.are_transitions_ongoing()
    }

    pub fn update_render_elements(&mut self, is_active: bool, view_rect: Rectangle<f64, Logical>) {
        self.tile.update_render_elements(is_active, view_rect);
    }

    pub fn is_pending_fullscreen(&self) -> bool {
        self.is_pending_fullscreen
    }

    pub fn is_pending_maximized(&self) -> bool {
        self.is_pending_maximized
    }

    pub fn pending_sizing_mode(&self) -> SizingMode {
        if self.is_pending_fullscreen {
            SizingMode::Fullscreen
        } else if self.is_pending_maximized {
            SizingMode::Maximized
        } else {
            SizingMode::Normal
        }
    }

    pub fn render_offset(&self) -> Point<f64, Logical> {
        Point::from((0., 0.))
    }

    pub fn animate_move_from(&mut self, _from_x_offset: f64) {
        // No-op stub.
    }

    pub fn animate_move_from_with_config(
        &mut self,
        _from_x_offset: f64,
        _config: sol_config::Animation,
    ) {
        // No-op stub.
    }

    pub fn contains(&self, window: &W::Id) -> bool {
        self.tile.window().id() == window
    }

    pub fn position(&self, window: &W::Id) -> Option<usize> {
        if self.contains(window) {
            Some(0)
        } else {
            None
        }
    }

    pub fn tiles(&self) -> impl Iterator<Item = (&Tile<W>, Point<f64, Logical>)> + '_ {
        std::iter::once((&self.tile, Point::from((0., 0.))))
    }

    pub fn start_open_animation(&mut self, id: &W::Id) -> bool {
        if self.tile.window().id() == id {
            self.tile.start_open_animation();
            true
        } else {
            false
        }
    }

    /// Sets the pending fullscreen state and asks the tile to size itself accordingly. Returns
    /// whether the state actually changed.
    fn set_fullscreen(&mut self, is_fullscreen: bool) -> bool {
        if self.is_pending_fullscreen == is_fullscreen {
            return false;
        }
        self.is_pending_fullscreen = is_fullscreen;
        if is_fullscreen {
            self.is_pending_maximized = false;
            self.tile.request_fullscreen(false, None);
        }
        // If unsetting, the parent ScrollingSpace will call update_tile_sizes() to resize.
        true
    }

    fn set_maximized(&mut self, maximize: bool, working_area_size: Size<f64, Logical>) -> bool {
        if self.is_pending_maximized == maximize {
            return false;
        }
        self.is_pending_maximized = maximize;
        if maximize {
            self.is_pending_fullscreen = false;
            self.tile.request_maximized(working_area_size, false, None);
        }
        true
    }
}

fn compute_working_area(
    parent_area: Rectangle<f64, Logical>,
    scale: f64,
    struts: Struts,
) -> Rectangle<f64, Logical> {
    let mut working_area = parent_area;

    working_area.size.w = f64::max(0., working_area.size.w - struts.left.0 - struts.right.0);
    working_area.loc.x += struts.left.0;

    working_area.size.h = f64::max(0., working_area.size.h - struts.top.0 - struts.bottom.0);
    working_area.loc.y += struts.top.0;

    let loc = working_area
        .loc
        .to_physical_precise_ceil(scale)
        .to_logical(scale);

    let mut size_diff = (loc - working_area.loc).to_size();
    size_diff.w = f64::min(working_area.size.w, size_diff.w);
    size_diff.h = f64::min(working_area.size.h, size_diff.h);

    working_area.size -= size_diff;
    working_area.loc = loc;

    working_area
}

#[cfg(test)]
mod tests {
    use sol_config::FloatOrInt;
    use smithay::utils::{Rectangle, Size};

    use super::*;
    use crate::utils::round_logical_in_physical;

    #[test]
    fn working_area_starts_at_physical_pixel() {
        let struts = Struts {
            left: FloatOrInt(0.5),
            right: FloatOrInt(1.),
            top: FloatOrInt(0.75),
            bottom: FloatOrInt(1.),
        };

        let parent_area = Rectangle::from_size(Size::from((1280., 720.)));
        let area = compute_working_area(parent_area, 1., struts);

        assert_eq!(round_logical_in_physical(1., area.loc.x), area.loc.x);
        assert_eq!(round_logical_in_physical(1., area.loc.y), area.loc.y);
    }

    #[test]
    fn large_fractional_strut() {
        let struts = Struts {
            left: FloatOrInt(0.),
            right: FloatOrInt(0.),
            top: FloatOrInt(50000.5),
            bottom: FloatOrInt(0.),
        };

        let parent_area = Rectangle::from_size(Size::from((1280., 720.)));
        compute_working_area(parent_area, 1., struts);
    }
}
