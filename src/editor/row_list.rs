//! `RowList`: the editor's scrollable column of visual rows — a
//! variable-height sibling of gpui's `uniform_list`. Every row's height is
//! known exactly at build time (`LineElement::height`), so rows are placed by
//! a prefix-sum offset table instead of `item_height * ix` and only the
//! visible slice is laid out per frame. Scroll state lives in a
//! `UniformListScrollHandle` — same type, same `scroll_to_item` /
//! `scroll_to_item_strict` semantics — so the editor's scroll call sites
//! don't care which list they drive.

use std::rc::Rc;

use gpui::{
    point, size, AnyElement, App, AvailableSpace, Bounds, ContentMask, Element, ElementId,
    GlobalElementId, Hitbox, InspectorElementId, InteractiveElement, Interactivity, IntoElement,
    ItemSize, LayoutId, Overflow, Pixels, ScrollStrategy, StatefulInteractiveElement,
    StyleRefinement, Styled, UniformListScrollHandle, Window,
};

use super::LineElement;

/// Rows where `rows[i]` occupies the y-range `offsets[i]..offsets[i + 1]`
/// (`offsets.len() == rows.len() + 1`, so the last entry is the content
/// height). Content gets `viewport − line_h` of overscroll past the last row,
/// vim-style: the last line can scroll up to the top of the pane.
pub(super) fn row_list(
    id: impl Into<ElementId>,
    rows: Rc<Vec<LineElement>>,
    offsets: Rc<Vec<Pixels>>,
    line_h: Pixels,
) -> RowList {
    let mut interactivity = Interactivity::new();
    interactivity.element_id = Some(id.into());
    interactivity.base_style.overflow.y = Some(Overflow::Scroll);
    RowList { rows, offsets, line_h, interactivity, scroll_handle: None }
}

pub(super) struct RowList {
    rows: Rc<Vec<LineElement>>,
    offsets: Rc<Vec<Pixels>>,
    line_h: Pixels,
    interactivity: Interactivity,
    scroll_handle: Option<UniformListScrollHandle>,
}

impl RowList {
    /// Track scroll state in `handle`. Wires the handle's base `ScrollHandle`
    /// into the interactivity, which owns wheel scrolling, offset clamping,
    /// and the `max_offset`/`bounds` bookkeeping the editor's scroll math
    /// reads back.
    pub(super) fn track_scroll(mut self, handle: UniformListScrollHandle) -> Self {
        let base = handle.0.borrow().base_handle.clone();
        self.scroll_handle = Some(handle);
        StatefulInteractiveElement::track_scroll(self, &base)
    }
}

pub(super) struct RowListFrameState {
    items: Vec<AnyElement>,
}

impl Styled for RowList {
    fn style(&mut self) -> &mut StyleRefinement {
        &mut self.interactivity.base_style
    }
}

impl InteractiveElement for RowList {
    fn interactivity(&mut self) -> &mut Interactivity {
        &mut self.interactivity
    }
}

// The element always has an id ("lines"), which is what the stateful
// subset requires.
impl StatefulInteractiveElement for RowList {}

impl Element for RowList {
    type RequestLayoutState = RowListFrameState;
    type PrepaintState = Option<Hitbox>;

    fn id(&self) -> Option<ElementId> {
        self.interactivity.element_id.clone()
    }

    fn source_location(&self) -> Option<&'static std::panic::Location<'static>> {
        None
    }

    fn request_layout(
        &mut self,
        global_id: Option<&GlobalElementId>,
        inspector_id: Option<&InspectorElementId>,
        window: &mut Window,
        cx: &mut App,
    ) -> (LayoutId, Self::RequestLayoutState) {
        // Style-driven sizing only (the editor gives the list flex_1 in a
        // column); content height is reported at prepaint.
        let layout_id = self.interactivity.request_layout(
            global_id,
            inspector_id,
            window,
            cx,
            |style, window, cx| {
                window.with_text_style(style.text_style().cloned(), |window| {
                    window.request_layout(style, None, cx)
                })
            },
        );
        (layout_id, RowListFrameState { items: Vec::new() })
    }

