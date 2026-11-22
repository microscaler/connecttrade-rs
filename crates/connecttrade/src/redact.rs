//! What an error may keep of an answer (#17): never a credential, never more than [`KEEP`] bytes.

/// The most of an answer an error keeps.
pub(crate) const KEEP: usize = 8 * 1024;

/// JSON keys whose string values are credentials.
const SECRET_KEYS: [&str; 6] = ["user_secret", "client_secret", "access_token", "token", "refresh_token", "secret"];

/// Operations whose answers carry a credential: a body that is not JSON is kept only as its size.
const SECRET_OPERATIONS: [&str; 4] =
    ["create_user", "rotate_user_secret", "create_platform_token", "create_user_token"];

/// An answer as an error keeps it, and the secret values it held (to scrub from messages derived from it).
pub(crate) struct Kept {
    pub text: String,
    pub secrets: Vec<String>,
}

/// `body` with every credential replaced by `<redacted>`, capped.
pub(crate) fn body(operation: &str, body: &[u8]) -> Kept {
    match serde_json::from_slice::<serde_json::Value>(body) {
        Ok(mut v) => {
            let mut secrets = Vec::new();
            redact_value(&mut v, &mut secrets);
            Kept { text: cap(&v.to_string()), secrets }
        }
        Err(_) if SECRET_OPERATIONS.contains(&operation) => {
            Kept { text: format!("<{} bytes, not JSON>", body.len()), secrets: Vec::new() }
        }
        Err(_) => Kept { text: cap(&String::from_utf8_lossy(body)), secrets: Vec::new() },
    }
}

fn redact_value(v: &mut serde_json::Value, secrets: &mut Vec<String>) {
    match v {
        serde_json::Value::Object(map) => {
            for (k, val) in map.iter_mut() {
                if SECRET_KEYS.iter().any(|s| k.eq_ignore_ascii_case(s)) {
                    if let serde_json::Value::String(text) = val {
                        if !text.is_empty() {
                            secrets.push(std::mem::take(text));
                        }
                        *val = serde_json::Value::String("<redacted>".into());
                    }
                } else {
                    redact_value(val, secrets);
                }
            }
        }
        serde_json::Value::Array(items) => items.iter_mut().for_each(|i| redact_value(i, secrets)),
        _ => {}
    }
}

/// `text` with every one of `secrets` replaced by `<redacted>`.
pub(crate) fn scrub(text: &str, secrets: &[String]) -> String {
    secrets.iter().fold(text.to_string(), |t, s| t.replace(s.as_str(), "<redacted>"))
}

fn cap(text: &str) -> String {
    if text.len() <= KEEP {
        return text.to_string();
    }
    let mut end = KEEP;
    while !text.is_char_boundary(end) {
        end -= 1;
    }
    format!("{}… ({} bytes)", text.get(..end).unwrap_or_default(), text.len())
}

/// `text` on one line, for `Display` (#22): control characters escaped, at most [`LINE`] characters.
pub(crate) fn one_line(text: &str) -> String {
    let mut out = String::with_capacity(text.len().min(LINE + 16));
    for (n, c) in text.chars().enumerate() {
        if n >= LINE {
            out.push('…');
            break;
        }
        match c {
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            c if c.is_control() => out.extend(c.escape_unicode()),
            c => out.push(c),
        }
    }
    out
}

/// The longest message `Display` shows.
pub(crate) const LINE: usize = 512;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn secrets_are_redacted_at_any_depth_and_scrubbed_from_messages() {
        let k = body("create_user", br#"{"user_id":"u","user_secret":"s3","nested":[{"access_token":"t0"}]}"#);
        assert!(!k.text.contains("s3") && !k.text.contains("t0"), "{}", k.text);
        assert!(k.text.contains("<redacted>") && k.text.contains("\"u\""));
        assert_eq!(scrub("value \"s3\" is bad", &k.secrets), "value \"<redacted>\" is bad");
        assert_eq!(body("create_user", b"user_secret=s3").text, "<14 bytes, not JSON>");
        assert_eq!(body("get_order", b"<html>bad gateway</html>").text, "<html>bad gateway</html>");
        assert!(body("get_order", &vec![b'x'; KEEP * 2]).text.ends_with(&format!("({} bytes)", KEEP * 2)));
    }

    #[test]
    fn one_line_escapes_controls_and_caps() {
        assert_eq!(one_line("a\nb\r\tc\u{7}"), "a\\nb\\r\\tc\\u{7}");
        assert_eq!(one_line(&"x".repeat(LINE + 10)).chars().count(), LINE + 1);
    }
}
