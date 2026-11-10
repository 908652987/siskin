//! Non-Windows placeholder. The S3 spike only measures the WebView2 path.

pub struct PaneRect {
    pub x: f32,
    pub y: f32,
    pub width: f32,
    pub height: f32,
}

pub fn sync(_window: &slint::Window, _rect: PaneRect) -> Result<(), String> {
    Err("the HTML reading pane is Windows-only in this spike".to_string())
}

pub fn close_reading() {}
