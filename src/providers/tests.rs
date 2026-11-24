use super::*;

#[test]
fn catalog_ids_are_unique_and_urls_are_https() {
    let mut ids: Vec<&str> = CATALOG.iter().map(|h| h.id).collect();
    ids.sort();
    ids.dedup();
    assert_eq!(ids.len(), CATALOG.len());
    // Cloud platforms derive their endpoint from region, project or resource.
    assert!(CATALOG.iter().all(|h| h.base_url.starts_with("https://")
        || ((h.kind.is_cloud() || h.id == "chanto") && h.base_url.is_empty())));
    assert!(find("groq").is_some() && find("nope").is_none());
}

#[test]
fn every_provider_has_a_way_to_authenticate() {
    for h in CATALOG {
        assert!(
            !h.env.is_empty() || h.browser_login || h.kind.is_cloud() || h.id == "chanto",
            "{} has no key variable, login or cloud credentials",
            h.id
        );
    }
}

#[test]
fn coding_plans_use_their_own_openai_endpoints() {
    for id in [
        "zai",
        "kimi-coding",
        "kimi-coding-cn",
        "qwen-coding",
        "qwen-coding-cn",
        "tencent-coding",
        "volcengine-coding",
        "stepfun-plan",
        "stepfun-plan-cn",
        "cline",
    ] {
        let h = find(id).unwrap_or_else(|| panic!("{id} is in the catalog"));
        assert_eq!(h.kind, OAI, "{id} should be OpenAI-compatible");
        assert!(
            h.base_url.starts_with("https://"),
            "{id} needs an https endpoint"
        );
        assert!(!h.env.is_empty(), "{id} needs its own key variable");
    }
}
