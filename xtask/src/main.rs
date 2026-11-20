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

#![allow(
    clippy::print_stdout,
    clippy::print_stderr,
    reason = "a command-line tool: what it wrote, and why it failed, go to the terminal"
)]

mod collect;
mod emit;
mod names;

use collect::Gen;
use serde_json::Value;
use std::io::Write as _;
use std::path::{Path, PathBuf};
use std::process::{Command, ExitCode, Stdio};

fn main() -> ExitCode {
    let args: Vec<String> = std::env::args().skip(1).collect();
    if args.first().map(String::as_str) != Some("codegen") {
        eprintln!("usage: cargo xtask codegen [--check]");
        return ExitCode::FAILURE;
    }
    let check = args.iter().any(|a| a == "--check");
    match codegen(check) {
        Ok(()) => ExitCode::SUCCESS,
        Err(e) => {
            eprintln!("xtask codegen: {e}");
            ExitCode::FAILURE
        }
    }
}

/// The workspace root (xtask lives one level down).
fn root() -> PathBuf {
    let here = Path::new(env!("CARGO_MANIFEST_DIR"));
    here.parent().unwrap_or(here).to_path_buf()
}

fn codegen(check: bool) -> Result<(), String> {
    let root = root();
    let spec: Value = read_json(&root.join("spec/openapi.json"))?;
    let overlay: Value = read_json(&root.join("spec/overlay.json"))?;
    let mut g = Gen::new(&spec, &overlay)?;
    g.collect()?;
    let models = rustfmt(&g.emit_models().map_err(|e| e.to_string())?)?;
    let ops = rustfmt(&g.emit_operations().map_err(|e| e.to_string())?)?;
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
    child.stdin.take().ok_or("rustfmt has no stdin")?.write_all(src.as_bytes()).map_err(|e| e.to_string())?;
    let out = child.wait_with_output().map_err(|e| e.to_string())?;
    if !out.status.success() {
        return Err(format!("rustfmt failed on generated code:\n{}", String::from_utf8_lossy(&out.stderr)));
    }
    String::from_utf8(out.stdout).map_err(|e| format!("rustfmt wrote invalid UTF-8: {e}"))
}
