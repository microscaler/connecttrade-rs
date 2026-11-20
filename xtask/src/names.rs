//! Names, docs and references: small helpers shared by collecting and emitting.

use serde_json::Value;
use std::fmt::Write as _;

pub(crate) fn collect_refs(v: &Value, out: &mut Vec<String>) {
    match v {
        Value::Object(m) => {
            if let Some(r) = m.get("$ref").and_then(Value::as_str) {
                out.push(r.rsplit('/').next().unwrap_or(r).to_string());
            }
            for x in m.values() {
                collect_refs(x, out);
            }
        }
        Value::Array(a) => a.iter().for_each(|x| collect_refs(x, out)),
        _ => {}
    }
}

pub(crate) fn doc_of(v: &Value) -> Option<String> {
    v.get("description").and_then(Value::as_str).map(|d| d.trim().to_string()).filter(|d| !d.is_empty())
}

pub(crate) fn doc(s: &mut String, d: Option<&str>, indent: &str) -> std::fmt::Result {
    if let Some(d) = d {
        for line in d.lines() {
            let line = line.trim_end();
            if line.is_empty() {
                writeln!(s, "{indent}///")?;
            } else {
                // keep doc tests out of descriptions: indented code would compile as a doc test
                writeln!(s, "{indent}/// {}", line.trim_start().replace("```", "`"))?;
            }
        }
    }
    Ok(())
}

pub(crate) fn pascal(s: &str) -> String {
    let mut out = String::new();
    let mut up = true;
    for c in s.chars() {
        if c.is_ascii_alphanumeric() {
            if up {
                out.push(c.to_ascii_uppercase());
                up = false;
            } else {
                out.push(c);
            }
        } else {
            up = true;
        }
    }
    out
}

pub(crate) fn variant(v: &str) -> String {
    let v = v.replace('+', "_plus");
    let lower = v.to_ascii_lowercase();
    let p = pascal(&lower);
    if p.chars().next().is_some_and(|c| c.is_ascii_digit()) {
        format!("V{p}")
    } else {
        p
    }
}

pub(crate) fn snake_ident(s: &str) -> String {
    let mut out = String::new();
    for (i, c) in s.chars().enumerate() {
        if c.is_ascii_uppercase() {
            if i > 0 {
                out.push('_');
            }
            out.push(c.to_ascii_lowercase());
        } else if c.is_ascii_alphanumeric() || c == '_' {
            out.push(c);
        } else {
            out.push('_');
        }
    }
    match out.as_str() {
        "type" | "ref" | "match" | "move" | "self" | "use" | "mod" | "fn" | "impl" | "trait" | "where" | "loop"
        | "async" | "await" => format!("r#{out}"),
        _ => out,
    }
}

/// The generated `Method` variant for a spec method (`get`, `post`, ...).
pub(crate) fn method_variant(m: &str) -> Option<&'static str> {
    match m.to_ascii_lowercase().as_str() {
        "get" => Some("Get"),
        "post" => Some("Post"),
        "put" => Some("Put"),
        "patch" => Some("Patch"),
        "delete" => Some("Delete"),
        _ => None,
    }
}
