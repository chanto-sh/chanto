use super::*;

#[test]
fn poll_result_is_tagged_by_status() {
    let pending = serde_json::to_string(&DevicePollResult::Pending { interval: 5 }).unwrap();
    assert_eq!(pending, r#"{"status":"pending","interval":5}"#);
}

#[test]
fn known_and_unknown_error_codes_parse() {
    let e: ApiError =
        serde_json::from_str(r#"{"error":"rate_limited","message":"slow down"}"#).unwrap();
    assert_eq!(e.error, ErrorCode::RateLimited);
    let e: ApiError =
        serde_json::from_str(r#"{"error":"a_future_code","message":"who knows"}"#).unwrap();
    assert_eq!(e.error, ErrorCode::Unknown);
}
