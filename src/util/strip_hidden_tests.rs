use super::strip_hidden;

#[test]
fn hidden_characters_go_and_real_writing_stays() {
    assert_eq!(
        strip_hidden("a\u{E0069}\u{E0067}b\u{202E}c\u{2066}d"),
        "abcd"
    );
    // England's flag, a family emoji, and Persian with a joiner and a mark.
    for real in [
        "\u{1F3F4}\u{E0067}\u{E0062}\u{E0065}\u{E006E}\u{E0067}\u{E007F}",
        "\u{1F468}\u{200D}\u{1F469}",
        "\u{0645}\u{200C}\u{0627}\u{200F}",
        "plain",
    ] {
        assert_eq!(strip_hidden(real), real);
    }
}
