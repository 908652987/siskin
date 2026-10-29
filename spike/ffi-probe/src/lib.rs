//! Minimal UniFFI surface used to measure cross-language call cost.

uniffi::setup_scaffolding!();

#[derive(Debug, Clone, uniffi::Record)]
pub struct Header {
    pub id: u32,
    pub from: String,
    pub subject: String,
    pub snippet: String,
    pub timestamp: i64,
}

fn synth(id: u32) -> Header {
    Header {
        id,
        from: format!("sender{}@mail.test", id % 97),
        subject: format!("Message number {id}"),
        snippet: "This is a synthetic snippet used to measure FFI copying cost.".to_string(),
        timestamp: 1_700_000_000 + id as i64 * 60,
    }
}

/// Returns a page of headers, simulating a store query.
#[uniffi::export]
pub fn page(offset: u32, limit: u32) -> Vec<Header> {
    (offset..offset.saturating_add(limit)).map(synth).collect()
}

/// Round-trip used to measure per-call overhead.
#[uniffi::export]
pub fn echo(text: String) -> String {
    text
}
