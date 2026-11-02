//! PATCH(rust-ui): the accessibility tree handed to screen readers through AccessKit.
//!
//! Upstream mapped a handful of tag names to roles and nothing else, so a screen reader heard
//! "unknown" for most of a page and no control had a name. This version follows the browser
//! rules closely enough for a component kit built on WAI-ARIA patterns:
//!
//! - roles from `role="…"` (first known token), else the HTML-AAM mapping of the element;
//! - names from `aria-labelledby`, `aria-label`, the native label (`<label for>`, a wrapping
//!   `<label>`, `alt`, `<legend>`, `<caption>`, a button's `value`), content (for roles that are
//!   named by their text), then `title` and `placeholder` (a simplified accname 1.2);
//! - descriptions from `aria-describedby` and `aria-description`;
//! - states: disabled, checked/pressed, selected, expanded, required, read-only, invalid,
//!   modal, multiselectable, busy, current, level, position in set, orientation, values of
//!   ranges and fields, live regions, `aria-haspopup`;
//! - relations: `aria-controls`, `aria-activedescendant`, `aria-labelledby`,
//!   `aria-describedby`, `aria-errormessage`;
//! - bounds in window pixels (for the screen reader's highlight and touch exploration);
//! - the actions a screen reader may ask for: focus, click, and set value on text fields.
//!
//! Hidden content (`display: none`, `visibility: hidden`, `hidden`, `aria-hidden="true"`,
//! `inert`) is left out with its subtree, and `role="none"`/`"presentation"` drop the element's
//! own semantics. [`BaseDocument::accessibility_action_target`] maps AccessKit node ids back to
//! DOM nodes for the embedder that carries out actions.

use std::collections::{HashMap, HashSet};

use crate::{BaseDocument, ElementData, Node as BlitzDomNode, local_name};
use accesskit::{
    Action, AriaCurrent, HasPopup, Invalid, Live, Node as AccessKitNode, NodeId, Orientation, Rect,
    Role, Toggled, Tree, TreeUpdate,
};

/// AccessKit id of the window (the tree's root).
pub const WINDOW_NODE_ID: NodeId = NodeId(u64::MAX);

impl BaseDocument {
    pub fn build_accessibility_tree(&self) -> TreeUpdate {
        // Pass 1: which DOM nodes appear in the tree (hidden subtrees are left out).
        let mut included: HashSet<usize> = HashSet::new();
        let root = self.root_element().id;
        let mut stack = vec![root];
        while let Some(id) = stack.pop() {
            let node = &self.nodes[id];
            if self.is_hidden_from_accessibility(node) {
                continue;
            }
            if node.is_text_node() {
                if node.text_content().trim().is_empty() {
                    continue;
                }
            } else if !node.is_element() {
                continue;
            }
            included.insert(id);
            stack.extend(node.children.iter().rev().copied());
        }

        // Pass 2: build the nodes.
        let mut window = AccessKitNode::new(Role::Window);
        if let Some(title) = self.find_title_node().map(|n| n.text_content()) {
            let title = collapse_whitespace(&title);
            if !title.is_empty() {
                window.set_label(title);
            }
        }
        window.push_child(NodeId(root as u64));
        let (w, h) = self.viewport.window_size;
        window.set_bounds(Rect::new(0.0, 0.0, w as f64, h as f64));

        let mut nodes = Vec::with_capacity(included.len() + 1);
        for &id in &included {
            let node = &self.nodes[id];
            let mut out = if node.is_text_node() {
                // Static text. AccessKit reads a Label's text from its value (AT-SPI and UIA
                // name it from there), so set both.
                let mut text = AccessKitNode::new(Role::Label);
                let content = collapse_whitespace(&node.text_content());
                text.set_value(content.as_str());
                text.set_label(content);
                if let Some(bounds) = node.parent.and_then(|p| self.accessibility_bounds(p)) {
                    text.set_bounds(bounds);
                }
                text
            } else {
                self.build_element_node(node, &included)
            };
            for &child in &node.children {
                if included.contains(&child) {
                    out.push_child(NodeId(child as u64));
                }
            }
            nodes.push((NodeId(id as u64), out));
        }
        nodes.push((WINDOW_NODE_ID, window));

        let focus = self
            .focus_node_id
            .filter(|id| included.contains(id))
            .map(|id| NodeId(id as u64))
            .unwrap_or(WINDOW_NODE_ID);

        TreeUpdate {
            nodes,
            tree: Some(Tree::new(WINDOW_NODE_ID)),
            focus,
        }
    }

    /// The DOM node an AccessKit action request is for (`None` for the window or an unknown
    /// node).
    pub fn accessibility_action_target(&self, target: NodeId) -> Option<usize> {
        if target == WINDOW_NODE_ID {
            return None;
        }
        let id = usize::try_from(target.0).ok()?;
        self.nodes.get(id).map(|_| id)
    }

