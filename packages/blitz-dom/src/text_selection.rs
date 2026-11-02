//! PATCH(rust-ui): selecting the text of a page (not of a text field) with the mouse, and
//! copying it.
//!
//! Browsers let people drag across paragraphs, double-click a word, triple-click a paragraph,
//! and copy with Ctrl+C (Cmd+C on macOS). Upstream Blitz only had selection inside `<input>` and
//! `<textarea>`. Here a selection runs between two points, each a byte offset into the text of an
//! *inline root* (a block whose content is laid out as lines of text by Parley). Every inline root
//! between the two, in layout order, is fully selected. `blitz-paint` draws the highlight from
//! [`BaseDocument::text_selection_range`].
//!
//! Not supported: `user-select` (Stylo's Servo build does not parse it); instead a press on an
//! interactive element (button, link, form control, anything focusable or with `cursor:
//! pointer`) never starts a selection, so clicks on controls keep working as before.

use std::ops::Range;

use markup5ever::local_name;
use parley::layout::{Affinity, Cursor, Selection};

use crate::BaseDocument;

/// A point in the text of an inline root.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct TextPoint {
    /// The inline root (element or anonymous block) whose text this points into.
    pub node: usize,
    /// Byte offset into that root's text.
    pub offset: usize,
}

/// The document's text selection.
#[derive(Clone, Debug)]
pub struct TextSelection {
    /// Where the selection started (stays put while dragging).
    pub anchor: TextPoint,
    /// Where it ends (follows the pointer).
    pub focus: TextPoint,
    /// Every inline root from the first point to the last, in layout order.
    roots: Vec<usize>,
    /// Whether `anchor` comes after `focus`.
    backwards: bool,
}

impl TextSelection {
    pub fn is_collapsed(&self) -> bool {
        self.anchor == self.focus
    }

    fn start(&self) -> TextPoint {
        if self.backwards {
            self.focus
        } else {
            self.anchor
        }
    }

    fn end(&self) -> TextPoint {
        if self.backwards {
            self.anchor
        } else {
            self.focus
        }
    }
}

/// Granularity of a press: 1 click places a point, 2 select a word, 3 a whole paragraph.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Granularity {
    Character,
    Word,
    Paragraph,
}

impl BaseDocument {
    /// The current page-text selection, if any (it may be collapsed).
    pub fn text_selection(&self) -> Option<&TextSelection> {
        self.text_selection.as_ref()
    }

    /// The selected byte range of `node_id`'s inline text, for painting the highlight.
    pub fn text_selection_range(&self, node_id: usize) -> Option<Range<usize>> {
        let sel = self.text_selection.as_ref()?;
        if sel.is_collapsed() {
            return None;
        }
        if !sel.roots.contains(&node_id) {
            return None;
        }
        let len = self.inline_text(node_id)?.len();
        let (start, end) = (sel.start(), sel.end());
        let from = if start.node == node_id {
            start.offset.min(len)
        } else {
            0
        };
        let to = if end.node == node_id {
            end.offset.min(len)
        } else {
            len
        };
        (from < to).then_some(from..to)
    }

    /// The selected page text, with a line break between blocks. `None` when nothing is selected.
    pub fn selected_text(&self) -> Option<String> {
        let sel = self.text_selection.as_ref()?;
        let mut out = String::new();
        for &root in &sel.roots {
            let Some(range) = self.text_selection_range(root) else {
                continue;
            };
            let Some(text) = self.inline_text(root) else {
                continue;
            };
            let Some(part) = text.get(range) else {
                continue;
            };
            if !out.is_empty() {
                out.push('\n');
            }
            out.push_str(part.trim_end_matches('\n'));
        }
        (!out.is_empty()).then_some(out)
    }

    /// Remove the page-text selection. Returns whether there was one.
    pub fn clear_text_selection(&mut self) -> bool {
        self.text_selecting = false;
        let had = self
            .text_selection
            .take()
            .is_some_and(|s| !s.is_collapsed());
        if had {
            self.shell_provider.request_redraw();
        }
        had
    }

