#[cfg(windows)]
mod windows;

#[cfg(windows)]
pub use windows::{close_reading, sync, PaneRect};

#[cfg(not(windows))]
mod stub;

#[cfg(not(windows))]
pub use stub::{close_reading, sync, PaneRect};