    fn prepaint(
        &mut self,
        global_id: Option<&GlobalElementId>,
        inspector_id: Option<&InspectorElementId>,
        bounds: Bounds<Pixels>,
        frame_state: &mut Self::RequestLayoutState,
        window: &mut Window,
        cx: &mut App,
    ) -> Option<Hitbox> {
        let total = self.offsets.last().copied().unwrap_or(Pixels::ZERO);
        let overscroll = (bounds.size.height - self.line_h).max(Pixels::ZERO);
        let content_size = size(bounds.size.width, total + overscroll);

        let deferred = self.scroll_handle.as_ref().and_then(|handle| {
            let mut state = handle.0.borrow_mut();
            state.last_item_size = Some(ItemSize { item: bounds.size, contents: content_size });
            state.deferred_scroll_to_item.take()
        });

        self.interactivity.prepaint(
            global_id,
            inspector_id,
            bounds,
            content_size,
            window,
            cx,
            |_style, mut scroll_offset, hitbox, window, cx| {
                let count = self.rows.len();
                if count > 0 {
                    // Resolve a queued scroll_to_item against the offset
                    // table. Non-strict only acts when the row is out of
                    // view; strict always repositions (zz/zt/zb).
                    if let Some(scroll) = deferred {
                        let ix = scroll.item_index.min(count - 1);
                        let viewport = bounds.size.height;
                        let (top, bottom) = (self.offsets[ix], self.offsets[ix + 1]);
                        let scroll_top = -scroll_offset.y;
                        let visible = top >= scroll_top && bottom <= scroll_top + viewport;
                        if scroll.scroll_strict || !visible {
                            let target = match scroll.strategy {
                                ScrollStrategy::Top => top,
                                ScrollStrategy::Center => (top + bottom - viewport) / 2.,
                                ScrollStrategy::Bottom => bottom - viewport,
                            };
                            let max = (content_size.height - viewport).max(Pixels::ZERO);
                            scroll_offset.y = -target.max(Pixels::ZERO).min(max);
                            if let Some(handle) = &self.scroll_handle {
                                handle.0.borrow().base_handle.set_offset(scroll_offset);
                            }
                        }
                    }

                    // Rows overlapping [scroll_top, scroll_top + viewport).
                    let scroll_top = -scroll_offset.y;
                    let first =
                        self.offsets.partition_point(|&o| o <= scroll_top).saturating_sub(1);
                    let last = self
                        .offsets
                        .partition_point(|&o| o < scroll_top + bounds.size.height)
                        .min(count);
                    window.with_content_mask(Some(ContentMask { bounds }), |window| {
                        for ix in first..last {
                            let mut item = self.rows[ix].clone().into_any_element();
                            let height = self.offsets[ix + 1] - self.offsets[ix];
                            item.layout_as_root(
                                size(
                                    AvailableSpace::Definite(bounds.size.width),
                                    AvailableSpace::Definite(height),
                                ),
                                window,
                                cx,
                            );
                            item.prepaint_at(
                                bounds.origin
                                    + point(Pixels::ZERO, self.offsets[ix] + scroll_offset.y),
                                window,
                                cx,
                            );
                            frame_state.items.push(item);
                        }
                    });
                }
                hitbox
            },
        )
    }

    fn paint(
        &mut self,
        global_id: Option<&GlobalElementId>,
        inspector_id: Option<&InspectorElementId>,
        bounds: Bounds<Pixels>,
        frame_state: &mut Self::RequestLayoutState,
        hitbox: &mut Option<Hitbox>,
        window: &mut Window,
        cx: &mut App,
    ) {
        self.interactivity.paint(
            global_id,
            inspector_id,
            bounds,
            hitbox.as_ref(),
            window,
            cx,
            |_, window, cx| {
                for item in &mut frame_state.items {
                    item.paint(window, cx);
                }
            },
        )
    }
}

impl IntoElement for RowList {
    type Element = Self;
    fn into_element(self) -> Self {
        self
    }
}