    /// Select all the text on the page (Ctrl+A / Cmd+A outside a text field). Returns whether
    /// there was any text to select.
    pub fn select_all_text(&mut self) -> bool {
        let roots = self.inline_roots_in_order();
        let (Some(&first), Some(&last)) = (roots.first(), roots.last()) else {
            return false;
        };
        let len = self.inline_text(last).map(str::len).unwrap_or(0);
        self.text_selection = Some(TextSelection {
            anchor: TextPoint {
                node: first,
                offset: 0,
            },
            focus: TextPoint {
                node: last,
                offset: len,
            },
            roots,
            backwards: false,
        });
        self.shell_provider.request_redraw();
        true
    }

    /// Select `range` (byte offsets) of the text of the inline root `node_id`.
    pub fn select_text_range(&mut self, node_id: usize, range: Range<usize>) -> bool {
        let Some(len) = self.inline_text(node_id).map(str::len) else {
            return false;
        };
        self.set_selection_points(
            TextPoint {
                node: node_id,
                offset: range.start.min(len),
            },
            TextPoint {
                node: node_id,
                offset: range.end.min(len),
            },
        );
        true
    }

    /// The point in page text under document coordinates `(x, y)`, if the point is over (or
    /// beside) a line of text.
    pub fn text_point_at(&self, x: f32, y: f32) -> Option<TextPoint> {
        let hit = self.hit(x, y)?;
        let root = self.inline_root_for(hit.node_id)?;
        let (lx, ly) = self.local_text_coords(root, x, y)?;
        let layout = &self.nodes[root]
            .element_data()?
            .inline_layout_data
            .as_ref()?
            .layout;
        let scale = layout.scale();
        let cursor = Cursor::from_point(layout, lx * scale, ly * scale);
        Some(TextPoint {
            node: root,
            offset: cursor.index(),
        })
    }

    /// A press at `(x, y)`: start a selection there (or clear the old one when the press is on a
    /// control or outside text). Returns whether anything visible changed.
    pub(crate) fn start_text_selection(
        &mut self,
        target: usize,
        x: f32,
        y: f32,
        clicks: u32,
        extend: bool,
    ) -> bool {
        if self.is_interactive(target) {
            return self.clear_text_selection();
        }
        let Some(point) = self.text_point_at(x, y) else {
            return self.clear_text_selection();
        };
        let granularity = match clicks {
            0 | 1 => Granularity::Character,
            2 => Granularity::Word,
            _ => Granularity::Paragraph,
        };
        self.text_selecting = true;
        if extend {
            if let Some(anchor) = self.text_selection.as_ref().map(|s| s.anchor) {
                self.set_selection_points(anchor, point);
                return true;
            }
        }
        let (anchor, focus) = match granularity {
            Granularity::Character => (point, point),
            Granularity::Word | Granularity::Paragraph => {
                let Some((lx, ly)) = self.local_text_coords(point.node, x, y) else {
                    return false;
                };
                let Some(layout) = self.nodes[point.node]
                    .element_data()
                    .and_then(|el| el.inline_layout_data.as_ref())
                    .map(|d| &d.layout)
                else {
                    return false;
                };
                let scale = layout.scale();
                let range = if granularity == Granularity::Word {
                    Selection::word_from_point(layout, lx * scale, ly * scale).text_range()
                } else {
                    0..self.inline_text(point.node).map(str::len).unwrap_or(0)
                };
                (
                    TextPoint {
                        node: point.node,
                        offset: range.start,
                    },
                    TextPoint {
                        node: point.node,
                        offset: range.end,
                    },
                )
            }
        };
        self.set_selection_points(anchor, focus);
        true
    }

    /// The pointer moved with the main button down: move the selection's focus.
    pub(crate) fn extend_text_selection(&mut self, x: f32, y: f32) -> bool {
        if !self.text_selecting {
            return false;
        }
        let Some(anchor) = self.text_selection.as_ref().map(|s| s.anchor) else {
            return false;
        };
        let Some(point) = self.text_point_at(x, y) else {
            return false;
        };
        if self
            .text_selection
            .as_ref()
            .is_some_and(|s| s.focus == point)
        {
            return false;
        }
        self.set_selection_points(anchor, point);
        true
    }

    pub(crate) fn end_text_selection_drag(&mut self) {
        self.text_selecting = false;
    }