    /// Centre of a node in window coordinates (logical pixels), where a synthetic press for an
    /// accessibility "click" lands.
    pub fn accessibility_click_point(&self, node_id: usize) -> Option<(f32, f32)> {
        let node = self.nodes.get(node_id)?;
        let origin = node.absolute_position(0.0, 0.0);
        let size = node.final_layout.size;
        let zoom = self.viewport.zoom();
        Some((
            (origin.x + size.width / 2.0 - self.viewport_scroll.x as f32) * zoom,
            (origin.y + size.height / 2.0 - self.viewport_scroll.y as f32) * zoom,
        ))
    }

    fn build_element_node(&self, node: &BlitzDomNode, included: &HashSet<usize>) -> AccessKitNode {
        let el = node.element_data().unwrap();
        let tag = el.name.local.to_string();
        let role = self.accessibility_role(node);
        let mut out = AccessKitNode::new(role);
        out.set_html_tag(tag.as_str());
        if let Some(bounds) = self.accessibility_bounds(node.id) {
            out.set_bounds(bounds);
        }

        // Name and description.
        let name = self.accessible_name(node.id, role);
        if !name.is_empty() {
            if role == Role::Label {
                // Platform adapters read a Label's text from its value.
                out.set_value(name.as_str());
            }
            out.set_label(name);
        }
        let description = self.accessible_description(el);
        if !description.is_empty() {
            out.set_description(description);
        }
        if let Some(v) = attr(el, "aria-roledescription") {
            out.set_role_description(v);
        }
        if let Some(v) = attr(el, "aria-keyshortcuts") {
            out.set_keyboard_shortcut(v);
        }
        if let Some(v) = el
            .attr(local_name!("placeholder"))
            .filter(|v| !v.is_empty())
        {
            out.set_placeholder(v);
        }

        // States.
        let disabled = el.attr(local_name!("disabled")).is_some()
            || attr(el, "aria-disabled") == Some("true")
            || self.in_disabled_fieldset(node);
        if disabled {
            out.set_disabled();
        }
        let checked = match attr(el, "aria-checked").or_else(|| attr(el, "aria-pressed")) {
            Some("true") => Some(Toggled::True),
            Some("mixed") => Some(Toggled::Mixed),
            Some("false") => Some(Toggled::False),
            _ => {
                el.checkbox_input_checked()
                    .map(|c| if c { Toggled::True } else { Toggled::False })
            }
        };
        if let Some(checked) = checked {
            out.set_toggled(checked);
        }
        match attr(el, "aria-selected") {
            Some("true") => out.set_selected(true),
            Some("false") => out.set_selected(false),
            _ => {
                if el.name.local == local_name!("option") {
                    out.set_selected(el.attr(local_name!("selected")).is_some());
                }
            }
        }
        match attr(el, "aria-expanded") {
            Some("true") => out.set_expanded(true),
            Some("false") => out.set_expanded(false),
            _ => {
                if el.name.local == local_name!("details") {
                    out.set_expanded(el.attr(local_name!("open")).is_some());
                }
            }
        }
        if el.attr(local_name!("required")).is_some() || attr(el, "aria-required") == Some("true") {
            out.set_required();
        }
        if el.attr(local_name!("readonly")).is_some() || attr(el, "aria-readonly") == Some("true") {
            out.set_read_only();
        }
        match attr(el, "aria-invalid") {
            Some("grammar") => out.set_invalid(Invalid::Grammar),
            Some("spelling") => out.set_invalid(Invalid::Spelling),
            Some("false") | Some("") | None => {}
            Some(_) => out.set_invalid(Invalid::True),
        }
        if attr(el, "aria-modal") == Some("true") {
            out.set_modal();
        }
        if attr(el, "aria-multiselectable") == Some("true")
            || (el.name.local == local_name!("select")
                && el.attr(local_name!("multiple")).is_some())
        {
            out.set_multiselectable();
        }
        if attr(el, "aria-busy") == Some("true") {
            out.set_busy();
        }
        if let Some(current) = attr(el, "aria-current") {
            let current = match current {
                "page" => Some(AriaCurrent::Page),
                "step" => Some(AriaCurrent::Step),
                "location" => Some(AriaCurrent::Location),
                "date" => Some(AriaCurrent::Date),
                "time" => Some(AriaCurrent::Time),
                "false" | "" => None,
                _ => Some(AriaCurrent::True),
            };
            if let Some(current) = current {
                out.set_aria_current(current);
            }
        }
        if let Some(popup) = attr(el, "aria-haspopup") {
            let popup = match popup {
                "menu" | "true" => Some(HasPopup::Menu),
                "listbox" => Some(HasPopup::Listbox),
                "tree" => Some(HasPopup::Tree),
                "grid" => Some(HasPopup::Grid),
                "dialog" => Some(HasPopup::Dialog),
                _ => None,
            };
            if let Some(popup) = popup {
                out.set_has_popup(popup);
            }
        }
        match attr(el, "aria-orientation") {
            Some("horizontal") => out.set_orientation(Orientation::Horizontal),
            Some("vertical") => out.set_orientation(Orientation::Vertical),
            _ => {}
        }
        let level = attr(el, "aria-level")
            .and_then(|v| v.parse::<usize>().ok())
            .or(match &*el.name.local {
                "h1" => Some(1),
                "h2" => Some(2),
                "h3" => Some(3),
                "h4" => Some(4),
                "h5" => Some(5),
                "h6" => Some(6),
                _ => None,
            });
        if let Some(level) = level {
            out.set_level(level);
        }
        if let Some(pos) = attr(el, "aria-posinset").and_then(|v| v.parse().ok()) {
            out.set_position_in_set(pos);
        }
        if let Some(size) = attr(el, "aria-setsize").and_then(|v| v.parse().ok()) {
            out.set_size_of_set(size);
        }

        // Live regions.
        let live = match attr(el, "aria-live") {
            Some("assertive") => Some(Live::Assertive),
            Some("polite") => Some(Live::Polite),
            Some("off") => Some(Live::Off),
            _ => match role {
                Role::Alert => Some(Live::Assertive),
                Role::Status | Role::Log => Some(Live::Polite),
                _ => None,
            },
        };
        if let Some(live) = live {
            out.set_live(live);
            if attr(el, "aria-atomic") == Some("true") || matches!(role, Role::Alert | Role::Status)
            {
                out.set_live_atomic();
            }
        }

        // Values.
        self.set_values(node, el, role, &mut out);

        // Relations (only to nodes that are in the tree).
        let ids = |name: &str| -> Vec<NodeId> {
            attr(el, name)
                .map(|v| {
                    v.split_ascii_whitespace()
                        .filter_map(|id| self.nodes_to_id.get(id).copied())
                        .filter(|id| included.contains(id))
                        .map(|id| NodeId(id as u64))
                        .collect()
                })
                .unwrap_or_default()
        };
        for id in ids("aria-controls") {
            out.push_controlled(id);
        }
        for id in ids("aria-labelledby") {
            out.push_labelled_by(id);
        }
        for id in ids("aria-describedby") {
            out.push_described_by(id);
        }
        if let Some(&id) = ids("aria-activedescendant").first() {
            out.set_active_descendant(id);
        }
        if let Some(&id) = ids("aria-errormessage").first() {
            out.set_error_message(id);
        }
        if let Some(href) = el
            .attr(local_name!("href"))
            .filter(|_| el.name.local == local_name!("a"))
        {
            out.set_url(href);
        }

        // Actions.
        let focusable =
            !disabled && (el.is_focussable || el.attr(local_name!("tabindex")).is_some());
        if focusable {
            out.add_action(Action::Focus);
        }
        if !disabled && is_clickable(role) {
            out.add_action(Action::Click);
        }
        if !disabled && el.text_input_data().is_some() && el.attr(local_name!("readonly")).is_none()
        {
            out.add_action(Action::SetValue);
        }
        if self.is_scroll_container_node(node) {
            out.set_clips_children();
        }

        out
    }

