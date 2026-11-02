use super::*;

#[test]
fn sse_blocks_split_across_chunks() {
    let mut p = SseParser { buf: String::new() };
    assert!(p.push(b"data: {\"type\":\"te").is_empty());
    let v = p.push(b"xt\",\"text\":\"hi\"}\n\n: keep-alive\n\ndata: {\"seq\":2}\n\n");
    assert_eq!(v.len(), 2);
    assert_eq!(v[0]["text"], "hi");
    assert_eq!(v[1]["seq"], 2);
}
