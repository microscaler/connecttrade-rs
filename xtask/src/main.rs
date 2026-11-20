//! `cargo xtask codegen [--check]`: generate `crates/connecttrade/src/generated/{models,operations}.rs` from
//! Connect Trade's spec (`spec/openapi.json`, as published) and our overlay (`spec/overlay.json`).
//!
//! The generated files are checked in and never edited by hand. `--check` regenerates in memory and fails when the
//! files on disk differ, which CI runs so a spec or overlay change cannot land without its generated code.
//!
//! What is generated, and the rules it follows:
//! - One struct per object schema, one enum per string enum (named schemas and inline ones, merged by field name),
//!   every enum with an `Unknown(String)` catch-all so a value the spec does not list never fails a decode.
//! - Request structs follow the spec's `required` lists. Response structs make every field `Option` except the
//!   identifiers the overlay lists, because the spec's `required` lists do not match Connect Trade's own examples.
//! - Decimal amounts and timestamps stay `String`, as on the wire.
//! - One function per operation returning a sans-IO `Request<T>`: method, path and query, JSON body, the
//!   credentials it needs, and the type a 2xx answer decodes to. Sending is the transport's job.

use serde_json::{Map, Value};
use std::collections::{BTreeMap, BTreeSet};
use std::fmt::Write as _;
use std::path::{Path, PathBuf};
use std::process::{Command, ExitCode, Stdio};

fn main() -> ExitCode {
    let args: Vec<String> = std::env::args().skip(1).collect();
    match args.first().map(String::as_str) {
        Some("codegen") => {
            let check = args.iter().any(|a| a == "--check");
            match codegen(check) {
                Ok(()) => ExitCode::SUCCESS,
                Err(e) => {
                    eprintln!("xtask codegen: {e}");
                    ExitCode::FAILURE
                }
            }
        }
        _ => {
            eprintln!("usage: cargo xtask codegen [--check]");
            ExitCode::FAILURE
        }
    }
}

fn root() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).parent().unwrap().to_path_buf()
}

fn codegen(check: bool) -> Result<(), String> {
    let root = root();
    let spec: Value = read_json(&root.join("spec/openapi.json"))?;
    let overlay: Value = read_json(&root.join("spec/overlay.json"))?;
    let mut g = Gen::new(&spec, &overlay)?;
    g.collect()?;
    let models = rustfmt(&g.emit_models())?;
    let ops = rustfmt(&g.emit_operations())?;
    let out = root.join("crates/connecttrade/src/generated");
    let files = [("models.rs", models), ("operations.rs", ops)];
    let mut stale = vec![];
    for (name, text) in &files {
        let path = out.join(name);
        let current = std::fs::read_to_string(&path).unwrap_or_default();
        if &current != text {
            if check {
                stale.push(name.to_string());
            } else {
                std::fs::write(&path, text).map_err(|e| format!("{}: {e}", path.display()))?;
                println!("wrote {}", path.display());
            }
        }
    }
    if !stale.is_empty() {
        return Err(format!(
            "generated code is stale ({}); run `cargo xtask codegen` and commit the result",
            stale.join(", ")
        ));
    }
    if check {
        println!("generated code is up to date");
    }
    Ok(())
}

fn read_json(p: &Path) -> Result<Value, String> {
    let text = std::fs::read_to_string(p).map_err(|e| format!("{}: {e}", p.display()))?;
    serde_json::from_str(&text).map_err(|e| format!("{}: {e}", p.display()))
}

