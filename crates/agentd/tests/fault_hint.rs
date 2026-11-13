//! Provider-fault evidence: agentd keeps the JSON-RPC `error.data` the
//! engine sends and maps an engine-transport failure to a `FaultHint`. The shapes below were
//! measured on the pinned engines (evidence 2026-09-30 engine fault probe): Cline 3.0.65 sends
//! `data: {message}`, OpenCode 1.18.32 sends `data: {service, errorName}`, and the Cline fork
//! patch 0011 adds `statusCode`, `errorName` (the AWS exception type), `isRetryable` and
//! `retryAfterS`.
#![allow(clippy::unwrap_used)]

use agentd::acp::{AcpClient, AcpError};
use agentd::direct::collector::DirectCollector;
use agentd::events::EventRecorder;
use harness_protocol::api::internal::{FaultHint, FaultTransport};
use serde_json::{Value, json};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};

/// An engine that answers its first request with the given JSON-RPC `error` object.
fn engine_answering(error: Value) -> AcpClient<tokio::io::DuplexStream, tokio::io::DuplexStream> {
    let (client_w, engine_r) = tokio::io::duplex(1 << 16);
    let (mut engine_w, client_r) = tokio::io::duplex(1 << 16);
    tokio::spawn(async move {
        let mut lines = BufReader::new(engine_r).lines();
        while let Ok(Some(line)) = lines.next_line().await {
            let msg: Value = serde_json::from_str(&line).unwrap();
            let reply = json!({"jsonrpc": "2.0", "id": msg["id"], "error": error});
            engine_w
                .write_all(format!("{reply}\n").as_bytes())
                .await
                .unwrap();
            engine_w.flush().await.unwrap();
        }
    });
    AcpClient::new(client_r, client_w)
}

async fn rpc_error(error: Value) -> AcpError {
    let mut client = engine_answering(error);
    client
        .request(
            "session/prompt",
            json!({"sessionId": "s-1", "prompt": []}),
            &mut DirectCollector::new(EventRecorder::new("run1", "a1")),
        )
        .await
        .unwrap_err()
}

fn rpc(code: i64, data: Option<Value>) -> AcpError {
    AcpError::Rpc {
        code,
        message: "Internal error: x".into(),
        data,
    }
}

#[tokio::test]
async fn an_rpc_error_keeps_the_engines_error_data() {
    let data = json!({"statusCode": 503, "errorName": "ServiceUnavailableException"});
    let e = rpc_error(json!({"code": -32603, "message": "Internal error: x", "data": data})).await;
    assert_eq!(
        e,
        AcpError::Rpc {
            code: -32603,
            message: "Internal error: x".into(),
            data: Some(data),
        }
    );
}

#[tokio::test]
async fn an_rpc_error_without_data_or_with_null_data_keeps_none() {
    for error in [
        json!({"code": -32603, "message": "Internal error: x"}),
        json!({"code": -32603, "message": "Internal error: x", "data": null}),
    ] {
        assert_eq!(rpc_error(error).await, rpc(-32603, None));
    }
}

#[test]
fn the_error_text_is_unchanged_by_the_data() {
    // The path and the old-agentd text rules (`engine error -32603:`) read this string.
    let e = rpc(-32603, Some(json!({"message": "carried elsewhere"})));
    assert_eq!(e.to_string(), "engine error -32603: Internal error: x");
}

#[test]
fn cline_3_0_65_data_yields_the_transport_only() {
    let e = rpc(
        -32603,
        Some(json!({"message": "The service isn't currently available. Retry your request."})),
    );
    assert_eq!(
        e.fault_hint(),
        Some(FaultHint {
            transport: Some(FaultTransport::Rpc32603),
            ..FaultHint::default()
        })
    );
}

