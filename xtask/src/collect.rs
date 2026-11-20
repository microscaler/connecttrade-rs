//! Reading the spec and the overlay into structs, enums and operations.
//!
//! Lookups into the spec and the overlay index `serde_json::Value` (`spec["paths"]`): that returns `Null` for a
//! missing key and never panics. Maps and slices are read with `get`.
#![allow(clippy::indexing_slicing, reason = "serde_json::Value indexing returns Null for a missing key, never panics")]

use crate::names::{collect_refs, doc_of, method_variant, pascal};
use serde_json::{Map, Value};
use std::collections::{BTreeMap, BTreeSet};

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(crate) enum Role {
    Request,
    Response,
}

pub(crate) struct Field {
    pub(crate) wire: String,
    pub(crate) ty: String,
    pub(crate) optional: bool,
    /// Optional, and sent as `null` when unset (instead of being left out).
    pub(crate) send_null: bool,
    pub(crate) doc: Option<String>,
}

pub(crate) struct Struct {
    pub(crate) doc: Option<String>,
    pub(crate) fields: Vec<Field>,
    pub(crate) role: Role,
}

pub(crate) struct Op {
    pub(crate) id: String,
    pub(crate) method: String,
    /// The `Method` variant the generated code names.
    pub(crate) method_variant: &'static str,
    pub(crate) path: String,
    pub(crate) summary: Option<String>,
    pub(crate) path_params: Vec<String>,
    pub(crate) query: Vec<(String, String, Option<String>)>, // wire name, rust type, doc
    pub(crate) body: Option<String>,
    pub(crate) ok: String,
    pub(crate) user_scope: bool,
}

pub(crate) struct Gen<'a> {
    pub(crate) spec: &'a Value,
    pub(crate) overlay: &'a Value,
    pub(crate) schemas: &'a Map<String, Value>,
    pub(crate) error_names: BTreeSet<String>,
    pub(crate) roles: BTreeMap<String, Role>,
    pub(crate) structs: BTreeMap<String, Struct>,
    pub(crate) enums: BTreeMap<String, (Option<String>, Vec<String>)>,
    pub(crate) ops: Vec<Op>,
}

pub(crate) const METHODS: [&str; 5] = ["get", "post", "put", "patch", "delete"];

impl<'a> Gen<'a> {
    pub(crate) fn new(spec: &'a Value, overlay: &'a Value) -> Result<Self, String> {
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
        r.rsplit('/').next().unwrap_or(r)
    }

    /// The Rust name for a named schema.
    fn type_name(&self, schema: &str) -> String {
        if self.error_names.contains(schema) {
            "ErrorResponse".into()
        } else {
            pascal(schema)
        }
    }

    pub(crate) fn collect(&mut self) -> Result<(), String> {
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
            let schema = self.schemas.get(&name).cloned().ok_or_else(|| format!("missing schema {name}"))?;
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
                let base = p.get("$ref").and_then(Value::as_str).and_then(|r| self.schemas.get(Self::ref_name(r)));
                if let Some(base) = base {
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
                if let Some(r) = parts.first().and_then(|p| p.get("$ref")).and_then(Value::as_str) {
                    return Ok(self.type_name(Self::ref_name(r)));
                }
            }
            return Err(format!("{owner}.{field}: allOf with more than one part is not supported"));
        }
        if let Some(values) = p.get("enum").and_then(Value::as_array) {
            let key = format!("{owner}.{field}");
            let name = self.overlay["enum_names"][&key].as_str().map_or_else(|| pascal(field), String::from);
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
                        items.get("properties").is_some_and(|v| v.as_object().is_some_and(|o| !o.is_empty()));
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
                    let name = self.overlay["enum_names"][&key].as_str().map_or_else(|| pascal(field), String::from);
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
        let paths = self.spec["paths"].as_object().ok_or("spec has no paths")?.clone();
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
                            path_params.push(name);
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
                    method_variant: method_variant(m).ok_or_else(|| format!("{key}: method {m} is not supported"))?,
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
}