fn rustfmt(src: &str) -> Result<String, String> {
    let config = root().join("rustfmt.toml");
    let mut child = Command::new("rustfmt")
        .args(["--edition", "2021", "--emit", "stdout", "--quiet", "--config-path"])
        .arg(&config)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .spawn()
        .map_err(|e| format!("rustfmt: {e}"))?;
    use std::io::Write;
    child.stdin.take().unwrap().write_all(src.as_bytes()).map_err(|e| e.to_string())?;
    let out = child.wait_with_output().map_err(|e| e.to_string())?;
    if !out.status.success() {
        return Err(format!("rustfmt failed on generated code:\n{}", String::from_utf8_lossy(&out.stderr)));
    }
    Ok(String::from_utf8(out.stdout).unwrap())
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum Role {
    Request,
    Response,
}

struct Field {
    wire: String,
    ty: String,
    optional: bool,
    /// Optional, and sent as `null` when unset (instead of being left out).
    send_null: bool,
    doc: Option<String>,
}

struct Struct {
    doc: Option<String>,
    fields: Vec<Field>,
    role: Role,
}

struct Op {
    id: String,
    method: String,
    path: String,
    summary: Option<String>,
    path_params: Vec<String>,
    query: Vec<(String, String, Option<String>)>, // wire name, rust type, doc
    body: Option<String>,
    ok: String,
    user_scope: bool,
}

struct Gen<'a> {
    spec: &'a Value,
    overlay: &'a Value,
    schemas: &'a Map<String, Value>,
    error_names: BTreeSet<String>,
    roles: BTreeMap<String, Role>,
    structs: BTreeMap<String, Struct>,
    enums: BTreeMap<String, (Option<String>, Vec<String>)>,
    ops: Vec<Op>,
}

const METHODS: [&str; 5] = ["get", "post", "put", "patch", "delete"];

impl<'a> Gen<'a> {
    fn new(spec: &'a Value, overlay: &'a Value) -> Result<Self, String> {
        let schemas = spec["components"]["schemas"].as_object().ok_or("spec has no components.schemas")?;
        let error_names = overlay["error_schemas"]["names"]
            .as_array()
            .map(|a| a.iter().filter_map(|v| v.as_str().map(String::from)).collect())
            .unwrap_or_default();
        Ok(Gen {
            spec,
            overlay,
            schemas,
            error_names,
            roles: BTreeMap::new(),
            structs: BTreeMap::new(),
            enums: BTreeMap::new(),
            ops: vec![],
        })
    }

    fn ref_name(r: &str) -> &str {
        r.rsplit('/').next().unwrap()
    }

    /// The Rust name for a named schema.
    fn type_name(&self, schema: &str) -> String {
        if self.error_names.contains(schema) {
            "ErrorResponse".into()
        } else {
            pascal(schema)
        }
    }

    fn collect(&mut self) -> Result<(), String> {
        // roles: which named schemas are sent, which are received
        let paths = self.spec["paths"].as_object().ok_or("spec has no paths")?;
        let mut req_roots = vec![];
        let mut resp_roots = vec![];
        for item in paths.values() {
            for m in METHODS {
                let Some(op) = item.get(m) else { continue };
                if let Some(r) = op.pointer("/requestBody/content/application~1json/schema") {
                    collect_refs(r, &mut req_roots);
                }
                for params in [item.get("parameters"), op.get("parameters")].into_iter().flatten() {
                    collect_refs(params, &mut req_roots);
                }
                if let Some(rs) = op.get("responses").and_then(Value::as_object) {
                    for (code, r) in rs {
                        if code.starts_with('2') {
                            if let Some(s) = r.pointer("/content/application~1json/schema") {
                                collect_refs(s, &mut resp_roots);
                            }
                        }
                    }
                }
            }
        }
        self.mark(&req_roots, Role::Request)?;
        self.mark(&resp_roots, Role::Response)?;

        let names: Vec<(String, Role)> = self.roles.iter().map(|(k, v)| (k.clone(), *v)).collect();
        for (name, role) in names {
            let schema = self.schemas[&name].clone();
            self.define(&name, &schema, role)?;
        }
        self.collect_ops()?;
        Ok(())
    }

    fn mark(&mut self, roots: &[String], role: Role) -> Result<(), String> {
        let mut stack: Vec<String> = roots.to_vec();
        while let Some(n) = stack.pop() {
            if self.error_names.contains(&n) {
                continue;
            }
            match self.roles.get(&n) {
                Some(r) if *r == role => continue,
                // a string enum may be both sent and received
                Some(_) if self.schemas.get(&n).is_some_and(|s| s.get("enum").is_some()) => continue,
                Some(r) => return Err(format!("schema {n} is used as both {r:?} and {role:?}")),
                None => {}
            }
            self.roles.insert(n.clone(), role);
            let s = self.schemas.get(&n).ok_or_else(|| format!("missing schema {n}"))?;
            let mut refs = vec![];
            collect_refs(s, &mut refs);
            stack.extend(refs);
        }
        Ok(())
    }