    fn set_values(
        &self,
        node: &BlitzDomNode,
        el: &ElementData,
        role: Role,
        out: &mut AccessKitNode,
    ) {
        // Range values (ARIA first, then the native element).
        let num = |name: &str| attr(el, name).and_then(|v| v.trim().parse::<f64>().ok());
        let mut now = num("aria-valuenow");
        let mut min = num("aria-valuemin");
        let mut max = num("aria-valuemax");
        if el.name.local == local_name!("progress") {
            let value = el.attr(local_name!("value")).and_then(|v| v.parse().ok());
            now = now.or(value);
            min = min.or(Some(0.0));
            max = max.or_else(|| {
                el.attr(local_name!("max"))
                    .and_then(|v| v.parse().ok())
                    .or(Some(1.0))
            });
        }
        if el.name.local == local_name!("input") && el.attr(local_name!("type")) == Some("range") {
            now = now.or_else(|| el.attr(local_name!("value")).and_then(|v| v.parse().ok()));
            min = min.or_else(|| {
                el.attr(local_name!("min"))
                    .and_then(|v| v.parse().ok())
                    .or(Some(0.0))
            });
            max = max.or_else(|| {
                el.attr(local_name!("max"))
                    .and_then(|v| v.parse().ok())
                    .or(Some(100.0))
            });
        }
        if let Some(v) = now {
            out.set_numeric_value(v);
        }
        if let Some(v) = min {
            out.set_min_numeric_value(v);
        }
        if let Some(v) = max {
            out.set_max_numeric_value(v);
        }
        if let Some(text) = attr(el, "aria-valuetext") {
            out.set_value(text);
            return;
        }

        // Text fields: their text (masked for passwords).
        if let Some(input) = el.text_input_data() {
            let text = input.editor.raw_text();
            let value = if el.attr(local_name!("type")) == Some("password") {
                "•".repeat(text.chars().count())
            } else {
                text.to_string()
            };
            out.set_value(value);
            return;
        }

        // <select>: the selected option's text.
        if el.name.local == local_name!("select") {
            let mut first = None;
            let mut chosen = None;
            self.walk(node.id, &mut |n| {
                if let Some(e) = n.element_data() {
                    if e.name.local == local_name!("option") {
                        first.get_or_insert(n.id);
                        if e.attr(local_name!("selected")).is_some() && chosen.is_none() {
                            chosen = Some(n.id);
                        }
                    }
                }
            });
            if let Some(opt) = chosen.or(first) {
                out.set_value(collapse_whitespace(&self.nodes[opt].text_content()));
            }
            return;
        }

        // A combobox that is not a text field (a button that opens a list) reads its text as
        // its value, as browsers do.
        if role == Role::ComboBox {
            let text = collapse_whitespace(&self.text_from_content(node.id, &mut HashSet::new()));
            if !text.is_empty() {
                out.set_value(text);
            }
        }
    }