    fn set_selection_points(&mut self, anchor: TextPoint, focus: TextPoint) {
        let order = self.inline_roots_in_order();
        let pos = |p: &TextPoint| order.iter().position(|&n| n == p.node);
        let (Some(a), Some(f)) = (pos(&anchor), pos(&focus)) else {
            return;
        };
        let backwards = (f, focus.offset) < (a, anchor.offset);
        let (lo, hi) = if backwards { (f, a) } else { (a, f) };
        self.text_selection = Some(TextSelection {
            anchor,
            focus,
            roots: order[lo..=hi].to_vec(),
            backwards,
        });
        self.shell_provider.request_redraw();
    }

    /// The text of an inline root.
    fn inline_text(&self, node_id: usize) -> Option<&str> {
        let node = self.nodes.get(node_id)?;
        if !node.flags.is_inline_root() {
            return None;
        }
        Some(
            node.element_data()?
                .inline_layout_data
                .as_ref()?
                .text
                .as_str(),
        )
    }

    /// The nearest inline root at or above `node_id` in the layout tree.
    fn inline_root_for(&self, node_id: usize) -> Option<usize> {
        let mut id = Some(node_id);
        while let Some(i) = id {
            let node = self.nodes.get(i)?;
            if node.flags.is_inline_root()
                && node
                    .element_data()
                    .is_some_and(|el| el.inline_layout_data.is_some())
            {
                return Some(i);
            }
            id = node.layout_parent.get().or(node.parent);
        }
        None
    }

    /// Document coordinates to coordinates in `root`'s text layout (CSS pixels).
    fn local_text_coords(&self, root: usize, x: f32, y: f32) -> Option<(f32, f32)> {
        let node = self.nodes.get(root)?;
        let origin = node.absolute_position(0.0, 0.0);
        let l = &node.final_layout;
        Some((
            x - origin.x - l.padding.left - l.border.left,
            y - origin.y - l.padding.top - l.border.top,
        ))
    }

    /// Every inline root that is rendered, in layout (reading) order.
    fn inline_roots_in_order(&self) -> Vec<usize> {
        let mut out = Vec::new();
        let mut stack = vec![self.root_element().id];
        while let Some(id) = stack.pop() {
            let node = &self.nodes[id];
            if node.is_element() && node.style.display == taffy::Display::None {
                continue;
            }
            if node.flags.is_inline_root()
                && node
                    .element_data()
                    .and_then(|el| el.inline_layout_data.as_ref())
                    .is_some_and(|d| !d.text.trim().is_empty())
            {
                out.push(id);
            }
            if let Some(children) = node.layout_children.borrow().as_ref() {
                stack.extend(children.iter().rev().copied());
            }
        }
        out
    }

    /// Whether a press on `node_id` belongs to a control rather than to text.
    fn is_interactive(&self, node_id: usize) -> bool {
        let mut id = Some(node_id);
        while let Some(i) = id {
            let node = &self.nodes[i];
            if let Some(el) = node.element_data() {
                let interactive_tag = matches!(
                    el.name.local,
                    local_name!("button")
                        | local_name!("input")
                        | local_name!("select")
                        | local_name!("textarea")
                        | local_name!("summary")
                        | local_name!("label")
                ) || (el.name.local == local_name!("a")
                    && el.attr(local_name!("href")).is_some());
                let interactive_role = matches!(
                    el.attr(local_name!("role")),
                    Some(
                        "button"
                            | "link"
                            | "checkbox"
                            | "radio"
                            | "switch"
                            | "tab"
                            | "menuitem"
                            | "menuitemcheckbox"
                            | "menuitemradio"
                            | "option"
                            | "slider"
                            | "combobox"
                            | "treeitem"
                    )
                );
                if interactive_tag
                    || interactive_role
                    || el.is_focussable
                    || el.attr(local_name!("tabindex")).is_some()
                    || el.attr(local_name!("contenteditable")).is_some()
                {
                    return true;
                }
                let pointer = node.primary_styles().is_some_and(|s| {
                    matches!(
                        s.get_inherited_ui().cursor.keyword,
                        style::values::computed::ui::CursorKind::Pointer
                    )
                });
                if pointer {
                    return true;
                }
            }
            id = node.parent;
        }
        false
    }
}

/// The byte range `range` of `layout` as a Parley selection (for drawing its geometry).
pub fn layout_selection<B: parley::Brush>(
    layout: &parley::Layout<B>,
    range: Range<usize>,
) -> Selection {
    Selection::new(
        Cursor::from_byte_index(layout, range.start, Affinity::Downstream),
        Cursor::from_byte_index(layout, range.end, Affinity::Upstream),
    )
}
