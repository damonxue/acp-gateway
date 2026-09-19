use serde::Serialize;

/// Redacted status suitable for local health/UI endpoints. It intentionally
/// contains no QR image, bot token, context token or raw upstream payload.
#[derive(Clone, Debug, Default, Serialize)]
pub struct WechatStatus {
    pub enabled: bool,
    pub logged_in: bool,
    pub binding_id: Option<String>,
    pub session_id: Option<String>,
    pub qr_state: Option<String>,
    pub inbox_pending: usize,
    pub outbox_uncertain: usize,
}

impl WechatStatus {
    #[must_use]
    pub fn disconnected() -> Self {
        Self::default()
    }
}
