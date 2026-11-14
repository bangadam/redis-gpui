//! Wrappers around the GPUI APIs most likely to move across revisions
//! (PLAN.md section 2: split-panes, virtualized lists, modals).
//!
//! Phase 0 holds the resizable split; a GPUI bump then touches this one
//! file instead of every view.
//!
//! Zed's own resizable-pane primitives live in `crates/ui`-layer components
//! that depend on Zed app infrastructure and are not usable standalone, so
//! the divider below follows the same *pattern* Zed's own data-table resize
//! uses at this revision: `on_drag(payload, empty overlay)` to start the
//! drag plus `on_drag_move::<Payload>` on the container to receive moves.

use gpui::{
    div, prelude::*, App, Context, CursorStyle, Div, DragMoveEvent, Empty, Entity, MouseButton,
    MouseDownEvent, Stateful, Styled, WeakEntity, Window,
};

/// Drag payload for panel resize handles.
///
/// Carries which edge is being dragged and the owning entity id, so
/// `on_drag_move` handlers for other splits ignore the event (same pattern
/// as Zed's `DraggedColumn` at this revision).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PanelDrag {
    /// Which divider this drag started on.
    pub edge: PanelEdge,
    /// Entity id of the view that owns the split being resized.
    pub owner: gpui::EntityId,
}

/// Which divider is being dragged.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PanelEdge {
    /// Divider between the Connections panel (left) and Keys panel (middle).
    ConnectionsKeys,
    /// Divider between the Keys panel (middle) and Value panel (right).
    KeysValue,
}

/// Width of the divider hit area (wider than the 1px line so it is easy
/// to grab). Matches Zed's column-resize handle width.
pub const DIVIDER_HIT_WIDTH: f32 = 8.;
/// Visual thickness of the divider line itself.
pub const DIVIDER_LINE_WIDTH: f32 = 1.;

/// Renders one draggable vertical divider between panels.
///
/// The 1px visual line sits centered in the 8px hit area; the caller gives
/// the line its color by adding a `.bg`-styled 1px child, or simply styles
/// the whole hit area.
///
/// `on_down` runs on divider mouse-down; pass a no-op if no per-divider
/// press handling is needed.
pub fn render_divider(
    edge: PanelEdge,
    owner: gpui::EntityId,
    on_down: impl Fn(&MouseDownEvent, &mut Window, &mut App) + 'static,
) -> Stateful<Div> {
    div()
        .id("panel-divider")
        .w(gpui::px(DIVIDER_HIT_WIDTH))
        .h_full()
        .flex_shrink(0.)
        .cursor(CursorStyle::ResizeLeftRight)
        .on_mouse_down(MouseButton::Left, on_down)
        .on_drag(PanelDrag { edge, owner }, move |_, _, _, cx: &mut App| {
            cx.new(|_| Empty)
        })
}

/// Container-level hook that routes panel drag moves to the owning view.
///
/// Attach to the split container. Ignores drags owned by other entities.
/// The callback receives the window, so it can read window bounds for
/// clamping.
///
/// Unused since Phase 7 (the stateful workspace root inlines its own
/// `on_drag_move` — the generic-`Div` signature here can't attach to a
/// `Stateful<Div>`), but kept as the documented pattern for future
/// plain-`Div` split containers.
#[allow(dead_code)]
pub fn on_panel_drag_move<T: 'static>(
    container: Div,
    owner: &Entity<T>,
    on_move: impl Fn(&mut T, &DragMoveEvent<PanelDrag>, &mut Window, &mut Context<T>) + 'static,
) -> Div {
    let view: WeakEntity<T> = owner.downgrade();
    container.on_drag_move::<PanelDrag>(move |event, window, cx| {
        if event.drag(cx).owner != view.entity_id() {
            return;
        }
        view.update(cx, |state, cx| on_move(state, event, window, cx))
            .ok();
    })
}

/// Clamps a desired panel width into `[min, max]` (inclusive), in pixels.
pub fn clamp_width(desired: f32, min: f32, max: f32) -> f32 {
    desired.max(min).min(max)
}
