use serde::{Deserialize, Serialize};
use specta::Type;
use tauri::AppHandle;
use tauri_specta::Event;

use crate::api_types::AppEvent;

#[derive(Clone, Debug, Deserialize, Serialize, Type)]
#[serde(transparent)]
pub struct AppEventEnvelope(pub AppEvent);

impl Event for AppEventEnvelope {
    const NAME: &'static str = "app://event";
}

pub trait EventSink: Send + Sync + 'static {
    fn emit(&self, event: AppEvent);
}

#[derive(Clone)]
pub struct TauriEventSink {
    app: AppHandle,
}

impl TauriEventSink {
    #[must_use]
    pub fn new(app: AppHandle) -> Self {
        Self { app }
    }
}

impl EventSink for TauriEventSink {
    fn emit(&self, event: AppEvent) {
        let _ = AppEventEnvelope(event).emit(&self.app);
    }
}
