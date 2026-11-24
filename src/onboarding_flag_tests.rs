#[test]
fn explicit_endpoint_queries_models_unless_model_is_explicit() {
    assert!(super::query_explicit_models(true, false));
    assert!(!super::query_explicit_models(true, true));
    assert!(!super::query_explicit_models(false, false));
    assert!(!super::query_explicit_models(false, true));
}
