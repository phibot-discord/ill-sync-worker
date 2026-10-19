use phi_unpackd::job::{self, Paths};
use phi_unpackd::layout::should_skip;
use serde::Deserialize;
use sha2::{Digest, Sha256};
use std::io::Read;
use std::sync::atomic::{AtomicBool, Ordering};
use tiny_http::{Header, Method, Request, Response, Server, StatusCode};

static BUSY: AtomicBool = AtomicBool::new(false);

/// Clears BUSY when the request thread ends, including by panic
struct BusyGuard;

impl Drop for BusyGuard {
    fn drop(&mut self) {
        BUSY.store(false, Ordering::Release);
    }
}

/// The Worker's cron sends `{"check":true}`; its admin `POST /unpack` forwards `{}` for an empty body,
/// which (as before) forces a full run
#[derive(Deserialize)]
struct Req {
    #[serde(default)]
    check: bool,
    #[serde(rename = "apkUrl", default)]
    apk_url: String,
}

fn main() {
    let args: Vec<String> = std::env::args().collect();
    // --export <apk> <dir> <code> <name>: extract only, into any directory; --history-report <dir>: what publishing
    // it would add to history/ (reads R2, writes nothing)
    if let Some(at) = args.iter().position(|arg| arg == "--export") {
        let arg = |n: usize| args.get(at + n).cloned().unwrap_or_default();
        match phi_unpackd::extract::export(arg(1).as_ref(), arg(2).as_ref(), &arg(3), &arg(4)) {
            Ok(done) => println!("jackets {} warnings {:?}", done.jackets, done.warnings),
            Err(err) => {
                eprintln!("export failed: {err}");
                std::process::exit(1);
            }
        }
        return;
    }
    // --insert-build <apk> <code> <name> [--dry-run]: a build that never got through in order, added to history/ only
    if let Some(at) = args.iter().position(|arg| arg == "--insert-build") {
        let arg = |n: usize| args.get(at + n).cloned().unwrap_or_default();
        let paths = Paths::from_env();
        let _lock = match paths.try_lock() {
            Ok(Some(lock)) => lock,
            Ok(None) => {
                eprintln!("another unpack is running");
                std::process::exit(1);
            }
            Err(err) => {
                eprintln!("lock failed: {err}");
                std::process::exit(1);
            }
        };
        let out = paths.root.join("insert").join(arg(2));
        let done = phi_unpackd::extract::export(arg(1).as_ref(), &out, &arg(2), &arg(3))
            .and_then(|_| phi_unpackd::layout::write_info_tsv(&out))
            .and_then(|_| phi_unpackd::pez::pack_all(&out))
            .and_then(|_| {
                phi_unpackd::publish::insert_build(&out, args.iter().any(|a| a == "--dry-run"))
            });
        if let Err(err) = done {
            eprintln!("insert failed: {err}");
            std::process::exit(1);
        }
        return;
    }
    if let Some(at) = args.iter().position(|arg| arg == "--history-report") {
        let out = args.get(at + 1).cloned().unwrap_or_default();
        if let Err(err) = phi_unpackd::publish::history_report(out.as_ref()) {
            eprintln!("history report failed: {err}");
            std::process::exit(1);
        }
        return;
    }
    // --extras <apk> <dir>: only the videos, the Collection and its art; touches no UNPACK_ROOT and uploads nothing
    // --same-chart <old> <new>...: each new chart against the old one, "same" or what differs
    if let Some(at) = args.iter().position(|arg| arg == "--same-chart") {
        let read = |p: &String| std::fs::read(p).unwrap_or_else(|e| panic!("{p}: {e}"));
        let old = read(&args[at + 1]);
        for new in &args[at + 2..] {
            let verdict = phi_unpackd::chart_eq::compare(&old, &read(new));
            println!("{new}\t{}", verdict.as_deref().unwrap_or("same"));
        }
        return;
    }
    if let Some(at) = args.iter().position(|arg| arg == "--extras") {
        let (Some(apk), Some(out)) = (args.get(at + 1), args.get(at + 2)) else {
            eprintln!("usage: phi-unpackd --extras <apk> <output dir>");
            std::process::exit(2);
        };
        match phi_unpackd::extract::export_game_extras(apk.as_ref(), out.as_ref()) {
            Ok((clips, entries, arts)) => {
                println!("videos {clips} collection entries {entries} collection art {arts}")
            }
            Err(err) => {
                eprintln!("extras failed: {err}");
                std::process::exit(1);
            }
        }
        return;
    }
    if std::env::args().any(|arg| arg == "--unpack") {
        let paths = Paths::from_env();
        let lock = match paths.try_lock() {
            Ok(Some(lock)) => lock,
            Ok(None) => {
                eprintln!("another unpack is running");
                std::process::exit(1);
            }
            Err(err) => {
                eprintln!("lock failed: {err}");
                std::process::exit(1);
            }
        };
        let apk = match job::resolve_apk(None) {
            Ok(apk) => apk,
            Err(err) => {
                eprintln!("resolve apk failed: {err}");
                std::process::exit(1);
            }
        };
        match job::unpack(&paths, apk, false, &lock) {
            Ok(Some(state)) => {
                println!(
                    "ok version {} songs {} jackets {} pez {}",
                    state.version_name, state.songs, state.jackets, state.pez
                );
                for warning in &state.warnings {
                    println!("warning: {warning}");
                }
            }
            Ok(None) => println!("skipped"),
            Err(err) => {
                eprintln!("unpack failed: {err}");
                std::process::exit(1);
            }
        }
        return;
    }
    if std::env::args().any(|arg| arg == "--publish") {
        let paths = Paths::from_env();
        let _lock = match paths.try_lock() {
            Ok(Some(lock)) => lock,
            Ok(None) => {
                eprintln!("another unpack is running");
                std::process::exit(1);
            }
            Err(err) => {
                eprintln!("lock failed: {err}");
                std::process::exit(1);
            }
        };
        // notesInfo.json is rebuilt from the charts on disk, in case an extract stopped part way
        match phi_unpackd::publish::publish(&paths.output(), &paths.manifest(), true) {
            Ok((n, warnings)) => {
                println!("uploaded {n}");
                for warning in warnings {
                    println!("warning: {warning}");
                }
                return;
            }
            Err(err) => {
                eprintln!("publish failed: {err}");
                std::process::exit(1);
            }
        }
    }
    let listen = std::env::var("UNPACK_LISTEN").unwrap_or_else(|_| "127.0.0.1:8788".into());
    let secret = std::env::var("UNPACK_SECRET").unwrap_or_default();
    if secret.is_empty() {
        eprintln!("UNPACK_SECRET is empty; refusing to listen");
        std::process::exit(1);
    }
    let secret_hash = Sha256::digest(secret.as_bytes());
    let server = Server::http(&listen).expect("bind");
    eprintln!("phi-unpackd listening on {listen}");
    for mut req in server.incoming_requests() {
        let path = req.url().split('?').next().unwrap_or("/").to_string();
        let route = match (req.method(), path.as_str()) {
            (Method::Post, "/" | "/unpack") => Route::Unpack,
            (Method::Get, "/status") => Route::Status,
            _ => {
                let _ = req.respond(json(404, r#"{"error":"not found"}"#));
                continue;
            }
        };
        // Compare digests so the time taken says nothing about the secret or its length
        let given = Sha256::digest(header(&req, "x-phi-proxy-key").as_bytes());
        if given
            .iter()
            .zip(secret_hash.iter())
            .fold(0u8, |acc, (a, b)| acc | (a ^ b))
            != 0
        {
            let _ = req.respond(json(401, r#"{"error":"unauthorized"}"#));
            continue;
        }
        if matches!(route, Route::Status) {
            let _ = req.respond(json(200, &status_body()));
            continue;
        }
        let mut body = String::new();
        let _ = req.as_reader().take(1_048_576).read_to_string(&mut body);
        let parsed = match parse_request(&body) {
            Ok(parsed) => parsed,
            Err(err) => {
                let _ = req.respond(json(400, &serde_json::json!({ "error": err }).to_string()));
                continue;
            }
        };
        // Answer 409 before any TapTap call while a run (or its version check) is in progress
        if BUSY.swap(true, Ordering::AcqRel) {
            let _ = req.respond(json(409, r#"{"busy":true}"#));
            continue;
        }
        let guard = BusyGuard;
        // TapTap lookups and the job itself run off the accept loop, so /status keeps answering
        std::thread::spawn(move || {
            let _guard = guard;
            handle_unpack(req, parsed);
        });
    }
}

enum Route {
    Unpack,
    Status,
}

fn parse_request(body: &str) -> Result<Req, String> {
    let bad = |e: serde_json::Error| {
        format!(r#"body must be a JSON object such as {{"check":true}}: {e}"#)
    };
    // Parsed as a map first: serde would otherwise accept `[true, ""]` for the struct
    let map: serde_json::Map<String, serde_json::Value> =
        serde_json::from_str(body).map_err(bad)?;
    let req: Req = serde_json::from_value(serde_json::Value::Object(map)).map_err(bad)?;
    if !req.apk_url.is_empty() && !req.apk_url.starts_with("https://") {
        return Err("apkUrl must be an https URL".into());
    }
    Ok(req)
}

fn status_body() -> String {
    let paths = Paths::from_env();
    let mut state = serde_json::to_value(job::read_state(&paths.root)).unwrap_or_default();
    if let Some(obj) = state.as_object_mut() {
        obj.insert("busy".into(), BUSY.load(Ordering::Acquire).into());
    }
    state.to_string()
}

fn handle_unpack(req: Request, parsed: Req) {
    let paths = Paths::from_env();
    let lock = match paths.try_lock() {
        Ok(Some(lock)) => lock,
        Ok(None) => {
            let _ = req.respond(json(409, r#"{"busy":true}"#));
            return;
        }
        Err(err) => {
            let _ = req.respond(json(500, &serde_json::json!({ "error": err }).to_string()));
            return;
        }
    };
    let apk = match job::resolve_apk((!parsed.apk_url.is_empty()).then_some(parsed.apk_url)) {
        Ok(apk) => apk,
        Err(err) => {
            let _ = req.respond(json(500, &serde_json::json!({ "error": err }).to_string()));
            return;
        }
    };
    let prev = job::read_state(&paths.root);
    if should_skip(
        parsed.check,
        &prev.version_code,
        &prev.status,
        &apk.version_code,
    ) {
        let body = serde_json::json!({ "skipped": true, "versionCode": apk.version_code });
        let _ = req.respond(json(200, &body.to_string()));
        return;
    }
    // A version that keeps failing is retried on a doubling delay instead of every cron tick
    if let Some(at) =
        job::retry_at(&prev, &apk.version_code).filter(|at| parsed.check && job::now_secs() < *at)
    {
        let body = serde_json::json!({
            "skipped": true,
            "versionCode": apk.version_code,
            "retryAt": at,
            "attempts": prev.attempts,
            "error": prev.error,
        });
        let _ = req.respond(json(200, &body.to_string()));
        return;
    }
    let _ = req.respond(json(202, r#"{"accepted":true}"#));
    if let Err(err) = job::unpack(&paths, apk, false, &lock) {
        eprintln!("unpack failed: {err}");
    }
}

fn header(req: &Request, name: &str) -> String {
    req.headers()
        .iter()
        .find(|h| {
            h.field
                .as_str()
                .as_bytes()
                .eq_ignore_ascii_case(name.as_bytes())
        })
        .map(|h| h.value.as_str().trim().to_string())
        .unwrap_or_default()
}

fn json(status: u16, body: &str) -> Response<std::io::Cursor<Vec<u8>>> {
    Response::from_string(body.to_string())
        .with_status_code(StatusCode(status))
        .with_header(Header::from_bytes(&b"Content-Type"[..], &b"application/json"[..]).unwrap())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_the_workers_bodies_and_rejects_the_rest() {
        let check = |body: &str| parse_request(body).map(|r| (r.check, r.apk_url));
        assert_eq!(check(r#"{"check":true}"#), Ok((true, String::new())));
        // What the Worker's admin POST /unpack forwards for an empty body: a full run, as before
        assert_eq!(check("{}"), Ok((false, String::new())));
        assert_eq!(
            check(r#"{"check":false,"apkUrl":"https://x.example/a.apk"}"#),
            Ok((false, "https://x.example/a.apk".into()))
        );
        for bad in [
            "",
            "garbage",
            "null",
            "[]",
            "[false]",
            "true",
            r#"{"check":"yes"}"#,
            r#"{"apkUrl":"http://x.example/a.apk"}"#,
        ] {
            assert!(parse_request(bad).is_err(), "{bad:?}");
        }
    }
}
