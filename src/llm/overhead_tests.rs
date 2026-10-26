use super::*;

#[test]
fn line_buffer_preserves_bytes_at_every_chunk_boundary() {
    let input = b"\nfirst\r\n\xe2\x98\x83\n\0last";
    for size in 1..=input.len() + 1 {
        let mut lines = LineBuf::default();
        let mut got = Vec::new();
        for chunk in input.chunks(size) {
            lines.push(chunk);
            while let Some(line) = lines.next_line() {
                got.push(line.to_vec());
            }
        }
        assert_eq!(
            got,
            [
                b"\n".to_vec(),
                b"first\r\n".to_vec(),
                b"\xe2\x98\x83\n".to_vec()
            ]
        );
        assert_eq!(lines.rest(), b"\0last");
        assert!(lines.rest().is_empty());
        lines.push(b"again\n");
        assert_eq!(lines.next_line(), Some(&b"again\n"[..]));
        assert!(lines.next_line().is_none());
        assert!(lines.rest().is_empty());
    }
}

#[test]
fn line_buffer_accepts_more_bytes_before_draining_the_previous_chunk() {
    let mut lines = LineBuf::default();
    lines.push(b"a\nb\npar");
    assert_eq!(lines.next_line(), Some(&b"a\n"[..]));
    lines.push(b"t\nz");
    assert_eq!(lines.next_line(), Some(&b"b\n"[..]));
    assert_eq!(lines.next_line(), Some(&b"part\n"[..]));
    assert_eq!(lines.rest(), b"z");
}

#[tokio::test]
async fn buffered_request_retries_preserve_signed_bytes_and_headers() {
    let (base, seen) = mock::serve(vec![
        (429, "{}".into()),
        (503, "{}".into()),
        (200, "{}".into()),
    ])
    .await;
    let provider = Provider::new(ProviderKind::OpenaiResponses, &base, None, 8192);
    // Preserve the exact serialized representation used by request signing.
    let body = format!("{{ \"text\": \"{}\" }}", "x".repeat(256 * 1024)).into_bytes();
    provider
        .post_bytes_retrying(
            &base,
            body.clone(),
            &[("x-test-signature".into(), "signed-body".into())],
        )
        .await
        .unwrap();
    let seen = seen.lock().unwrap();
    assert_eq!(seen.raw.len(), 3);
    for (raw, headers) in seen.raw.iter().zip(&seen.headers) {
        assert_eq!(raw, &body);
        assert!(
            headers
                .to_ascii_lowercase()
                .contains("x-test-signature: signed-body")
        );
    }
}
