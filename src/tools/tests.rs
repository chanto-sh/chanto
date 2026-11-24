use super::*;

/// Advertised tool definitions ride along with every request, so their size
/// is a standing cost on every turn. Run with `--nocapture` for the
/// per-tool table; the budget keeps a new tool from quietly adding a couple
/// of hundred tokens to every call.
#[test]
fn advertised_tools_stay_within_a_prompt_budget() {
    let reg = Registry::builtin();
    let mut rows: Vec<(usize, String)> = reg
        .tools
        .iter()
        .map(|t| {
            let spec = crate::llm::ToolSpec {
                name: t.name(),
                description: t.description(),
                parameters: t.schema(),
            };
            let bytes = serde_json::to_string(&spec).map(|s| s.len()).unwrap_or(0);
            (bytes, t.name())
        })
        .collect();
    rows.sort_by_key(|a| std::cmp::Reverse(a.0));
    let total: usize = rows.iter().map(|(bytes, _)| bytes).sum();
    for (bytes, name) in &rows {
        println!("{bytes:6}  {name}");
    }
    println!(
        "{total:6}  total ({} tools, ~{} tokens)",
        rows.len(),
        total.div_ceil(4)
    );
    assert!(
        total <= 22_000,
        "advertised tools take {total} bytes (~{} tokens)",
        total.div_ceil(4)
    );
}
