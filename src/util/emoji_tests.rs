#[test]
fn replaces_known_shortcodes_only() {
    use super::emoji_shortcodes as e;
    assert_eq!(e("shipped it :tada: :+1:"), "shipped it 🎉 👍");
    assert_eq!(e("std::io::Write and a:b:c"), "std::io::Write and a:b:c");
    assert_eq!(e(":notanemoji: stays"), ":notanemoji: stays");
    assert_eq!(e("time 10:30:45"), "time 10:30:45");
    assert_eq!(e(":fire::rocket:"), "🔥🚀");
}
