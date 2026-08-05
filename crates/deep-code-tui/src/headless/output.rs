//! Result rendering for headless runs — the `--output-format` surface.
//!
//! Three formats, three consumers:
//! - `text`: humans and shell pipes; stdout carries the answer, nothing else.
//! - `json`: automation that wants one summary object (the bot workflow reads
//!   `.result` / `.reasoning`). The field set is a wire contract: extend it,
//!   never rename or repurpose.
//! - `stream-json`: NDJSON of the same envelopes the SSE server sends, so jq
//!   paths (`.item.kind`, `.item.payload.text`) work unchanged against both.

use std::io::Write;

use deep_code_agent::{CostEstimate, RuntimeEvent, TurnTelemetry, Usage};
use deep_code_runtime::EnvelopeStream;
use serde::Serialize;
use serde_json::Value;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum OutputFormat {
    Text,
    Json,
    StreamJson,
}

impl OutputFormat {
    pub fn parse(value: &str) -> Option<Self> {
        match value {
            "text" => Some(Self::Text),
            "json" => Some(Self::Json),
            "stream-json" => Some(Self::StreamJson),
            _ => None,
        }
    }
}

/// Final machine-readable summary of a headless run. Also emitted as the
/// last `print.result` envelope in `stream-json`, so streaming consumers get
/// the same totals without re-deriving them from deltas.
#[derive(Debug, Serialize)]
pub(crate) struct PrintReport {
    /// `finished` | `cancelled` | `timeout` | `error`.
    pub status: &'static str,
    /// The turn's answer (empty unless `status == "finished"`).
    pub result: String,
    /// Whole-turn reasoning text ("" when the model emitted none).
    pub reasoning: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub session_id: Option<String>,
    /// Gated calls auto-denied by the autonomous posture.
    pub denied_approvals: u32,
    pub duration_ms: u64,
    /// Turn cost, duplicated out of `telemetry` because it is the one number
    /// every consumer wants and the telemetry blob is large.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub cost: Option<CostEstimate>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub usage: Option<Usage>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub telemetry: Option<TurnTelemetry>,
}

pub(crate) fn report_to_json(report: &PrintReport) -> String {
    serde_json::to_string(report).unwrap_or_else(|error| {
        format!(r#"{{"status":"error","error":"report serialization failed: {error}"}}"#)
    })
}

/// NDJSON emitter: one SSE-shaped envelope per line, flushed per line so a
/// piped consumer sees events as they happen rather than on process exit.
pub(crate) struct NdjsonEmitter<W: Write> {
    envelopes: EnvelopeStream,
    out: W,
}

impl<W: Write> NdjsonEmitter<W> {
    pub(crate) fn new(thread_id: String, out: W) -> Self {
        Self {
            envelopes: EnvelopeStream::new(thread_id),
            out,
        }
    }

    pub(crate) fn event(&mut self, event: &RuntimeEvent) {
        let envelope = self.envelopes.event(event);
        self.write_line(serde_json::to_string(&envelope));
    }

    pub(crate) fn manual(&mut self, kind: &str, payload: Value) {
        let envelope = self.envelopes.manual(kind.to_string(), payload);
        self.write_line(serde_json::to_string(&envelope));
    }

    fn write_line(&mut self, line: Result<String, serde_json::Error>) {
        // A broken pipe here means the consumer went away; there is nowhere
        // better to report it, and the drive loop finishes regardless.
        if let Ok(line) = line {
            let _ = writeln!(self.out, "{line}");
            let _ = self.out.flush();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use deep_code_agent::{RuntimeEvent, TurnId};
    use deep_code_runtime::RuntimeEnvelope;

    #[test]
    fn output_format_parses_exact_labels_only() {
        assert_eq!(OutputFormat::parse("text"), Some(OutputFormat::Text));
        assert_eq!(OutputFormat::parse("json"), Some(OutputFormat::Json));
        assert_eq!(
            OutputFormat::parse("stream-json"),
            Some(OutputFormat::StreamJson)
        );
        assert_eq!(OutputFormat::parse("JSON"), None);
        assert_eq!(OutputFormat::parse("ndjson"), None);
    }

    #[test]
    fn ndjson_lines_are_sse_envelopes_with_monotonic_seq() {
        let mut buffer = Vec::new();
        {
            let mut emitter = NdjsonEmitter::new("print_test".to_string(), &mut buffer);
            emitter.manual("user.message", serde_json::json!({ "content": "hi" }));
            emitter.event(&RuntimeEvent::AssistantDelta {
                turn_id: TurnId("turn_test_0".to_string()),
                text: "hello".to_string(),
            });
        }

        let text = String::from_utf8(buffer).unwrap();
        let envelopes: Vec<RuntimeEnvelope> = text
            .lines()
            .map(|line| serde_json::from_str(line).expect("every line is one envelope"))
            .collect();
        assert_eq!(envelopes.len(), 2);
        assert_eq!(envelopes[0].item.kind, "user.message");
        assert_eq!(envelopes[0].seq, 1);
        // Same contract the bot's jq depends on for SSE: `.item.payload.text`.
        assert_eq!(envelopes[1].item.kind, "assistant.delta");
        assert_eq!(envelopes[1].seq, 2);
        assert_eq!(
            envelopes[1]
                .item
                .payload
                .get("text")
                .and_then(Value::as_str),
            Some("hello")
        );
    }

    #[test]
    fn report_serializes_contract_fields_and_omits_absent_ones() {
        let report = PrintReport {
            status: "finished",
            result: "answer".to_string(),
            reasoning: String::new(),
            error: None,
            session_id: Some("session_1_0".to_string()),
            denied_approvals: 2,
            duration_ms: 1234,
            cost: None,
            usage: None,
            telemetry: None,
        };
        let value: Value = serde_json::from_str(&report_to_json(&report)).unwrap();
        assert_eq!(value["status"], "finished");
        assert_eq!(value["result"], "answer");
        assert_eq!(value["reasoning"], "");
        assert_eq!(value["denied_approvals"], 2);
        assert_eq!(value["session_id"], "session_1_0");
        assert!(value.get("error").is_none(), "absent error must be omitted");
        assert!(value.get("cost").is_none());
    }
}