    fn accessibility_bounds(&self, node_id: usize) -> Option<Rect> {
        let node = self.nodes.get(node_id)?;
        let origin = node.absolute_position(0.0, 0.0);
        let size = node.final_layout.size;
        let scale = self.viewport.scale_f64() * self.viewport.zoom() as f64;
        let x = (origin.x as f64 - self.viewport_scroll.x) * scale;
        let y = (origin.y as f64 - self.viewport_scroll.y) * scale;
        Some(Rect::new(
            x,
            y,
            x + size.width as f64 * scale,
            y + size.height as f64 * scale,
        ))
    }

    fn is_scroll_container_node(&self, node: &BlitzDomNode) -> bool {
        node.is_scroll_container()
    }

    fn in_disabled_fieldset(&self, node: &BlitzDomNode) -> bool {
        let el = node.element_data().unwrap();
        let form_control = matches!(
            el.name.local,
            local_name!("input")
                | local_name!("button")
                | local_name!("select")
                | local_name!("textarea")
        );
        if !form_control {
            return false;
        }
        let mut id = node.parent;
        while let Some(i) = id {
            let n = &self.nodes[i];
            if let Some(e) = n.element_data() {
                if e.name.local == local_name!("fieldset")
                    && e.attr(local_name!("disabled")).is_some()
                {
                    return true;
                }
            }
            id = n.parent;
        }
        false
    }

    /// Whether a node (and so its subtree) is left out of the accessibility tree.
    fn is_hidden_from_accessibility(&self, node: &BlitzDomNode) -> bool {
        let Some(el) = node.element_data() else {
            return false;
        };
        if matches!(
            el.name.local,
            local_name!("head")
                | local_name!("script")
                | local_name!("style")
                | local_name!("template")
                | local_name!("title")
                | local_name!("meta")
                | local_name!("link")
                | local_name!("noscript")
        ) {
            return true;
        }
        if attr(el, "aria-hidden") == Some("true")
            || el.attr(local_name!("hidden")).is_some()
            || attr(el, "inert").is_some()
        {
            return true;
        }
        if node.style.display == taffy::Display::None {
            return true;
        }
        node.primary_styles().is_some_and(|s| {
            s.get_box().display.is_none()
                || s.get_inherited_box().visibility
                    != style::computed_values::visibility::T::Visible
        })
    }

    /// The role of an element: the first known token of `role`, else the implicit one.
    pub fn accessibility_role(&self, node: &BlitzDomNode) -> Role {
        let Some(el) = node.element_data() else {
            return Role::Unknown;
        };
        if let Some(roles) = el.attr(local_name!("role")) {
            for token in roles.split_ascii_whitespace() {
                if matches!(token, "none" | "presentation") {
                    // Focusable elements keep their semantics (ARIA presentational conflict).
                    if el.is_focussable || el.attr(local_name!("tabindex")).is_some() {
                        break;
                    }
                    return Role::GenericContainer;
                }
                if let Some(role) = role_from_aria(token) {
                    return role;
                }
            }
        }
        implicit_role(el).unwrap_or(Role::GenericContainer)
    }

