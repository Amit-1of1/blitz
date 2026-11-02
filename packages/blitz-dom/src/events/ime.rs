use blitz_traits::events::{BlitzImeEvent, BlitzInputEvent, DomEvent, DomEventData};

use crate::BaseDocument;

// PATCH(rust-ui): IME composition that behaves like a browser's.
//
// - A commit replaces the preedit text (upstream inserted it next to a live preedit when the
//   platform did not clear the preedit first, which doubled the text).
// - Every change of the field's text, preedit included, fires `input`, so a controlled field
//   (`value: "{sig}"`) sees the composed text and does not overwrite it on re-render.
// - `Disabled` while composing keeps nothing, as the platform withdrew the composition.
// - No `println!` on every IME event.
//
// The `composition*` events themselves are fired by the embedder from the `Ime` DOM event (it is
// dispatched to the focused element before this default action runs).
pub(crate) fn handle_ime_event<F: FnMut(DomEvent)>(
    doc: &mut BaseDocument,
    event: BlitzImeEvent,
    mut dispatch_event: F,
) {
    let Some(node_id) = doc.focus_node_id else {
        return;
    };
    let node = &mut doc.nodes[node_id];
    let Some(element) = node.data.downcast_element_mut() else {
        return;
    };
    if element.attr(markup5ever::local_name!("readonly")).is_some()
        || element.attr(markup5ever::local_name!("disabled")).is_some()
    {
        return;
    }
    let Some(input_data) = element.text_input_data_mut() else {
        return;
    };

    let before = input_data.editor.raw_text().to_string();
    {
        let editor = &mut input_data.editor;
        let mut font_ctx = doc.font_ctx.lock().unwrap();
        let mut driver = editor.driver(&mut font_ctx, &mut doc.layout_ctx);

        match &event {
            BlitzImeEvent::Enabled => {}
            BlitzImeEvent::Disabled => driver.clear_compose(),
            BlitzImeEvent::Commit(text) => {
                driver.clear_compose();
                let text = if input_data.is_multiline {
                    text.clone()
                } else {
                    text.replace(['\n', '\r'], " ")
                };
                driver.insert_or_replace_selection(&text);
            }
            BlitzImeEvent::Preedit(text, cursor) => {
                if text.is_empty() {
                    driver.clear_compose();
                } else {
                    // Keep the cursor inside the text and on char boundaries, whatever the
                    // platform sends.
                    let cursor = cursor.map(|(a, b)| {
                        let fix = |i: usize| {
                            let mut i = i.min(text.len());
                            while !text.is_char_boundary(i) {
                                i -= 1;
                            }
                            i
                        };
                        (fix(a), fix(b))
                    });
                    driver.set_compose(text, cursor);
                }
            }
        }
    }

    let value = input_data.editor.raw_text().to_string();
    if value != before {
        dispatch_event(DomEvent::new(
            node_id,
            DomEventData::Input(BlitzInputEvent { value }),
        ));
    }
    doc.shell_provider.request_redraw();
}
