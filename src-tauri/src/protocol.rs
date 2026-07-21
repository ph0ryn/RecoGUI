use serde::{Deserialize, Serialize};
use serde_json::Value;

pub const PROTOCOL_VERSION: u32 = 1;
pub const MAX_LINE_BYTES: usize = 8 * 1024 * 1024;

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct RequestEnvelope {
    pub protocol_version: u32,
    #[serde(rename = "type")]
    pub message_type: &'static str,
    pub request_id: String,
    pub session_id: Option<String>,
    pub sequence: u64,
    pub command: String,
    pub payload: Value,
}

impl RequestEnvelope {
    pub fn new(
        request_id: String,
        session_id: Option<String>,
        sequence: u64,
        command: impl Into<String>,
        payload: Value,
    ) -> Self {
        Self {
            protocol_version: PROTOCOL_VERSION,
            message_type: "request",
            request_id,
            session_id,
            sequence,
            command: command.into(),
            payload,
        }
    }
}

#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct IncomingEnvelope {
    pub protocol_version: u32,
    #[serde(rename = "type")]
    pub message_type: IncomingType,
    pub request_id: Option<String>,
    pub session_id: Option<String>,
    pub sequence: u64,
    #[serde(default)]
    pub event: Option<String>,
    #[serde(default)]
    pub ok: Option<bool>,
    #[serde(default)]
    pub payload: Value,
    #[serde(default)]
    pub error: Option<ProtocolError>,
}

#[derive(Debug, Clone, Copy, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub enum IncomingType {
    Response,
    Event,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ProtocolError {
    pub code: String,
    pub message: String,
    pub recoverable: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub details: Option<Value>,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct HostEngineEvent {
    pub event: String,
    pub session_id: Option<String>,
    pub sequence: u64,
    pub payload: Value,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct EngineStateSnapshot {
    pub state: EngineConnectionState,
    pub pid: Option<u32>,
    pub last_heartbeat_age_ms: Option<u64>,
}

#[derive(Debug, Clone, Copy, Serialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub enum EngineConnectionState {
    Offline,
    Starting,
    Ready,
    Unresponsive,
    RestartLimitReached,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn request_envelope_uses_wire_names() {
        let value = serde_json::to_value(RequestEnvelope::new(
            "request-1".into(),
            None,
            7,
            "engine.getState",
            serde_json::json!({}),
        ))
        .unwrap();

        assert_eq!(value["protocolVersion"], 1);
        assert_eq!(value["type"], "request");
        assert_eq!(value["requestId"], "request-1");
        assert_eq!(value["sequence"], 7);
    }

    #[test]
    fn incoming_event_deserializes() {
        let envelope: IncomingEnvelope = serde_json::from_value(serde_json::json!({
            "protocolVersion": 1,
            "type": "event",
            "requestId": null,
            "sessionId": "session-1",
            "sequence": 2,
            "event": "segment.persisted",
            "payload": {"text": "hello"}
        }))
        .unwrap();

        assert_eq!(envelope.message_type, IncomingType::Event);
        assert_eq!(envelope.event.as_deref(), Some("segment.persisted"));
        assert_eq!(envelope.sequence, 2);
    }

    #[test]
    fn shared_response_and_event_fixtures_deserialize() {
        let response: IncomingEnvelope = serde_json::from_str(include_str!(
            "../../protocol/fixtures/response.engine-get-state.json"
        ))
        .unwrap();
        let event: IncomingEnvelope = serde_json::from_str(include_str!(
            "../../protocol/fixtures/event.segment-persisted.json"
        ))
        .unwrap();
        let request: serde_json::Value = serde_json::from_str(include_str!(
            "../../protocol/fixtures/request.engine-get-state.json"
        ))
        .unwrap();
        let model_list: IncomingEnvelope = serde_json::from_str(include_str!(
            "../../protocol/fixtures/response.model-list.json"
        ))
        .unwrap();
        let model_select: serde_json::Value = serde_json::from_str(include_str!(
            "../../protocol/fixtures/request.model-select.json"
        ))
        .unwrap();

        assert_eq!(response.message_type, IncomingType::Response);
        assert_eq!(event.message_type, IncomingType::Event);
        assert_eq!(event.sequence, 2);
        assert_eq!(event.payload["rowVersion"], 3);
        assert_eq!(event.payload["totalSegments"], 1);
        assert_eq!(request["command"], "engine.getState");
        assert_eq!(request["protocolVersion"], PROTOCOL_VERSION);
        assert_eq!(model_list.payload["models"][0]["size"], "2.5G");
        assert_eq!(model_select["command"], "model.select");
    }
}