#[test]
fn opencode_data_names_the_error_class_without_claiming_an_aws_type() {
    let e = rpc(
        -32603,
        Some(json!({"service": "session", "errorName": "APIError"})),
    );
    let h = e.fault_hint().unwrap();
    assert_eq!(h.code.as_deref(), Some("APIError"));
    assert_eq!(h.aws_error_type, None, "APIError is not an AWS exception");
    assert_eq!(h.http_status, None);
    assert_eq!(h.transport, Some(FaultTransport::Rpc32603));
}

#[test]
fn patch_0011_data_yields_the_full_hint() {
    let e = rpc(
        -32603,
        Some(json!({
            "message": "The service isn't currently available. Retry your request.",
            "statusCode": 503,
            "errorName": "ServiceUnavailableException",
            "isRetryable": true,
            "retryAfterS": 7,
        })),
    );
    assert_eq!(
        e.fault_hint(),
        Some(FaultHint {
            code: Some("ServiceUnavailableException".into()),
            http_status: Some(503),
            aws_error_type: Some("ServiceUnavailableException".into()),
            retry_after_s: Some(7),
            transport: Some(FaultTransport::Rpc32603),
            silent_s: None,
        })
    );
}

#[test]
fn an_aws_error_type_loses_its_namespace_and_suffix() {
    // `x-amzn-errortype` can arrive as `<namespace>#<Type>:<url>`.
    let e = rpc(
        -32603,
        Some(
            json!({"errorName": "com.amazon.coral.availability#ThrottlingException:http://internal.amazon.com/coral/com.amazon.bedrock/"}),
        ),
    );
    let h = e.fault_hint().unwrap();
    assert_eq!(h.aws_error_type.as_deref(), Some("ThrottlingException"));
    assert_eq!(h.code.as_deref(), Some("ThrottlingException"));
}

#[test]
fn an_explicit_aws_error_type_key_is_taken_as_the_aws_type() {
    let e = rpc(
        -32603,
        Some(json!({"awsErrorType": "ValidationException", "status": "400", "retryAfter": "3"})),
    );
    let h = e.fault_hint().unwrap();
    assert_eq!(h.aws_error_type.as_deref(), Some("ValidationException"));
    assert_eq!(h.http_status, Some(400), "a numeric string status is read");
    assert_eq!(h.retry_after_s, Some(3));
}

#[test]
fn values_that_are_not_safe_facts_are_dropped() {
    let e = rpc(
        -32603,
        Some(json!({
            "statusCode": 99999,
            "errorName": "not a name; it has spaces and a secret token abcdef",
            "retryAfterS": -4,
        })),
    );
    assert_eq!(
        e.fault_hint(),
        Some(FaultHint {
            transport: Some(FaultTransport::Rpc32603),
            ..FaultHint::default()
        })
    );
    let long = "A".repeat(65);
    let e = rpc(-32603, Some(json!({ "errorName": long })));
    assert_eq!(e.fault_hint().unwrap().code, None, "a 65-character name");
    let e = rpc(
        -32603,
        Some(json!({"statusCode": 99, "retryAfterS": "abc"})),
    );
    let h = e.fault_hint().unwrap();
    assert_eq!((h.http_status, h.retry_after_s), (None, None));
}

#[test]
fn the_hint_never_carries_the_engines_message_text() {
    let e = rpc(
        -32603,
        Some(
            json!({"message": "canary-secret-text", "errorName": "ThrottlingException", "detail": "canary-secret-text"}),
        ),
    );
    let text = serde_json::to_string(&e.fault_hint().unwrap()).unwrap();
    assert!(!text.contains("canary-secret-text"), "{text}");
}

#[test]
fn a_float_or_large_retry_after_is_rounded_up_and_saturated() {
    let e = rpc(-32603, Some(json!({"retryAfterS": 2.2})));
    assert_eq!(e.fault_hint().unwrap().retry_after_s, Some(3));
    let e = rpc(-32603, Some(json!({"retryAfterS": 1_u64 << 40})));
    assert_eq!(e.fault_hint().unwrap().retry_after_s, Some(u32::MAX));
}