    /// Flattened properties and required list of an object schema (allOf parts merged).
    fn object_parts(&self, schema: &Value) -> (Vec<(String, Value)>, BTreeSet<String>) {
        let mut props = vec![];
        let mut req = BTreeSet::new();
        let add = |s: &Value, props: &mut Vec<(String, Value)>, req: &mut BTreeSet<String>| {
            if let Some(p) = s.get("properties").and_then(Value::as_object) {
                for (k, v) in p {
                    if !props.iter().any(|(n, _)| n == k) {
                        props.push((k.clone(), v.clone()));
                    }
                }
            }
            if let Some(r) = s.get("required").and_then(Value::as_array) {
                req.extend(r.iter().filter_map(|v| v.as_str().map(String::from)));
            }
        };
        if let Some(parts) = schema.get("allOf").and_then(Value::as_array) {
            for p in parts {
                if let Some(r) = p.get("$ref").and_then(Value::as_str) {
                    let base = &self.schemas[Self::ref_name(r)];
                    add(base, &mut props, &mut req);
                } else {
                    add(p, &mut props, &mut req);
                }
            }
        }
        add(schema, &mut props, &mut req);
        (props, req)
    }

    fn define(&mut self, name: &str, schema: &Value, role: Role) -> Result<(), String> {
        if let Some(values) = schema.get("enum").and_then(Value::as_array) {
            let rust = self.type_name(name);
            self.add_enum(&rust, doc_of(schema), values);
            return Ok(());
        }
        let rust = self.type_name(name);
        if self.structs.contains_key(&rust) {
            return Ok(());
        }
        let (props, required) = self.object_parts(schema);
        let keep: BTreeSet<String> = self.overlay["response_required"][name]
            .as_array()
            .map(|a| a.iter().filter_map(|v| v.as_str().map(String::from)).collect())
            .unwrap_or_default();
        let send_null: BTreeSet<String> = self.overlay["request_nullable"][name]
            .as_array()
            .map(|a| a.iter().filter_map(|v| v.as_str().map(String::from)).collect())
            .unwrap_or_default();
        let loosen: BTreeSet<String> = self.overlay["request_optional"][name]
            .as_array()
            .map(|a| a.iter().filter_map(|v| v.as_str().map(String::from)).collect())
            .unwrap_or_default();
        let mut fields = vec![];
        for (wire, p) in props {
            let ty = self.field_type(name, &wire, &p, role)?;
            let nullable = p.get("nullable").and_then(Value::as_bool).unwrap_or(false);
            let null = role == Role::Request && send_null.contains(&wire);
            let optional = null
                || match role {
                    Role::Request => nullable || !required.contains(&wire) || loosen.contains(&wire),
                    Role::Response => !keep.contains(&wire),
                };
            fields.push(Field { doc: doc_of(&p), wire, ty, optional, send_null: null });
        }
        self.structs.insert(rust, Struct { doc: doc_of(schema), fields, role });
        Ok(())
    }

