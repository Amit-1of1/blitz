//! PATCH(rust-ui): `position: fixed` against the viewport.
//!
//! Upstream maps `fixed` to Taffy's `absolute`, and Taffy positions absolute boxes against
//! their *parent*, so a fixed overlay inside a small element was sized and placed relative to
//! that element. After the main layout pass this re-runs the absolute-positioning step for
//! each fixed box with the viewport as the containing block (adapted from Taffy 0.9's
//! `perform_absolute_layout_on_absolute_children`, MIT), then stores the result relative to
//! the box's layout parent so painting, hit testing and `absolute_position` need no changes.
//! A box with no insets keeps its static position from the main pass.

use crate::BaseDocument;
use style::properties::generated::longhands::position::computed_value::T as Position;
use taffy::{
    AvailableSpace, BoxSizing, CoreStyle, Layout, LayoutInput, LayoutPartialTree, Line, MaybeMath,
    MaybeResolve, NodeId, Point, Rect, RequestedAxis, ResolveOrZero, RunMode, Size, SizingMode,
};

impl BaseDocument {
    fn is_position_fixed(&self, node_id: usize) -> bool {
        self.nodes[node_id]
            .primary_styles()
            .is_some_and(|s| s.clone_position() == Position::Fixed)
    }

    /// Document position of a node's border box from unrounded layout (rounding happens later).
    fn unrounded_position(&self, node_id: usize) -> Point<f32> {
        let mut pos = self.nodes[node_id].unrounded_layout.location;
        let mut parent = self.nodes[node_id].layout_parent.get();
        while let Some(p) = parent {
            let node = &self.nodes[p];
            pos.x += node.unrounded_layout.location.x - node.scroll_offset.x as f32;
            pos.y += node.unrounded_layout.location.y - node.scroll_offset.y as f32;
            parent = node.layout_parent.get();
        }
        pos
    }

    pub(crate) fn layout_fixed_position_boxes(&mut self) {
        let mut fixed = Vec::new();
        let mut stack = vec![self.root_element().id];
        while let Some(id) = stack.pop() {
            if let Some(children) = &*self.nodes[id].layout_children.borrow() {
                stack.extend(children.iter().rev());
            }
            if self.nodes[id].layout_parent.get().is_some() && self.is_position_fixed(id) {
                fixed.push(id);
            }
        }
        // Tree order, so a fixed box inside another is placed after its ancestor moved.
        fixed.sort_by_key(|&id| self.depth(id));
        for id in fixed {
            self.place_fixed_box(id);
        }
    }

    fn depth(&self, node_id: usize) -> usize {
        let mut depth = 0;
        let mut parent = self.nodes[node_id].layout_parent.get();
        while let Some(p) = parent {
            depth += 1;
            parent = self.nodes[p].layout_parent.get();
        }
        depth
    }