    /// The accessible name of an element (simplified accname 1.2).
    pub fn accessible_name(&self, node_id: usize, role: Role) -> String {
        let el = match self.nodes[node_id].element_data() {
            Some(el) => el,
            None => return String::new(),
        };
        // 1. aria-labelledby
        if let Some(ids) = attr(el, "aria-labelledby") {
            let mut visited = HashSet::from([node_id]);
            let text = ids
                .split_ascii_whitespace()
                .filter_map(|id| self.nodes_to_id.get(id).copied())
                .map(|id| {
                    let n = &self.nodes[id];
                    if let Some(label) = n.element_data().and_then(|e| attr(e, "aria-label")) {
                        label.to_string()
                    } else {
                        self.text_from_content(id, &mut visited)
                    }
                })
                .collect::<Vec<_>>()
                .join(" ");
            let text = collapse_whitespace(&text);
            if !text.is_empty() {
                return text;
            }
        }
        // 2. aria-label
        if let Some(label) = attr(el, "aria-label") {
            let label = collapse_whitespace(label);
            if !label.is_empty() {
                return label;
            }
        }
        // 3. Native labels
        if let Some(native) = self.native_name(node_id, el) {
            let native = collapse_whitespace(&native);
            if !native.is_empty() {
                return native;
            }
        }
        // 4. Content. Focusable elements whose role is not named by content (a draggable list
        // item, a focusable row) also fall back to it, so focus landing on one reads something.
        let focusable = el.is_focussable || el.attr(local_name!("tabindex")).is_some();
        let container = matches!(
            role,
            Role::GenericContainer
                | Role::Unknown
                | Role::Dialog
                | Role::AlertDialog
                | Role::Menu
                | Role::ListBox
                | Role::TabPanel
                | Role::Grid
                | Role::Tree
                | Role::Table
                | Role::List
        );
        if name_from_content(role) || (focusable && !container) {
            let text = collapse_whitespace(&self.text_from_content(node_id, &mut HashSet::new()));
            if !text.is_empty() {
                return text;
            }
        }
        // 5. Tooltip attribute
        if let Some(title) = el.attr(local_name!("title")) {
            let title = collapse_whitespace(title);
            if !title.is_empty() {
                return title;
            }
        }
        String::new()
    }

    fn accessible_description(&self, el: &ElementData) -> String {
        if let Some(ids) = attr(el, "aria-describedby") {
            let mut visited = HashSet::new();
            let text = ids
                .split_ascii_whitespace()
                .filter_map(|id| self.nodes_to_id.get(id).copied())
                .map(|id| self.text_from_content(id, &mut visited))
                .collect::<Vec<_>>()
                .join(" ");
            let text = collapse_whitespace(&text);
            if !text.is_empty() {
                return text;
            }
        }
        attr(el, "aria-description")
            .map(collapse_whitespace)
            .unwrap_or_default()
    }

    fn native_name(&self, node_id: usize, el: &ElementData) -> Option<String> {
        let tag = &*el.name.local;
        let input_type = el.attr(local_name!("type")).unwrap_or("text");
        match tag {
            "input" if matches!(input_type, "button" | "submit" | "reset") => {
                return Some(
                    el.attr(local_name!("value"))
                        .map(str::to_string)
                        .unwrap_or_else(|| match input_type {
                            "submit" => "Submit".into(),
                            "reset" => "Reset".into(),
                            _ => String::new(),
                        }),
                );
            }
            "input" if input_type == "image" => {
                return el.attr(local_name!("alt")).map(str::to_string);
            }
            "img" | "area" => return el.attr(local_name!("alt")).map(str::to_string),
            "fieldset" => return self.first_child_text(node_id, local_name!("legend")),
            "table" => return self.first_child_text(node_id, local_name!("caption")),
            "figure" => return self.first_child_text(node_id, local_name!("figcaption")),
            _ => {}
        }
        let labelable = matches!(
            tag,
            "input" | "select" | "textarea" | "button" | "meter" | "output" | "progress"
        );
        if !labelable {
            return None;
        }
        // <label for="id">
        let mut labels = Vec::new();
        if let Some(id) = el.id.as_ref() {
            let id = id.to_string();
            self.walk(self.root_element().id, &mut |n| {
                if let Some(e) = n.element_data() {
                    if e.name.local == local_name!("label")
                        && e.attr(local_name!("for")) == Some(id.as_str())
                    {
                        labels.push(n.id);
                    }
                }
            });
        }
        // A wrapping <label>
        let mut parent = self.nodes[node_id].parent;
        while let Some(p) = parent {
            let n = &self.nodes[p];
            if n.element_data()
                .is_some_and(|e| e.name.local == local_name!("label"))
            {
                labels.push(p);
                break;
            }
            parent = n.parent;
        }
        if !labels.is_empty() {
            let mut visited = HashSet::from([node_id]);
            let text = labels
                .into_iter()
                .map(|l| self.text_from_content(l, &mut visited))
                .collect::<Vec<_>>()
                .join(" ");
            return Some(text);
        }
        if el.text_input_data().is_some() {
            // Placeholder is the last resort for text fields (after title).
            if let Some(title) = el.attr(local_name!("title")) {
                return Some(title.to_string());
            }
            return el.attr(local_name!("placeholder")).map(str::to_string);
        }
        None
    }