    fn field_type(&mut self, owner: &str, field: &str, p: &Value, role: Role) -> Result<String, String> {
        if let Some(r) = p.get("$ref").and_then(Value::as_str) {
            return Ok(self.type_name(Self::ref_name(r)));
        }
        if let Some(parts) = p.get("allOf").and_then(Value::as_array) {
            if parts.len() == 1 {
                if let Some(r) = parts[0].get("$ref").and_then(Value::as_str) {
                    return Ok(self.type_name(Self::ref_name(r)));
                }
            }
            return Err(format!("{owner}.{field}: allOf with more than one part is not supported"));
        }
        if let Some(values) = p.get("enum").and_then(Value::as_array) {
            let key = format!("{owner}.{field}");
            let name = self.overlay["enum_names"][&key].as_str().map(String::from).unwrap_or_else(|| pascal(field));
            self.add_enum(&name, None, values);
            return Ok(name);
        }
        match p.get("type").and_then(Value::as_str) {
            Some("string") => Ok("String".into()),
            Some("integer") => Ok("i64".into()),
            Some("number") => Ok("f64".into()),
            Some("boolean") => Ok("bool".into()),
            Some("array") => {
                let items = &p["items"];
                let key = format!("{owner}.{field}");
                if items.get("type").and_then(Value::as_str) == Some("object") && items.get("$ref").is_none() {
                    let name = self.overlay["inline_object_names"][&key]
                        .as_str()
                        .ok_or_else(|| {
                            format!("{key}: array of inline objects needs a name in overlay.inline_object_names")
                        })?
                        .to_string();
                    let has_props =
                        items.get("properties").map(|v| v.as_object().is_some_and(|o| !o.is_empty())).unwrap_or(false);
                    if has_props && !self.structs.contains_key(&name) {
                        let (props, required) = self.object_parts(items);
                        let mut fields = vec![];
                        for (wire, ip) in props {
                            let ty = self.field_type(&name, &wire, &ip, role)?;
                            let nullable = ip.get("nullable").and_then(Value::as_bool).unwrap_or(false);
                            let optional = match role {
                                Role::Request => nullable || !required.contains(&wire),
                                Role::Response => true,
                            };
                            fields.push(Field { doc: doc_of(&ip), wire, ty, optional, send_null: false });
                        }
                        self.structs.insert(name.clone(), Struct { doc: doc_of(items), fields, role });
                    }
                    // an item schema with no properties takes the same-named struct defined elsewhere; checked at emit
                    return Ok(format!("Vec<{name}>"));
                }
                if let Some(values) = items.get("enum").and_then(Value::as_array) {
                    let name =
                        self.overlay["enum_names"][&key].as_str().map(String::from).unwrap_or_else(|| pascal(field));
                    self.add_enum(&name, None, values);
                    return Ok(format!("Vec<{name}>"));
                }
                let inner = self.field_type(owner, field, items, role)?;
                Ok(format!("Vec<{inner}>"))
            }
            Some("object") | None => Ok("serde_json::Value".into()),
            Some(t) => Err(format!("{owner}.{field}: unsupported type {t}")),
        }
    }

    fn add_enum(&mut self, name: &str, doc: Option<String>, values: &[Value]) {
        let e = self.enums.entry(name.to_string()).or_insert((doc.clone(), vec![]));
        if e.0.is_none() {
            e.0 = doc;
        }
        for v in values.iter().filter_map(Value::as_str) {
            if !e.1.iter().any(|x| x == v) {
                e.1.push(v.to_string());
            }
        }
    }