#[test]
fn only_engine_transport_failures_carry_a_hint() {
    assert_eq!(
        AcpError::Io("broken".into()).fault_hint(),
        Some(FaultHint {
            transport: Some(FaultTransport::Io),
            ..FaultHint::default()
        })
    );
    assert_eq!(
        AcpError::Closed.fault_hint(),
        Some(FaultHint {
            transport: Some(FaultTransport::Closed),
            ..FaultHint::default()
        })
    );
    // Other JSON-RPC codes (auth -32000, invalid params -32602) are not the engine's internal
    // error: they keep the ordinary failed-turn path.
    assert_eq!(rpc(-32000, None).fault_hint(), None);
    assert_eq!(
        rpc(-32602, Some(json!({"statusCode": 503}))).fault_hint(),
        None
    );
    assert_eq!(AcpError::Protocol("x".into()).fault_hint(), None);
    assert_eq!(
        AcpError::ModelMismatch {
            wanted: "a".into(),
            got: None
        }
        .fault_hint(),
        None
    );
    assert_eq!(AcpError::CancelIgnored.fault_hint(), None);
}

// ---- an engine's own idle timeout is a stall (coordinator note 2026-09-30, engine fault probe) -----

fn rpc_msg(message: &str, data: Option<Value>) -> AcpError {
    AcpError::Rpc {
        code: -32603,
        message: message.into(),
        data,
    }
}

#[test]
fn the_engines_own_idle_timeout_is_a_stall() {
    // Cline 3.0.65 ends a request that stayed silent for 300 s with -32603 "The operation timed
    // out." (one request, no retry); OpenCode does the same for a provider `timeout` and for
    // response headers that never came. agentd's 900 s watchdog never sees them, so the hint
    // names them.
    for (message, data) in [
        (
            "Internal error: The operation timed out.",
            json!({"message": "The operation timed out."}),
        ),
        (
            "Internal error: The operation timed out.",
            json!({"service": "session", "errorName": "UnknownError"}),
        ),
        (
            "Internal error: Provider response headers timed out after 300000ms",
            json!({"service": "session", "errorName": "UnknownError"}),
        ),
    ] {
        let h = rpc_msg(message, Some(data)).fault_hint().unwrap();
        assert_eq!(h.code.as_deref(), Some("stall"), "{message}");
        assert_eq!(h.transport, Some(FaultTransport::Rpc32603), "{message}");
        assert_eq!(
            h.silent_s, None,
            "agentd fills the silence from its own clock"
        );
    }
    let h = rpc_msg("The operation timed out", None)
        .fault_hint()
        .unwrap();
    assert_eq!(
        h.code.as_deref(),
        Some("stall"),
        "the final period is optional"
    );
}

#[test]
fn a_timeout_message_is_anchored_and_structured_data_wins() {
    for message in [
        "Internal error: the operation timed out.",
        "Internal error: The operation timed out. Please try again later",
        "Internal error: Request failed: The operation timed out.",
        "Internal error: The operation timed out.\nsecond line",
        "Internal error: Provider response headers timed out after soon",
    ] {
        let h = rpc_msg(message, Some(json!({"message": message})))
            .fault_hint()
            .unwrap();
        assert_eq!(
            h.code, None,
            "{message:?} is not the engine's own timeout text"
        );
    }
    // A structured error type is final, whatever the text says.
    let h = rpc_msg(
        "Internal error: The operation timed out.",
        Some(json!({"statusCode": 429, "errorName": "ThrottlingException"})),
    )
    .fault_hint()
    .unwrap();
    assert_eq!(h.code.as_deref(), Some("ThrottlingException"));
    assert_eq!(h.aws_error_type.as_deref(), Some("ThrottlingException"));
    assert_eq!(h.http_status, Some(429));
    // Only the engine's -32603 carries the text rule.
    let e = AcpError::Rpc {
        code: -32000,
        message: "Internal error: The operation timed out.".into(),
        data: None,
    };
    assert_eq!(e.fault_hint(), None);
}
