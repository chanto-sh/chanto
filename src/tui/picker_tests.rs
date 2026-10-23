use super::PickerSearch;

#[test]
fn resume_search_filters_by_title_and_first_prompt() {
    let labels = vec![
        "10:00 · Auth refactor".to_string(),
        "11:00 · write the README".to_string(),
    ];
    let hay = vec![
        "Auth refactor\nmove login to OAuth tokens\nid-1".to_string(),
        "\nwrite the README\nid-2".to_string(),
    ];
    let mut s = PickerSearch::new(&labels, hay, true);
    s.query = "oauth LOGIN".into();
    s.refilter();
    assert_eq!(s.indices, vec![0]);
    assert!(s.heading().contains("Search: oauth LOGIN") && s.heading().contains("ctrl+d delete"));
    s.query = "readme".into();
    s.refilter();
    assert_eq!(s.indices, vec![1]);
    s.query = "nothing".into();
    s.refilter();
    assert!(s.indices.is_empty() && s.heading().contains("No session"));
    s.query.clear();
    s.refilter();
    assert_eq!(s.indices, vec![0, 1]);
}