    fn first_child_text(&self, node_id: usize, tag: markup5ever::LocalName) -> Option<String> {
        self.nodes[node_id]
            .children
            .iter()
            .find(|&&c| {
                self.nodes[c]
                    .element_data()
                    .is_some_and(|e| e.name.local == tag)
            })
            .map(|&c| self.text_from_content(c, &mut HashSet::new()))
    }

    /// The text of a subtree as a name: text nodes, `aria-label`s and `alt`s of elements in it,
    /// field values, skipping hidden parts.
    fn text_from_content(&self, node_id: usize, visited: &mut HashSet<usize>) -> String {
        let mut out = String::new();
        self.collect_text(node_id, visited, &mut out, true);
        out
    }

    fn collect_text(
        &self,
        node_id: usize,
        visited: &mut HashSet<usize>,
        out: &mut String,
        root: bool,
    ) {
        let node = &self.nodes[node_id];
        if node.is_text_node() {
            out.push_str(&node.text_content());
            return;
        }
        let Some(el) = node.element_data() else {
            return;
        };
        if !root && !visited.insert(node_id) {
            return;
        }
        if self.is_hidden_from_accessibility(node) && !root {
            return;
        }
        if !root {
            if let Some(label) = attr(el, "aria-label").filter(|l| !l.trim().is_empty()) {
                out.push(' ');
                out.push_str(label);
                out.push(' ');
                return;
            }
            if el.name.local == local_name!("img") {
                if let Some(alt) = el.attr(local_name!("alt")) {
                    out.push_str(alt);
                }
                return;
            }
            if let Some(input) = el.text_input_data() {
                out.push(' ');
                out.push_str(input.editor.raw_text());
                out.push(' ');
                return;
            }
        }
        let block = node.style.display != taffy::Display::None
            && !matches!(
                &*el.name.local,
                "span"
                    | "a"
                    | "b"
                    | "i"
                    | "em"
                    | "strong"
                    | "code"
                    | "kbd"
                    | "small"
                    | "sup"
                    | "sub"
                    | "mark"
                    | "abbr"
                    | "label"
                    | "s"
                    | "u"
            );
        if block {
            out.push(' ');
        }
        for &child in &node.children {
            self.collect_text(child, visited, out, false);
        }
        if block {
            out.push(' ');
        }
    }

    fn walk(&self, node_id: usize, f: &mut impl FnMut(&BlitzDomNode)) {
        let node = &self.nodes[node_id];
        f(node);
        for &child in &node.children {
            self.walk(child, f);
        }
    }

    /// The tree as a map from DOM node id to the AccessKit node built for it (the window left
    /// out), for tests and debugging.
    pub fn accessibility_nodes(&self) -> HashMap<usize, AccessKitNode> {
        self.build_accessibility_tree()
            .nodes
            .into_iter()
            .filter(|(id, _)| *id != WINDOW_NODE_ID)
            .map(|(id, node)| (id.0 as usize, node))
            .collect()
    }
}

fn attr<'a>(el: &'a ElementData, name: &str) -> Option<&'a str> {
    el.attrs()
        .iter()
        .find(|a| &*a.name.local == name)
        .map(|a| a.value.as_str())
}

fn collapse_whitespace(text: &str) -> String {
    text.split_whitespace().collect::<Vec<_>>().join(" ")
}

/// Roles that take their name from their content when nothing else names them.
fn name_from_content(role: Role) -> bool {
    matches!(
        role,
        Role::Button
            | Role::CheckBox
            | Role::Switch
            | Role::RadioButton
            | Role::Link
            | Role::MenuItem
            | Role::MenuItemCheckBox
            | Role::MenuItemRadio
            | Role::ListBoxOption
            | Role::Tab
            | Role::TreeItem
            | Role::Heading
            | Role::Cell
            | Role::ColumnHeader
            | Role::RowHeader
            | Role::Tooltip
            | Role::Label
            | Role::DisclosureTriangle
            | Role::Caption
            | Role::Term
            | Role::Definition
    )
}

