//! WebView2 reading pane hosted as a child window of the Slint window.

use std::cell::RefCell;

use slint::Window;
use wry::dpi::{LogicalPosition, LogicalSize};
use wry::{Rect, WebView, WebViewBuilder};

const READING_HTML: &str = include_str!("reading.html");

thread_local! {
    static PANE: RefCell<Option<WebView>> = const { RefCell::new(None) };
}

pub struct PaneRect {
    pub x: f32,
    pub y: f32,
    pub width: f32,
    pub height: f32,
}

/// Creates the WebView2 on first use, then keeps its bounds in sync with the
/// Slint placeholder. Bounds are logical pixels; wry converts them with the
/// parent window's DPI.
pub fn sync(window: &Window, rect: PaneRect) -> Result<(), String> {
    let bounds = Rect {
        position: LogicalPosition::new(rect.x as f64, rect.y as f64).into(),
        size: LogicalSize::new(
            f64::from(rect.width.max(1.0)),
            f64::from(rect.height.max(1.0)),
        )
        .into(),
    };

    PANE.with(|pane| {
        let mut pane = pane.borrow_mut();
        if let Some(webview) = pane.as_ref() {
            return webview
                .set_bounds(bounds)
                .map_err(|error| error.to_string());
        }

        let handle = window.window_handle();
        let webview = WebViewBuilder::new()
            .with_html(READING_HTML)
            .with_bounds(bounds)
            .with_javascript_disabled()
            .with_devtools(false)
            .with_background_color((255, 255, 255, 255))
            .build_as_child(&handle)
            .map_err(|error| error.to_string())?;
        *pane = Some(webview);
        Ok(())
    })
}

pub fn close_reading() {
    PANE.with(|pane| {
        *pane.borrow_mut() = None;
    });
}
