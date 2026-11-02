use blitz_traits::{
    events::{
        BlitzInputEvent, BlitzMouseButtonEvent, DomEvent, DomEventData, MouseEventButton,
        MouseEventButtons,
    },
    navigation::NavigationOptions,
};
use markup5ever::local_name;

use crate::{BaseDocument, node::SpecialElementData};

pub(crate) fn handle_mousemove(
    doc: &mut BaseDocument,
    target: usize,
    x: f32,
    y: f32,
    buttons: MouseEventButtons,
) -> bool {
    let mut changed = doc.set_hover_to(x, y);

    // PATCH(rust-ui): a drag that started in a text field keeps selecting in that field when the
    // pointer leaves it (upstream stopped as soon as the pointer was over another node), and a
    // drag that started on page text extends the page selection.
    if !buttons.contains(MouseEventButtons::Primary) {
        doc.text_input_drag = None;
        doc.end_text_selection_drag();
        return changed;
    }
    if let Some(field) = doc.text_input_drag {
        return extend_field_selection(doc, field, x, y) || changed;
    }
    if doc.extend_text_selection(x, y) {
        changed = true;
    }
    let _ = target;
    changed
}

/// Field-local, layout-scaled coordinates of the document point `(x, y)`.
fn field_coords(doc: &BaseDocument, field: usize, x: f32, y: f32) -> (f32, f32) {
    let node = &doc.nodes[field];
    let origin = node.absolute_position(0.0, 0.0);
    let l = &node.final_layout;
    let scale = doc.viewport.scale_f64();
    (
        ((x - origin.x - l.padding.left - l.border.left) as f64 * scale) as f32,
        ((y - origin.y - l.padding.top - l.border.top) as f64 * scale) as f32,
    )
}

fn extend_field_selection(doc: &mut BaseDocument, field: usize, x: f32, y: f32) -> bool {
    let (fx, fy) = field_coords(doc, field, x, y);
    let node = &mut doc.nodes[field];
    let Some(input) = node
        .data
        .downcast_element_mut()
        .and_then(|el| el.text_input_data_mut())
    else {
        doc.text_input_drag = None;
        return false;
    };
    if input.editor.is_composing() {
        return false;
    }
    input
        .editor
        .driver(&mut doc.font_ctx.lock().unwrap(), &mut doc.layout_ctx)
        .extend_selection_to_point(fx, fy);
    true
}

pub(crate) fn handle_mousedown(
    doc: &mut BaseDocument,
    target: usize,
    event: &BlitzMouseButtonEvent,
) {
    let (x, y) = (event.x, event.y);
    // PATCH(rust-ui): a click focuses the nearest focusable ancestor (or blurs when there is
    // none), as browsers do; upstream only focused text inputs.
    match doc.focus_target_for_click(target) {
        Some(id) => {
            doc.set_focus_to(id);
        }
        None => doc.clear_focus(),
    }

    if event.button != MouseEventButton::Main {
        return;
    }
    // PATCH(rust-ui): count presses close in time and place, for double and triple click.
    let clicks = doc.count_press(x, y);
    let shift = event.mods.contains(keyboard_types::Modifiers::SHIFT);

    let field = doc.nodes[target]
        .element_data()
        .is_some_and(|el| el.text_input_data().is_some())
        .then_some(target);
    let Some(field) = field else {
        // Page text: start (or clear) a selection.
        doc.text_input_drag = None;
        doc.start_text_selection(target, x, y, clicks, shift);
        return;
    };

    let node = &mut doc.nodes[field];
    let Some(el) = node.data.downcast_element_mut() else {
        return;
    };
    if el.attr(local_name!("disabled")).is_some() {
        return;
    }
    doc.clear_text_selection();

    let (fx, fy) = field_coords(doc, field, x, y);
    let node = &mut doc.nodes[field];
    let Some(input) = node
        .data
        .downcast_element_mut()
        .and_then(|el| el.text_input_data_mut())
    else {
        return;
    };
    if input.editor.is_composing() {
        // A press while composing commits the composition, as browsers do.
        input
            .editor
            .driver(&mut doc.font_ctx.lock().unwrap(), &mut doc.layout_ctx)
            .finish_compose();
    }
    let mut font_ctx = doc.font_ctx.lock().unwrap();
    let mut driver = input.editor.driver(&mut font_ctx, &mut doc.layout_ctx);
    // PATCH(rust-ui): Shift+click extends, a double click selects a word and a triple click
    // the line, as in browsers; upstream only placed the caret.
    match (shift, clicks) {
        (true, _) => driver.shift_click_extension(fx, fy),
        (false, 2) => driver.select_word_at_point(fx, fy),
        (false, n) if n >= 3 => driver.select_hard_line_at_point(fx, fy),
        _ => driver.move_to_point(fx, fy),
    }
    drop(font_ctx);
    doc.text_input_drag = Some(field);
    doc.set_focus_to(field);
}