    fn place_fixed_box(&mut self, node_id: usize) {
        let viewport = self.stylist.device().au_viewport_size();
        let area_size = Size {
            width: viewport.width.to_f32_px(),
            height: viewport.height.to_f32_px(),
        };
        let area_width = area_size.width;
        let area_height = area_size.height;
        let calc = |val, basis| crate::layout::resolve_calc_value(val, basis);

        let style = self.nodes[node_id].style.clone();
        let aspect_ratio = style.aspect_ratio();
        let margin = style
            .margin()
            .map(|m| m.resolve_to_option(area_width, calc));
        let padding = style.padding().resolve_or_zero(Some(area_width), calc);
        let border = style.border().resolve_or_zero(Some(area_width), calc);
        let pb = padding + border;
        let padding_border_sum = Size {
            width: pb.left + pb.right,
            height: pb.top + pb.bottom,
        };
        let box_sizing_adjustment = if style.box_sizing() == BoxSizing::ContentBox {
            padding_border_sum
        } else {
            Size::ZERO
        };

        let left = style.inset.left.maybe_resolve(area_width, calc);
        let right = style.inset.right.maybe_resolve(area_width, calc);
        let top = style.inset.top.maybe_resolve(area_height, calc);
        let bottom = style.inset.bottom.maybe_resolve(area_height, calc);

        let style_size = style
            .size
            .maybe_resolve(area_size, calc)
            .maybe_apply_aspect_ratio(aspect_ratio)
            .maybe_add(box_sizing_adjustment);
        let min_size = style
            .min_size
            .maybe_resolve(area_size, calc)
            .maybe_apply_aspect_ratio(aspect_ratio)
            .maybe_add(box_sizing_adjustment)
            .or(padding_border_sum.map(Some))
            .maybe_max(padding_border_sum);
        let max_size = style
            .max_size
            .maybe_resolve(area_size, calc)
            .maybe_apply_aspect_ratio(aspect_ratio)
            .maybe_add(box_sizing_adjustment);
        let mut known = style_size.maybe_clamp(min_size, max_size);

        if let (None, Some(l), Some(r)) = (known.width, left, right) {
            let w = area_width.maybe_sub(margin.left).maybe_sub(margin.right) - l - r;
            known.width = Some(w.max(0.0));
            known = known
                .maybe_apply_aspect_ratio(aspect_ratio)
                .maybe_clamp(min_size, max_size);
        }
        if let (None, Some(t), Some(b)) = (known.height, top, bottom) {
            let h = area_height.maybe_sub(margin.top).maybe_sub(margin.bottom) - t - b;
            known.height = Some(h.max(0.0));
            known = known
                .maybe_apply_aspect_ratio(aspect_ratio)
                .maybe_clamp(min_size, max_size);
        }

        let available = Size {
            width: AvailableSpace::Definite(area_width.maybe_clamp(min_size.width, max_size.width)),
            height: AvailableSpace::Definite(
                area_height.maybe_clamp(min_size.height, max_size.height),
            ),
        };
        let taffy_id = NodeId::from(node_id);
        let input = |known_dimensions, run_mode| LayoutInput {
            known_dimensions,
            parent_size: area_size.map(Some),
            available_space: available,
            sizing_mode: SizingMode::ContentSize,
            axis: RequestedAxis::Both,
            run_mode,
            vertical_margins_are_collapsible: Line::FALSE,
        };
        let measured = self
            .compute_child_layout(taffy_id, input(known, RunMode::ComputeSize))
            .size;
        let final_size = known.unwrap_or(measured).maybe_clamp(min_size, max_size);
        let output = self.compute_child_layout(
            taffy_id,
            input(final_size.map(Some), RunMode::PerformLayout),
        );

        // Auto margins only resolve when both insets of an axis are set.
        let auto_margin = |start: Option<f32>,
                           end: Option<f32>,
                           size: f32,
                           area: f32,
                           m: (Option<f32>, Option<f32>)| {
            let (Some(s), Some(e)) = (start, end) else {
                return (m.0.unwrap_or(0.0), m.1.unwrap_or(0.0));
            };
            let free = area - s - e - size - m.0.unwrap_or(0.0) - m.1.unwrap_or(0.0);
            match (m.0, m.1) {
                (None, None) => ((free / 2.0).max(0.0), (free / 2.0).max(0.0)),
                (None, Some(b)) => (free, b),
                (Some(a), None) => (a, free),
                (Some(a), Some(b)) => (a, b),
            }
        };
        let (ml, mr) = auto_margin(
            left,
            right,
            final_size.width,
            area_width,
            (margin.left, margin.right),
        );
        let (mt, mb) = auto_margin(
            top,
            bottom,
            final_size.height,
            area_height,
            (margin.top, margin.bottom),
        );
        let resolved_margin = Rect {
            left: ml,
            right: mr,
            top: mt,
            bottom: mb,
        };

        // Viewport position of the border box; fixed boxes do not move when the page scrolls.
        let scroll = self.viewport_scroll;
        let viewport_x = left
            .map(|l| l + ml)
            .or(right.map(|r| area_width - final_size.width - r - mr));
        let viewport_y = top
            .map(|t| t + mt)
            .or(bottom.map(|b| area_height - final_size.height - b - mb));

        let parent_pos = self.nodes[node_id]
            .layout_parent
            .get()
            .map(|p| {
                let pos = self.unrounded_position(p);
                let node = &self.nodes[p];
                Point {
                    x: pos.x - node.scroll_offset.x as f32,
                    y: pos.y - node.scroll_offset.y as f32,
                }
            })
            .unwrap_or(Point::ZERO);
        let old = self.nodes[node_id].unrounded_layout;
        let location = Point {
            x: viewport_x.map_or(old.location.x, |x| x + scroll.x as f32 - parent_pos.x),
            y: viewport_y.map_or(old.location.y, |y| y + scroll.y as f32 - parent_pos.y),
        };

        self.set_unrounded_layout(
            taffy_id,
            &Layout {
                order: old.order,
                size: final_size,
                content_size: output.content_size,
                scrollbar_size: old.scrollbar_size,
                location,
                padding,
                border,
                margin: resolved_margin,
            },
        );
    }
}