    fn collect_ops(&mut self) -> Result<(), String> {
        let paths = self.spec["paths"].as_object().unwrap().clone();
        let global_sec = self.spec.get("security").cloned().unwrap_or(Value::Null);
        for (path, item) in &paths {
            for m in METHODS {
                let Some(op) = item.get(m) else { continue };
                let key = format!("{} {}", m.to_uppercase(), path);
                let id = self.overlay["operation_ids"][&key]
                    .as_str()
                    .ok_or_else(|| format!("{key}: no operation id in overlay.operation_ids"))?
                    .to_string();
                let mut params: Vec<Value> = vec![];
                for src in [item.get("parameters"), op.get("parameters")] {
                    if let Some(a) = src.and_then(Value::as_array) {
                        params.extend(a.iter().cloned());
                    }
                }
                let mut path_params = vec![];
                let mut query = vec![];
                for p in &params {
                    let name = p["name"].as_str().ok_or("parameter without a name")?.to_string();
                    let mut loc = p["in"].as_str().unwrap_or("query").to_string();
                    if let Some(fix) = self.overlay["parameter_location_fixes"][&key][&name].as_str() {
                        loc = fix.to_string();
                    }
                    match loc.as_str() {
                        "path" => {
                            if !path.contains(&format!("{{{name}}}")) {
                                return Err(format!(
                                    "{key}: path parameter {name} is not in the path (fix it in the overlay)"
                                ));
                            }
                            path_params.push(name)
                        }
                        "query" => {
                            let schema = p.get("schema").cloned().unwrap_or(Value::Null);
                            let enum_key = format!("{key} {name}");
                            let ty = if let Some(values) = schema.get("enum").and_then(Value::as_array) {
                                let n = self.overlay["enum_names"][&enum_key]
                                    .as_str()
                                    .ok_or_else(|| {
                                        format!("{enum_key}: query enum needs a name in overlay.enum_names")
                                    })?
                                    .to_string();
                                self.add_enum(&n, None, values);
                                n
                            } else {
                                self.field_type(&pascal(&id), &name, &schema, Role::Request)?
                            };
                            query.push((name, ty, doc_of(p)));
                        }
                        other => return Err(format!("{key}: parameter location {other} is not supported")),
                    }
                }
                let body = op
                    .pointer("/requestBody/content/application~1json/schema/$ref")
                    .and_then(Value::as_str)
                    .map(|r| self.type_name(Self::ref_name(r)));
                let mut ok = None;
                if let Some(rs) = op.get("responses").and_then(Value::as_object) {
                    for (code, r) in rs {
                        if code.starts_with('2') {
                            let s = r.pointer("/content/application~1json/schema").cloned().unwrap_or(Value::Null);
                            ok = Some(self.field_type(&pascal(&id), "response", &s, Role::Response)?);
                            break;
                        }
                    }
                }
                let sec = op.get("security").cloned().unwrap_or_else(|| global_sec.clone());
                let user_scope = sec.to_string().contains("user-id");
                self.ops.push(Op {
                    id,
                    method: m.to_uppercase(),
                    path: path.clone(),
                    summary: op.get("summary").and_then(Value::as_str).map(String::from),
                    path_params,
                    query,
                    body,
                    ok: ok.ok_or_else(|| format!("{key}: no 2xx response"))?,
                    user_scope,
                });
            }
        }
        self.ops.sort_by(|a, b| a.id.cmp(&b.id));
        Ok(())
    }

