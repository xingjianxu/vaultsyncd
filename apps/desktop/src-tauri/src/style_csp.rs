//! Add a document-specific nonce while preserving Tauri's script hashes and IPC policy.

pub fn with_style_nonce(html: &str, policy: &str, nonce: &str) -> Option<(String, String)> {
    if html.matches("</head>").count() != 1
        || html.contains("name=\"csp-nonce\"")
        || nonce.len() != 32
        || !nonce.bytes().all(|byte| byte.is_ascii_hexdigit())
    {
        return None;
    }
    let mut directives = policy.split(';').map(str::trim).collect::<Vec<_>>();
    let index = directives
        .iter()
        .position(|value| value.split_whitespace().next() == Some("style-src"))?;
    let style = format!("{} 'nonce-{nonce}'", directives[index]);
    directives[index] = &style;
    let meta = format!("<meta name=\"csp-nonce\" content=\"{nonce}\"></head>");
    Some((html.replacen("</head>", &meta, 1), directives.join("; ")))
}

#[cfg(test)]
mod tests {
    use super::with_style_nonce;

    #[test]
    fn preserves_tauri_script_sources_and_only_authorizes_document_styles() {
        let html = "<html><head><script src=\"/main.js\"></script></head><body></body></html>";
        let policy = "default-src 'self'; script-src 'self' 'sha256-example'; style-src 'self'; connect-src ipc: http://ipc.localhost; object-src 'none'";
        let nonce = "0123456789abcdef0123456789abcdef";
        let (document, result) = with_style_nonce(html, policy, nonce).unwrap();
        assert!(document.contains(&format!("name=\"csp-nonce\" content=\"{nonce}\"")));
        assert!(result.contains(&format!("style-src 'self' 'nonce-{nonce}'")));
        assert!(result.contains("script-src 'self' 'sha256-example';"));
        assert!(result.contains("connect-src ipc: http://ipc.localhost; object-src 'none'"));
        assert!(!result.contains("unsafe-inline"));
        assert!(!document.contains("<script nonce="));
        let other = "abcdef0123456789abcdef0123456789";
        assert_ne!(
            with_style_nonce(html, policy, nonce),
            with_style_nonce(html, policy, other)
        );
    }

    #[test]
    fn rejects_malformed_or_already_modified_documents_and_missing_policy() {
        let nonce = "0123456789abcdef0123456789abcdef";
        assert!(with_style_nonce("<head></head>", "default-src 'self'", nonce).is_none());
        assert!(with_style_nonce("<head>", "style-src 'self'", nonce).is_none());
        assert!(with_style_nonce("<head></head></head>", "style-src 'self'", nonce).is_none());
        assert!(with_style_nonce("<head></head>", "style-src 'self'", "bad' nonce").is_none());
        assert!(
            with_style_nonce(
                "<head><meta name=\"csp-nonce\" content=\"old\"></head>",
                "style-src 'self'",
                nonce
            )
            .is_none()
        );
    }
}
