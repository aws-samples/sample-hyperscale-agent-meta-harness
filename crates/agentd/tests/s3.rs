//! S3 error reports: the status plus S3's own `Code` and `Message`, never the rest of the error
//! document. A bare "returned 403" hid a KMS forward-access denial that only CloudTrail showed.
#![allow(clippy::unwrap_used)]

use agentd::s3::s3_error;

const URI: &str = "s3://hamh-live-harness-800000000022/runs/R1/input/repo.bundle";

#[test]
fn an_s3_error_document_adds_its_code_and_message() {
    let body = "<?xml version=\"1.0\" encoding=\"UTF-8\"?>\n<Error><Code>AccessDenied</Code><Message>User: arn:aws:sts::800000000022:assumed-role/hamh-live-harness-worker-session/R1.a1 is not authorized to perform: kms:Decrypt</Message><RequestId>ABC</RequestId></Error>";
    let e = s3_error("GET", URI, 403, body.as_bytes());
    assert_eq!(
        e,
        format!(
            "s3 GET {URI} returned 403 AccessDenied: User: arn:aws:sts::800000000022:assumed-role/hamh-live-harness-worker-session/R1.a1 is not authorized to perform: kms:Decrypt"
        )
    );
}

#[test]
fn a_body_without_an_error_document_reports_the_status_only() {
    assert_eq!(
        s3_error("GET", URI, 404, b"NoSuchKey"),
        format!("s3 GET {URI} returned 404")
    );
    assert_eq!(
        s3_error("PUT", URI, 500, b""),
        format!("s3 PUT {URI} returned 500")
    );
}

#[test]
fn signature_details_never_reach_the_report() {
    let body = "<Error><Code>SignatureDoesNotMatch</Code><Message>The request signature we calculated does not match the signature you provided.</Message><AWSAccessKeyId>ASIAEXAMPLE</AWSAccessKeyId><StringToSign>AWS4-HMAC-SHA256\n20260925T000000Z</StringToSign><SignatureProvided>abc123</SignatureProvided><CanonicalRequest>GET /runs</CanonicalRequest></Error>";
    let e = s3_error("GET", URI, 403, body.as_bytes());
    assert!(
        e.contains("403 SignatureDoesNotMatch: The request signature"),
        "{e}"
    );
    for hidden in [
        "ASIAEXAMPLE",
        "AWS4-HMAC-SHA256",
        "abc123",
        "CanonicalRequest",
        "GET /runs",
    ] {
        assert!(!e.contains(hidden), "{hidden} in {e}");
    }
}

#[test]
fn a_long_message_is_cut_at_300_characters() {
    let body = format!(
        "<Error><Code>X</Code><Message>{}</Message></Error>",
        "m".repeat(1000)
    );
    let e = s3_error("GET", URI, 400, body.as_bytes());
    let msg = e.split_once("400 X: ").unwrap().1;
    assert_eq!(msg.chars().count(), 300);
}
