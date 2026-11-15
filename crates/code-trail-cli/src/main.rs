use code_trail_core::{Engine, NavigationRequest};
use serde::Deserialize;
use serde_json::{Value, json};
use std::io::{self, BufRead, Write};
use std::path::PathBuf;

#[derive(Deserialize)]
struct Params {
    file: PathBuf,
    line: usize,
    column: usize,
    symbol: String,
}

fn error(id: Value, code: &str, message: impl AsRef<str>) -> Value {
    json!({"version": 1, "id": id, "error": {"code": code, "message": message.as_ref()}})
}

fn respond(engine: &mut Engine, line: &str) -> Value {
    let request: Value = match serde_json::from_str(line) {
        Ok(value) => value,
        Err(e) => return error(Value::Null, "parse_error", e.to_string()),
    };
    let id = request.get("id").cloned().unwrap_or(Value::Null);
    if !request.is_object() || !(id.is_string() || id.is_number()) {
        return error(
            id,
            "invalid_request",
            "request requires a string or numeric id",
        );
    }
    if request.get("version").and_then(Value::as_u64) != Some(1) {
        return error(id, "unsupported_version", "version must be 1");
    }
    let method = match request.get("method").and_then(Value::as_str) {
        Some(method @ ("definition" | "references" | "implementation")) => method,
        Some(_) => return error(id, "method_not_found", "supported methods: definition, references, implementation"),
        None => return error(id, "invalid_request", "method must be a string"),
    };
    let params: Params = match serde_json::from_value(request["params"].clone()) {
        Ok(params) => params,
        Err(e) => return error(id, "invalid_request", e.to_string()),
    };
    if !params.file.is_absolute() || params.line == 0 {
        return error(
            id,
            "invalid_request",
            "file must be absolute and line must be at least 1",
        );
    }
    let navigation = NavigationRequest {
        file: params.file,
        line: params.line,
        column: params.column,
        symbol: params.symbol,
    };
    let result = match method {
        "references" => engine.references(&navigation),
        "implementation" => engine.implementations(&navigation),
        _ => engine.definitions(&navigation),
    };
    match result {
        Ok(locations) => {
            let locations: Vec<Value> = locations
                .into_iter()
                .map(|location| {
                    json!({
                        "file": location.file, "line": location.line, "column": location.column,
                        "summary": location.summary, "kind": location.kind, "score": location.score,
                    })
                })
                .collect();
            json!({"version": 1, "id": id, "result": locations})
        }
        Err(message) => error(id, "navigation_error", message),
    }
}

fn run() -> io::Result<()> {
    let mut arguments = std::env::args().skip(1);
    if let Some(argument) = arguments.next() {
        if arguments.next().is_some() {
            eprintln!("code-trail: expected no arguments, --help, or --version");
            std::process::exit(2);
        }
        match argument.as_str() {
            "--help" | "-h" => println!(
                "code-trail: Java definition, reference, and implementation navigation over persistent stdio\nUsage: code-trail [--help | --version]\nSend one version-1 JSON definition, references, or implementation request per line; EOF exits."
            ),
            "--version" | "-V" => println!("code-trail {}", env!("CARGO_PKG_VERSION")),
            _ => {
                eprintln!("code-trail: unknown argument: {argument}");
                std::process::exit(2);
            }
        }
        return Ok(());
    }
    let mut engine = Engine::default();
    let input = io::stdin();
    let mut output = io::BufWriter::new(io::stdout().lock());
    for line in input.lock().lines() {
        let response = respond(&mut engine, &line?);
        serde_json::to_writer(&mut output, &response)?;
        output.write_all(b"\n")?;
        output.flush()?;
    }
    Ok(())
}

fn main() {
    if let Err(error) = run() {
        if error.kind() != io::ErrorKind::BrokenPipe {
            eprintln!("code-trail: {error}");
            std::process::exit(1);
        }
    }
}
