use crate::event::BlitzShellEvent;
use accesskit_winit::Adapter;
use blitz_dom::BaseDocument;
use winit::{event_loop::EventLoopProxy, window::Window};

/// State of the accessibility node tree and platform adapter.
pub struct AccessibilityState {
    /// Adapter to connect to the [`EventLoop`](`winit::event_loop::EventLoop`).
    adapter: accesskit_winit::Adapter,
}

impl AccessibilityState {
    pub fn new(window: &Window, proxy: EventLoopProxy<BlitzShellEvent>) -> Self {
        Self {
            adapter: Adapter::with_event_loop_proxy(window, proxy.clone()),
        }
    }
    pub fn update_tree(&mut self, doc: &BaseDocument) {
        self.adapter
            .update_if_active(|| doc.build_accessibility_tree());
    }

    /// PATCH(rust-ui): tell the adapter about window focus, position and size. AccessKit needs
    /// this for every window event, before the app handles it; upstream 0.2 never called it, so
    /// screen readers on Windows and macOS did not follow focus into the window.
    pub fn process_window_event(&mut self, window: &Window, event: &winit::event::WindowEvent) {
        self.adapter.process_event(window, event);
    }
}