/// Roles a screen reader can "click" (activate).
fn is_clickable(role: Role) -> bool {
    matches!(
        role,
        Role::Button
            | Role::CheckBox
            | Role::Switch
            | Role::RadioButton
            | Role::Link
            | Role::MenuItem
            | Role::MenuItemCheckBox
            | Role::MenuItemRadio
            | Role::ListBoxOption
            | Role::Tab
            | Role::TreeItem
            | Role::ComboBox
            | Role::DisclosureTriangle
            | Role::ColorWell
    )
}

fn role_from_aria(name: &str) -> Option<Role> {
    Some(match name {
        "alert" => Role::Alert,
        "alertdialog" => Role::AlertDialog,
        "application" => Role::Application,
        "article" => Role::Article,
        "banner" => Role::Banner,
        "blockquote" => Role::Blockquote,
        "button" => Role::Button,
        "caption" => Role::Caption,
        "cell" => Role::Cell,
        "checkbox" => Role::CheckBox,
        "code" => Role::Code,
        "columnheader" => Role::ColumnHeader,
        "combobox" => Role::ComboBox,
        "complementary" => Role::Complementary,
        "contentinfo" => Role::ContentInfo,
        "definition" => Role::Definition,
        "dialog" => Role::Dialog,
        "directory" => Role::List,
        "document" => Role::Document,
        "emphasis" => Role::Emphasis,
        "feed" => Role::Feed,
        "figure" => Role::Figure,
        "form" => Role::Form,
        "generic" => Role::GenericContainer,
        "grid" => Role::Grid,
        "gridcell" => Role::Cell,
        "group" => Role::Group,
        "heading" => Role::Heading,
        "img" | "image" => Role::Image,
        "link" => Role::Link,
        "list" => Role::List,
        "listbox" => Role::ListBox,
        "listitem" => Role::ListItem,
        "log" => Role::Log,
        "main" => Role::Main,
        "mark" => Role::Mark,
        "marquee" => Role::Marquee,
        "math" => Role::Math,
        "menu" => Role::Menu,
        "menubar" => Role::MenuBar,
        "menuitem" => Role::MenuItem,
        "menuitemcheckbox" => Role::MenuItemCheckBox,
        "menuitemradio" => Role::MenuItemRadio,
        "meter" => Role::Meter,
        "navigation" => Role::Navigation,
        "note" => Role::Note,
        "option" => Role::ListBoxOption,
        "paragraph" => Role::Paragraph,
        "progressbar" => Role::ProgressIndicator,
        "radio" => Role::RadioButton,
        "radiogroup" => Role::RadioGroup,
        "region" => Role::Region,
        "row" => Role::Row,
        "rowgroup" => Role::RowGroup,
        "rowheader" => Role::RowHeader,
        "scrollbar" => Role::ScrollBar,
        "search" => Role::Search,
        "searchbox" => Role::SearchInput,
        "separator" => Role::Splitter,
        "slider" => Role::Slider,
        "spinbutton" => Role::SpinButton,
        "status" => Role::Status,
        "strong" => Role::Strong,
        "switch" => Role::Switch,
        "tab" => Role::Tab,
        "table" => Role::Table,
        "tablist" => Role::TabList,
        "tabpanel" => Role::TabPanel,
        "term" => Role::Term,
        "textbox" => Role::TextInput,
        "time" => Role::Time,
        "timer" => Role::Timer,
        "toolbar" => Role::Toolbar,
        "tooltip" => Role::Tooltip,
        "tree" => Role::Tree,
        "treegrid" => Role::TreeGrid,
        "treeitem" => Role::TreeItem,
        _ => return None,
    })
}