pub(crate) fn handle_mouseup<F: FnMut(DomEvent)>(
    doc: &mut BaseDocument,
    target: usize,
    event: &BlitzMouseButtonEvent,
    mut dispatch_event: F,
) {
    // PATCH(rust-ui): the drag that selects text ends with the button.
    if event.button == MouseEventButton::Main {
        doc.text_input_drag = None;
        doc.end_text_selection_drag();
    }

    if doc.devtools().highlight_hover {
        let mut node = doc.get_node(target).unwrap();
        if event.button == MouseEventButton::Secondary {
            if let Some(parent_id) = node.layout_parent.get() {
                node = doc.get_node(parent_id).unwrap();
            }
        }
        doc.debug_log_node(node.id);
        doc.devtools_mut().highlight_hover = false;
        return;
    }

    // Determine whether to dispatch a click event
    let do_click = true;
    // let do_click = doc.mouse_down_node.is_some_and(|mouse_down_id| {
    //     // Anonymous node ids are unstable due to tree reconstruction. So we compare the id
    //     // of the first non-anonymous ancestor.
    //     mouse_down_id == target
    //         || doc.non_anon_ancestor_if_anon(mouse_down_id) == doc.non_anon_ancestor_if_anon(target)
    // });

    // Dispatch a click event
    if do_click && event.button == MouseEventButton::Main {
        dispatch_event(DomEvent::new(target, DomEventData::Click(event.clone())));
    }
}

pub(crate) fn handle_click<F: FnMut(DomEvent)>(
    doc: &mut BaseDocument,
    target: usize,
    event: &BlitzMouseButtonEvent,
    mut dispatch_event: F,
) {
    let mut maybe_node_id = Some(target);
    while let Some(node_id) = maybe_node_id {
        let maybe_element = {
            let node = &mut doc.nodes[node_id];
            node.data.downcast_element_mut()
        };

        let Some(el) = maybe_element else {
            maybe_node_id = doc.nodes[node_id].parent;
            continue;
        };

        let disabled = el.attr(local_name!("disabled")).is_some();
        if disabled {
            return;
        }

        if let SpecialElementData::TextInput(_) = el.special_data {
            return;
        }

        match el.name.local {
            local_name!("input") if el.attr(local_name!("type")) == Some("checkbox") => {
                let is_checked = BaseDocument::toggle_checkbox(el);
                let value = is_checked.to_string();
                dispatch_event(DomEvent::new(
                    node_id,
                    DomEventData::Input(BlitzInputEvent { value }),
                ));
                doc.set_focus_to(node_id);
                return;
            }
            local_name!("input") if el.attr(local_name!("type")) == Some("radio") => {
                let radio_set = el.attr(local_name!("name")).unwrap().to_string();
                BaseDocument::toggle_radio(doc, radio_set, node_id);

                // TODO: make input event conditional on value actually changing
                let value = String::from("true");
                dispatch_event(DomEvent::new(
                    node_id,
                    DomEventData::Input(BlitzInputEvent { value }),
                ));

                BaseDocument::set_focus_to(doc, node_id);

                return;
            }
            // Clicking labels triggers click, and possibly input event, of associated input
            local_name!("label") => {
                if let Some(target_node_id) = doc.label_bound_input_element(node_id).map(|n| n.id) {
                    // Apply default click event action for target node
                    let target_node = doc.get_node_mut(target_node_id).unwrap();
                    let syn_event = target_node.synthetic_click_event_data(event.mods);
                    handle_click(doc, target_node_id, &syn_event, dispatch_event);
                    return;
                }
            }
            local_name!("a") => {
                if let Some(href) = el.attr(local_name!("href")) {
                    if let Some(url) = doc.url.resolve_relative(href) {
                        doc.navigation_provider.navigate_to(NavigationOptions::new(
                            url,
                            String::from("text/plain"),
                            doc.id(),
                        ));
                    } else {
                        println!("{href} is not parseable as a url. : {:?}", *doc.url)
                    }
                    return;
                } else {
                    println!("Clicked link without href: {:?}", el.attrs());
                }
            }
            local_name!("input")
                if el.is_submit_button() || el.attr(local_name!("type")) == Some("submit") =>
            {
                if let Some(form_owner) = doc.controls_to_form.get(&node_id) {
                    doc.submit_form(*form_owner, node_id);
                }
            }
            #[cfg(feature = "file_input")]
            local_name!("input") if el.attr(local_name!("type")) == Some("file") => {
                use crate::qual_name;
                //TODO: Handle accept attribute https://developer.mozilla.org/en-US/docs/Web/HTML/Reference/Attributes/accept by passing an appropriate filter
                let multiple = el.attr(local_name!("multiple")).is_some();
                let files = doc.shell_provider.open_file_dialog(multiple, None);

                if let Some(file) = files.first() {
                    el.attrs
                        .set(qual_name!("value", html), &file.to_string_lossy());
                }
                let text_content = match files.len() {
                    0 => "No Files Selected".to_string(),
                    1 => files
                        .first()
                        .unwrap()
                        .file_name()
                        .unwrap_or_default()
                        .to_string_lossy()
                        .to_string(),
                    x => format!("{x} Files Selected"),
                };

                if files.is_empty() {
                    el.special_data = SpecialElementData::None;
                } else {
                    el.special_data = SpecialElementData::FileInput(files.into())
                }
                let child_label_id = doc.nodes[node_id].children[1];
                let child_text_id = doc.nodes[child_label_id].children[0];
                let text_data = doc.nodes[child_text_id]
                    .text_data_mut()
                    .expect("Text data not found");
                text_data.content = text_content;
            }
            _ => {}
        }

        // No match. Recurse up to parent.
        maybe_node_id = doc.nodes[node_id].parent;
    }

    // PATCH(rust-ui): focus is now set (or cleared) on mousedown, like browsers do; upstream
    // cleared it here, after click handlers had run, which blurred focused buttons.
}
