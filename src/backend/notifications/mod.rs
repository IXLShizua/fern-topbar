//! Desktop notification types and D-Bus client/server transports.

pub mod client;
pub mod server;

/// A private hint lets explicit alert shows resurface retained fern-topbar previews.
/// Other desktop notification servers may ignore it under the standard protocol.
pub const ATTENTION_HINT: &str = "x-fern-topbar-request-attention";

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, PartialOrd, Ord)]
#[repr(u8)]
pub enum Urgency {
    Low = 0,
    #[default]
    Normal = 1,
    Critical = 2,
}

#[derive(Clone, Debug)]
pub struct Notification {
    pub id: u32,
    pub app: String,
    pub icon: String,
    pub desktop_entry: Option<String>,
    pub summary: String,
    pub body: String,
    pub default_action: bool,
    pub resident: bool,
    pub urgency: Urgency,
    /// Resurface a retained preview, even when replacing at the same urgency.
    pub request_attention: bool,
}

#[derive(Debug)]
pub enum Event {
    Added(Notification, i32),
    Closed(u32),
}
