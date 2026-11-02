use super::*;

#[test]
fn names() {
    assert!(valid_agent_name("triage") && valid_agent_name("deps-weekly2"));
    assert!(
        !valid_agent_name("")
            && !valid_agent_name("Triage")
            && !valid_agent_name("2x")
            && !valid_agent_name("a_b")
            && !valid_agent_name("a-")
    );
}

#[test]
fn spec_wire_format() {
    let spec: AgentSpec = serde_json::from_value(serde_json::json!({
        "name": "triage", "instructions": "Label new issues.", "repo": "https://github.com/acme/api.git",
        "schedule": "0 9 * * 1-5", "triggers": [{"on": "issue_opened"}, {"on": "issue_labeled", "label": "bug"}]
    }))
    .unwrap();
    assert!(spec.enabled && spec.placement.is_none());
    assert_eq!(
        spec.triggers,
        vec![
            AgentTrigger::IssueOpened,
            AgentTrigger::IssueLabeled {
                label: Some("bug".into())
            }
        ]
    );
    assert_eq!(
        serde_json::to_value(&spec.triggers[0]).unwrap(),
        serde_json::json!({"on": "issue_opened"})
    );
}