    fn emit_models(&self) -> String {
        let mut s = String::new();
        s.push_str(HEADER);
        s.push_str("//! Models: every schema in the spec, plus the overlay's named inline objects.\n\n");
        s.push_str("use serde::{Deserialize, Serialize};\n\n");
        let mut erroring = BTreeSet::new();
        for (name, st) in &self.structs {
            for f in &st.fields {
                let inner = f.ty.trim_start_matches("Vec<").trim_end_matches('>');
                if inner.chars().next().is_some_and(char::is_uppercase)
                    && !self.structs.contains_key(inner)
                    && !self.enums.contains_key(inner)
                    && inner != "ErrorResponse"
                    && !inner.starts_with("serde_json")
                    && inner != "String"
                {
                    erroring.insert(format!("{name}.{}: {inner}", f.wire));
                }
            }
        }
        assert!(erroring.is_empty(), "undefined types: {erroring:?}");

        // the one error type
        s.push_str("/// The error body Connect Trade returns (all four error schemas in the spec).\n");
        s.push_str("#[derive(Debug, Clone, PartialEq, Default, Serialize, Deserialize)]\n");
        s.push_str("pub struct ErrorResponse {\n");
        s.push_str(
            "    #[serde(default, skip_serializing_if = \"Option::is_none\")]\n    pub message: Option<String>,\n",
        );
        s.push_str("    #[serde(default, skip_serializing_if = \"Option::is_none\")]\n    pub code: Option<String>,\n");
        s.push_str("    #[serde(default, skip_serializing_if = \"Option::is_none\")]\n    pub details: Option<serde_json::Value>,\n");
        s.push_str("}\n\n");

        for (name, st) in &self.structs {
            doc(&mut s, st.doc.as_deref(), "");
            let _ = writeln!(
                s,
                "/// {}",
                match st.role {
                    Role::Request => "Sent to Connect Trade.",
                    Role::Response => "Received from Connect Trade: every field is optional except the identifiers.",
                }
            );
            let default = if st.fields.iter().all(|f| f.optional) { ", Default" } else { "" };
            let _ = writeln!(s, "#[derive(Debug, Clone, PartialEq{default}, Serialize, Deserialize)]");
            let _ = writeln!(s, "pub struct {name} {{");
            for f in &st.fields {
                doc(&mut s, f.doc.as_deref(), "    ");
                let ident = snake_ident(&f.wire);
                if ident.trim_start_matches("r#") != f.wire {
                    let _ = writeln!(s, "    #[serde(rename = \"{}\")]", f.wire);
                }
                if f.send_null {
                    s.push_str("    /// Sent as `null` when unset.\n    #[serde(default)]\n");
                    let _ = writeln!(s, "    pub {ident}: Option<{}>,", f.ty);
                } else if f.optional {
                    s.push_str("    #[serde(default, skip_serializing_if = \"Option::is_none\")]\n");
                    let _ = writeln!(s, "    pub {ident}: Option<{}>,", f.ty);
                } else {
                    let _ = writeln!(s, "    pub {ident}: {},", f.ty);
                }
            }
            s.push_str("}\n\n");
        }

        for (name, (d, values)) in &self.enums {
            doc(&mut s, d.as_deref(), "");
            s.push_str("///\n/// Values the spec does not list decode to `Unknown`, so a new broker value never fails a decode.\n");
            s.push_str("#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]\n");
            let _ = writeln!(s, "pub enum {name} {{");
            for v in values {
                let _ = writeln!(s, "    #[serde(rename = \"{v}\")]\n    {},", variant(v));
            }
            s.push_str("    /// A value the spec does not list.\n    #[serde(untagged)]\n    Unknown(String),\n}\n\n");
            let _ = writeln!(s, "impl {name} {{");
            s.push_str(
                "    /// The value as sent on the wire.\n    pub fn as_str(&self) -> &str {\n        match self {\n",
            );
            for v in values {
                let _ = writeln!(s, "            Self::{} => \"{v}\",", variant(v));
            }
            s.push_str("            Self::Unknown(s) => s,\n        }\n    }\n}\n\n");
            let _ = writeln!(s, "impl std::fmt::Display for {name} {{");
            s.push_str("    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {\n        f.write_str(self.as_str())\n    }\n}\n\n");
        }
        s
    }