/// Implicit roles (HTML-AAM).
fn implicit_role(el: &ElementData) -> Option<Role> {
    Some(match &*el.name.local {
        "article" => Role::Article,
        "aside" => Role::Complementary,
        "footer" => Role::ContentInfo,
        "header" => Role::Banner,
        "main" => Role::Main,
        "nav" => Role::Navigation,
        "search" => Role::Search,
        "section" => {
            let named = attr(el, "aria-label").is_some() || attr(el, "aria-labelledby").is_some();
            if named {
                Role::Region
            } else {
                Role::GenericContainer
            }
        }
        "h1" | "h2" | "h3" | "h4" | "h5" | "h6" => Role::Heading,
        "p" => Role::Paragraph,
        "blockquote" => Role::Blockquote,
        "figure" => Role::Figure,
        "figcaption" => Role::FigureCaption,
        "caption" => Role::Caption,
        "hr" => Role::Splitter,
        "ul" | "ol" | "menu" => Role::List,
        "li" => Role::ListItem,
        "dl" => Role::DescriptionList,
        "dt" => Role::Term,
        "dd" => Role::Definition,
        "dialog" => Role::Dialog,
        "fieldset" => Role::Group,
        "details" => Role::Details,
        "summary" => Role::DisclosureTriangle,
        "form" => Role::Form,
        "table" => Role::Table,
        "thead" | "tbody" | "tfoot" => Role::RowGroup,
        "tr" => Role::Row,
        "td" => Role::Cell,
        "th" => match el.attr(local_name!("scope")) {
            Some("row") | Some("rowgroup") => Role::RowHeader,
            _ => Role::ColumnHeader,
        },
        "a" => match el.attr(local_name!("href")) {
            Some(_) => Role::Link,
            None => Role::GenericContainer,
        },
        "button" => Role::Button,
        "label" | "legend" => Role::Label,
        "select" => match (
            el.attr(local_name!("multiple")),
            el.attr(local_name!("size")),
        ) {
            (Some(_), _) => Role::ListBox,
            (None, Some(size)) if size.parse::<u32>().is_ok_and(|s| s > 1) => Role::ListBox,
            _ => Role::ComboBox,
        },
        "option" => Role::ListBoxOption,
        "optgroup" => Role::Group,
        "textarea" => Role::MultilineTextInput,
        "progress" => Role::ProgressIndicator,
        "meter" => Role::Meter,
        "output" => Role::Status,
        "code" => Role::Code,
        "em" => Role::Emphasis,
        "strong" => Role::Strong,
        "mark" => Role::Mark,
        "time" => Role::Time,
        "img" => match el.attr(local_name!("alt")) {
            Some("") => Role::GenericContainer,
            _ => Role::Image,
        },
        "svg" => {
            if attr(el, "aria-label").is_some() {
                Role::Image
            } else {
                Role::GenericContainer
            }
        }
        "iframe" => Role::Iframe,
        "input" => {
            let ty = el.attr(local_name!("type")).unwrap_or("text");
            match ty {
                "button" | "submit" | "reset" | "image" => Role::Button,
                "checkbox" => Role::CheckBox,
                "color" => Role::ColorWell,
                "date" => Role::DateInput,
                "datetime-local" => Role::DateTimeInput,
                "email" => Role::EmailInput,
                "number" => Role::SpinButton,
                "password" => Role::PasswordInput,
                "radio" => Role::RadioButton,
                "range" => Role::Slider,
                "search" => Role::SearchInput,
                "tel" => Role::PhoneNumberInput,
                "time" => Role::TimeInput,
                "url" => Role::UrlInput,
                "month" => Role::MonthInput,
                "week" => Role::WeekInput,
                "hidden" => return None,
                _ => Role::TextInput,
            }
        }
        _ => return None,
    })
}

/// PATCH(rust-ui): carry out what a screen reader (or another assistive technology) asks for
/// on `doc`: move focus, click (a press and release aimed at the element, so the app's handlers
/// run as for a mouse click, even when something covers it), or replace a text field's value
/// (as if typed: `input` fires). Returns whether the action was carried out. The embedder
/// polls the document afterwards and refreshes the tree.
pub fn perform_accessibility_action(
    doc: &mut dyn crate::Document,
    request: &accesskit::ActionRequest,
) -> bool {
    use accesskit::ActionData;
    use blitz_traits::events::{
        BlitzImeEvent, BlitzMouseButtonEvent, MouseEventButton, MouseEventButtons, UiEvent,
    };
    let Some(node_id) = doc.accessibility_action_target(request.target) else {
        return false;
    };
    match request.action {
        Action::Focus => {
            doc.set_focus_to(node_id);
            true
        }
        Action::Blur => {
            if doc.get_focussed_node_id() == Some(node_id) {
                doc.clear_focus();
            }
            true
        }
        Action::Click => {
            let Some((x, y)) = doc.accessibility_click_point(node_id) else {
                return false;
            };
            let event = |buttons| BlitzMouseButtonEvent {
                x,
                y,
                button: MouseEventButton::Main,
                buttons,
                mods: Default::default(),
            };
            doc.set_next_event_target(Some(node_id));
            doc.handle_ui_event(UiEvent::MouseDown(event(MouseEventButtons::Primary)));
            doc.set_next_event_target(Some(node_id));
            doc.handle_ui_event(UiEvent::MouseUp(event(MouseEventButtons::None)));
            true
        }
        Action::SetValue => {
            let Some(ActionData::Value(value)) = &request.data else {
                return false;
            };
            doc.set_focus_to(node_id);
            if !doc.select_all_in_text_input(node_id) {
                return false;
            }
            doc.handle_ui_event(UiEvent::Ime(BlitzImeEvent::Commit(value.to_string())));
            true
        }
        _ => false,
    }
}