    fn emit_operations(&self) -> String {
        let mut s = String::new();
        s.push_str(HEADER);
        s.push_str("//! Operations: one function per endpoint, each returning a sans-IO [`Request`].\n\n");
        s.push_str("#[allow(unused_imports)]\nuse super::models::*;\nuse crate::request::{Method, Query, Request, Scope, encode_path};\n\n");

        // operation table
        s.push_str("/// Every operation in the spec: id, method, path template, and whether it acts for a user.\n");
        s.push_str("pub const OPERATIONS: &[(&str, Method, &str, Scope)] = &[\n");
        for o in &self.ops {
            let _ = writeln!(
                s,
                "    (\"{}\", Method::{}, \"{}\", Scope::{}),",
                o.id,
                method_variant(&o.method),
                o.path,
                if o.user_scope { "User" } else { "Platform" }
            );
        }
        s.push_str("];\n\n");

        for o in &self.ops {
            let qname = format!("{}Query", pascal(&o.id));
            if !o.query.is_empty() {
                let _ = writeln!(s, "/// Query parameters of [`{}`]. Unset fields are not sent.", o.id);
                s.push_str("#[derive(Debug, Clone, Default, PartialEq)]\n");
                let _ = writeln!(s, "pub struct {qname} {{");
                for (name, ty, d) in &o.query {
                    doc(&mut s, d.as_deref(), "    ");
                    let _ = writeln!(s, "    pub {}: Option<{ty}>,", snake_ident(name));
                }
                s.push_str("}\n\n");
                let _ = writeln!(s, "impl {qname} {{");
                s.push_str("    fn to_query(&self) -> Query {\n        let mut q = Query::default();\n");
                for (name, ty, _) in &o.query {
                    let ident = snake_ident(name);
                    let conv = if ty == "String" {
                        "v.clone()".to_string()
                    } else if ty == "i64" || ty == "bool" || ty == "f64" {
                        "v.to_string()".to_string()
                    } else {
                        "v.as_str().to_string()".to_string()
                    };
                    let _ = writeln!(s, "        if let Some(v) = &self.{ident} {{ q.push(\"{name}\", {conv}); }}");
                }
                s.push_str("        q\n    }\n}\n\n");
            }

            let _ = writeln!(
                s,
                "/// `{} {}`{}",
                o.method,
                o.path,
                o.summary.as_deref().map(|x| format!(": {x}.")).unwrap_or_default()
            );
            let _ = writeln!(s, "///\n/// Needs {} credentials.", if o.user_scope { "user" } else { "platform" });
            let mut args = vec![];
            for p in &o.path_params {
                args.push(format!("{}: &str", snake_ident(p)));
            }
            if !o.query.is_empty() {
                args.push(format!("query: &{qname}"));
            }
            if let Some(b) = &o.body {
                args.push(format!("body: &{b}"));
            }
            let _ = writeln!(s, "pub fn {}({}) -> Request<{}> {{", o.id, args.join(", "), o.ok);
            // path
            let mut fmt = o.path.clone();
            let mut fmt_args = vec![];
            for p in &o.path_params {
                fmt = fmt.replace(&format!("{{{p}}}"), "{}");
                fmt_args.push(format!("encode_path({})", snake_ident(p)));
            }
            let path_expr = if fmt_args.is_empty() {
                format!("\"{fmt}\".to_string()")
            } else {
                format!("format!(\"{fmt}\", {})", fmt_args.join(", "))
            };
            let query_expr =
                if o.query.is_empty() { "Query::default()".to_string() } else { "query.to_query()".to_string() };
            let body_expr = if o.body.is_some() {
                "Some(serde_json::to_vec(body).expect(\"a model always serialises\"))"
            } else {
                "None"
            };
            let _ = writeln!(
                s,
                "    Request::new(\"{}\", Method::{}, {path_expr}, {query_expr}, {body_expr}, Scope::{})",
                o.id,
                method_variant(&o.method),
                if o.user_scope { "User" } else { "Platform" }
            );
            s.push_str("}\n\n");
        }
        s
    }
}

const HEADER: &str =
    "// @generated by `cargo xtask codegen` from spec/openapi.json and spec/overlay.json. Do not edit.\n";

fn collect_refs(v: &Value, out: &mut Vec<String>) {
    match v {
        Value::Object(m) => {
            if let Some(r) = m.get("$ref").and_then(Value::as_str) {
                out.push(r.rsplit('/').next().unwrap().to_string());
            }
            for x in m.values() {
                collect_refs(x, out);
            }
        }
        Value::Array(a) => a.iter().for_each(|x| collect_refs(x, out)),
        _ => {}
    }
}

fn doc_of(v: &Value) -> Option<String> {
    v.get("description").and_then(Value::as_str).map(|d| d.trim().to_string()).filter(|d| !d.is_empty())
}

fn doc(s: &mut String, d: Option<&str>, indent: &str) {
    if let Some(d) = d {
        for line in d.lines() {
            let line = line.trim_end();
            if line.is_empty() {
                let _ = writeln!(s, "{indent}///");
            } else {
                // keep doc tests out of descriptions: indented code would compile as a doc test
                let _ = writeln!(s, "{indent}/// {}", line.trim_start().replace("```", "`"));
            }
        }
    }
}

fn pascal(s: &str) -> String {
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

fn variant(v: &str) -> String {
    let v = v.replace('+', "_plus");
    let lower = v.to_ascii_lowercase();
    let p = pascal(&lower);
    if p.chars().next().is_some_and(|c| c.is_ascii_digit()) {
        format!("V{p}")
    } else {
        p
    }
}

fn snake_ident(s: &str) -> String {
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

fn method_variant(m: &str) -> &'static str {
    match m {
        "GET" => "Get",
        "POST" => "Post",
        "PUT" => "Put",
        "PATCH" => "Patch",
        "DELETE" => "Delete",
        _ => unreachable!(),
    }
}
