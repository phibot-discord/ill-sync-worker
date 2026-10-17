use crate::extract;
use crate::layout::{should_skip, write_info_tsv};
use crate::par;
use crate::pez::pack_all;
use crate::publish::{self, publish};
use crate::taptap::{self, Apk};
use serde::{Deserialize, Serialize};
use std::fs::{self, File, OpenOptions, TryLockError};
use std::panic::{catch_unwind, AssertUnwindSafe};
use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::{Duration, Instant};

/// First retry delay after a failed run of a version; doubles per failure up to `RETRY_MAX_SECS`
const RETRY_BASE_SECS: u64 = 600;
const RETRY_MAX_SECS: u64 = 6 * 3600;
const DOWNLOAD_ATTEMPTS: u32 = 5;

#[derive(Clone, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct State {
    pub status: String,
    pub version_code: String,
    pub version_name: String,
    pub songs: u64,
    pub pez: u64,
    pub jackets: u64,
    pub pv: u64,
    pub collection: u64,
    pub collection_art: u64,
    pub warnings: Vec<String>,
    pub error: String,
    pub updated_at: String,
    pub attempts: u32,
    pub started_at: u64,
    pub finished_at: u64,
    pub stage: String,
}

pub struct Paths {
    pub root: PathBuf,
}

/// Held for a whole run so the daemon and the `--unpack`/`--publish` CLI never write output/ at once
/// The OS releases it when the process exits, however it exits
pub struct RunLock(#[allow(dead_code)] File);

impl Paths {
    pub fn from_env() -> Self {
        Self {
            root: std::env::var("UNPACK_ROOT")
                .unwrap_or_else(|_| "/mnt/nvme/phi/ill-sync".into())
                .into(),
        }
    }
    pub fn output(&self) -> PathBuf {
        self.root.join("output")
    }
    /// Key -> MD5, size and type of every object this daemon uploaded to R2
    pub fn manifest(&self) -> PathBuf {
        self.root.join("published.json")
    }
    fn state_path(&self) -> PathBuf {
        self.root.join("state.json")
    }
    /// The run lock, or None while another run (in any process) holds it
    pub fn try_lock(&self) -> Result<Option<RunLock>, String> {
        fs::create_dir_all(&self.root).map_err(|e| e.to_string())?;
        let file = OpenOptions::new()
            .create(true)
            .truncate(false)
            .write(true)
            .open(self.root.join(".lock"))
            .map_err(|e| e.to_string())?;
        match file.try_lock() {
            Ok(()) => Ok(Some(RunLock(file))),
            Err(TryLockError::WouldBlock) => Ok(None),
            Err(TryLockError::Error(err)) => Err(err.to_string()),
        }
    }
}

pub fn read_state(root: &Path) -> State {
    fs::read(root.join("state.json"))
        .ok()
        .and_then(|b| serde_json::from_slice(&b).ok())
        .unwrap_or_default()
}

fn write_state(paths: &Paths, state: &State) -> Result<(), String> {
    fs::create_dir_all(&paths.root).map_err(|e| e.to_string())?;
    let body = serde_json::to_vec_pretty(state).map_err(|e| e.to_string())?;
    let tmp = paths.root.join("state.json.tmp");
    fs::write(&tmp, body).map_err(|e| e.to_string())?;
    fs::rename(tmp, paths.state_path()).map_err(|e| e.to_string())
}

pub fn resolve_apk(url: Option<String>) -> Result<Apk, String> {
    if let Some(download_url) = url.filter(|s| !s.is_empty()) {
        return Ok(Apk {
            version_code: download_url.clone(),
            version_name: String::new(),
            download_url,
        });
    }
    taptap::latest_apk()
}

/// Unix seconds before which a scheduled check should not retry `next_code`, when its latest run
/// failed (or died while running). The delay counts from when that run ended, or from its start if it
/// never recorded an end. Explicit unpack requests ignore this
pub fn retry_at(prev: &State, next_code: &str) -> Option<u64> {
    if prev.status == "ok" || prev.attempts == 0 || prev.version_code != next_code {
        return None;
    }
    let delay = RETRY_BASE_SECS
        .saturating_mul(1u64 << (prev.attempts - 1).min(16))
        .min(RETRY_MAX_SECS);
    Some(prev.finished_at.max(prev.started_at).saturating_add(delay))
}

/// Returns Ok(None) when a check finds the unpacked version is current. The caller holds the run lock
pub fn unpack(
    paths: &Paths,
    apk: Apk,
    check: bool,
    _lock: &RunLock,
) -> Result<Option<State>, String> {
    let prev = read_state(&paths.root);
    if should_skip(check, &prev.version_code, &prev.status, &apk.version_code) {
        return Ok(None);
    }
    let retrying = prev.version_code == apk.version_code && prev.status != "ok";
    let mut state = State {
        status: "running".into(),
        version_code: apk.version_code.clone(),
        version_name: apk.version_name.clone(),
        attempts: if retrying {
            prev.attempts.saturating_add(1)
        } else {
            1
        },
        started_at: now_secs(),
        ..State::default()
    };
    write_state(paths, &state)?;
    if prev.status == "ok" && prev.version_code != apk.version_code {
        archive_apk(paths, &prev.version_name);
    }
    let result = catch_unwind(AssertUnwindSafe(|| run(paths, &apk, &mut state)))
        .unwrap_or_else(|panic| Err(format!("panicked: {}", par::panic_message(&*panic))));
    state.finished_at = now_secs();
    state.updated_at = state.finished_at.to_string();
    match result {
        Ok(()) => {
            state.status = "ok".into();
            state.stage = "done".into();
            state.attempts = 0;
            state.error.clear();
            write_state(paths, &state)?;
            Ok(Some(state))
        }
        Err(err) => {
            state.status = "error".into();
            state.error = err.clone();
            let _ = write_state(paths, &state);
            Err(err)
        }
    }
}

fn run(paths: &Paths, apk: &Apk, state: &mut State) -> Result<(), String> {
    let started = Instant::now();
    let enter = |state: &mut State, stage: &str| {
        eprintln!("[{:>6.1}s] {stage}", started.elapsed().as_secs_f64());
        state.stage = stage.into();
        write_state(paths, state)
    };
    enter(state, "preflight")?;
    // Publishing is the last step; find missing credentials before spending minutes on the APK
    publish::check_env()?;
    enter(state, "download")?;
    let apk_path = paths.root.join("apk/game.apk");
    download(
        &apk.download_url,
        &apk_path,
        &apk.version_code,
        Duration::from_secs(5),
    )?;
    enter(state, "extract")?;
    let output = paths.output();
    let exported = extract::export(&apk_path, &output, &apk.version_code, &apk.version_name)?;
    state.jackets = exported.jackets as u64;
    state.pv = exported.clips as u64;
    state.collection = exported.collection as u64;
    state.collection_art = exported.arts as u64;
    state.warnings = exported.warnings;
    state.songs = write_info_tsv(&output)? as u64;
    enter(state, "pez")?;
    state.pez = pack_all(&output)? as u64;
    enter(state, "publish")?;
    let (_, warnings) = publish(&output, &paths.manifest(), false)?;
    state.warnings.extend(warnings);
    enter(state, "mirror")?;
    mirror(&output)?;
    eprintln!("[{:>6.1}s] done", started.elapsed().as_secs_f64());
    Ok(())
}

/// Keep the APK a new version is about to replace: `<archive>/Phigros_<version>.apk`, a hard link when it can be.
/// What the game drops then stays recoverable (removed songs and charts are taken from these). The archive is
/// UNPACK_APK_ARCHIVE, else `apk/archive` under the root. A failure only warns: it must not hold up the new version
fn archive_apk(paths: &Paths, version_name: &str) {
    let name: String = version_name
        .chars()
        .filter(|c| c.is_ascii_alphanumeric() || *c == '.' || *c == '-')
        .collect();
    let apk = paths.root.join("apk/game.apk");
    if name.is_empty() || !apk.is_file() {
        return;
    }
    let dir = std::env::var("UNPACK_APK_ARCHIVE")
        .ok()
        .filter(|d| !d.trim().is_empty())
        .map(PathBuf::from)
        .unwrap_or_else(|| paths.root.join("apk/archive"));
    let dest = dir.join(format!("Phigros_{name}.apk"));
    if dest.exists() {
        return;
    }
    let kept = fs::create_dir_all(&dir)
        .and_then(|()| fs::hard_link(&apk, &dest).or_else(|_| fs::copy(&apk, &dest).map(|_| ())));
    match kept {
        Ok(()) => eprintln!("archive: kept {version_name} as {}", dest.display()),
        Err(err) => eprintln!("archive: {version_name} not kept: {err}"),
    }
}

pub fn now_secs() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

fn is_zip(path: &Path) -> bool {
    File::open(path)
        .ok()
        .and_then(|f| zip::ZipArchive::new(f).ok())
        .is_some()
}

/// Download into `game.apk.part`, resuming after drops and stalls, and rename it into place only once
/// it opens as a zip. `part.txt` records which version the partial file belongs to. Retries wait
/// `pause` doubled per attempt, at most a minute
fn download(url: &str, dest: &Path, version_code: &str, pause: Duration) -> Result<(), String> {
    if let Some(parent) = dest.parent() {
        fs::create_dir_all(parent).map_err(|e| e.to_string())?;
    }
    let stamp = dest.with_file_name("version.txt");
    let stamped = |path: &Path| {
        fs::read_to_string(path)
            .ok()
            .is_some_and(|s| s.trim() == version_code)
    };
    if dest.is_file() && stamped(&stamp) && is_zip(dest) {
        return Ok(());
    }
    let part = dest.with_file_name("game.apk.part");
    let part_stamp = dest.with_file_name("part.txt");
    if !stamped(&part_stamp) {
        let _ = fs::remove_file(&part);
        fs::write(&part_stamp, version_code).map_err(|e| e.to_string())?;
    }
    let mut last_error = String::new();
    for attempt in 0..DOWNLOAD_ATTEMPTS {
        if is_zip(&part) {
            break;
        }
        if attempt > 0 {
            std::thread::sleep((pause * (1 << attempt)).min(Duration::from_secs(60)));
        }
        // Abort when slower than 64 KiB/s for a minute; the next attempt resumes with -C -
        let out = Command::new("curl")
            .args([
                "-fsSL",
                "--connect-timeout",
                "30",
                "--speed-limit",
                "65536",
                "--speed-time",
                "60",
            ])
            .args(["-C", "-", "-w", "%{http_code}", "-o"])
            .arg(&part)
            .arg(url)
            .output()
            .map_err(|e| e.to_string())?;
        let http = String::from_utf8_lossy(&out.stdout).trim().to_string();
        if out.status.success() && is_zip(&part) {
            break;
        }
        // 416: nothing left to resume from (the partial file is complete but not a zip, or longer than
        // the file). 33: the server ignored the range request. A bad body on 200 is also unusable
        if http == "416" || out.status.code() == Some(33) || out.status.success() {
            let _ = fs::remove_file(&part);
        }
        last_error = format!(
            "curl download exited {} (http {http}): {}",
            out.status,
            String::from_utf8_lossy(&out.stderr).trim()
        );
        eprintln!("download attempt {} failed: {last_error}", attempt + 1);
    }
    if !is_zip(&part) {
        return Err(if last_error.is_empty() {
            "downloaded apk is not a zip".into()
        } else {
            last_error
        });
    }
    fs::rename(&part, dest).map_err(|e| e.to_string())?;
    fs::write(&stamp, version_code).map_err(|e| e.to_string())?;
    let _ = fs::remove_file(&part_stamp);
    Ok(())
}

fn mirror(output: &Path) -> Result<(), String> {
    let Some(dest) = std::env::var("UNPACK_MIRROR")
        .ok()
        .filter(|s| !s.is_empty())
    else {
        return Ok(());
    };
    // Unchanged outputs keep their mtime, so rsync's size+mtime check skips them
    let status = Command::new("rsync")
        .args(["-a", "--delete"])
        .arg(format!("{}/", output.display()))
        .arg(dest)
        .status()
        .map_err(|e| e.to_string())?;
    if status.success() {
        Ok(())
    } else {
        Err(format!("mirror rsync exited {status}"))
    }
}

#[cfg(test)]
mod tests {

    #[test]
    fn archive_keeps_the_outgoing_apk_once() {
        let root = std::env::temp_dir().join(format!("phi-archive-test-{}", std::process::id()));
        let _ = fs::remove_dir_all(&root);
        fs::create_dir_all(root.join("apk")).unwrap();
        fs::write(root.join("apk/game.apk"), b"4.0.1 bytes").unwrap();
        let paths = Paths { root: root.clone() };
        archive_apk(&paths, "4.0.1");
        let kept = root.join("apk/archive/Phigros_4.0.1.apk");
        assert_eq!(fs::read(&kept).unwrap(), b"4.0.1 bytes");
        // download() renames the new file into place, so the hard link keeps the old bytes
        fs::write(root.join("apk/game.apk.part"), b"newer").unwrap();
        fs::rename(root.join("apk/game.apk.part"), root.join("apk/game.apk")).unwrap();
        archive_apk(&paths, "4.0.1");
        assert_eq!(
            fs::read(&kept).unwrap(),
            b"4.0.1 bytes",
            "an archived version is never overwritten"
        );
        archive_apk(&paths, "../x");
        assert!(root.join("apk/archive/Phigros_..x.apk").exists() || !root.join("x").exists());
        let _ = fs::remove_dir_all(&root);
    }
    use super::*;

    #[test]
    fn backs_off_exponentially_on_repeated_failures_of_one_version() {
        let mut prev = State {
            status: "error".into(),
            version_code: "140".into(),
            attempts: 1,
            started_at: 1_000_000,
            ..State::default()
        };
        assert_eq!(retry_at(&prev, "140"), Some(1_000_000 + 600));
        prev.attempts = 3;
        assert_eq!(retry_at(&prev, "140"), Some(1_000_000 + 2400));
        prev.attempts = 40;
        assert_eq!(retry_at(&prev, "140"), Some(1_000_000 + RETRY_MAX_SECS));
        // A long run that failed late backs off from when it failed, not from when it started
        prev.attempts = 1;
        prev.finished_at = 1_000_000 + 1500;
        assert_eq!(retry_at(&prev, "140"), Some(1_000_000 + 1500 + 600));
        // A run that died mid-way counts too (from its start); a new version or a success clears it
        prev.status = "running".into();
        prev.started_at = 2_000_000;
        prev.finished_at = 0;
        assert_eq!(retry_at(&prev, "140"), Some(2_000_000 + 600));
        assert_eq!(retry_at(&prev, "141"), None);
        prev.status = "ok".into();
        assert_eq!(retry_at(&prev, "140"), None);
    }

    #[test]
    fn reads_state_written_before_the_new_fields_existed() {
        let old = r#"{"status":"error","version_code":"140","version_name":"3.18","songs":1,"pez":2,"jackets":3,"error":"x","updated_at":"5"}"#;
        let state: State = serde_json::from_str(old).unwrap();
        assert_eq!(
            (state.status.as_str(), state.attempts, state.started_at),
            ("error", 0, 0)
        );
        assert_eq!(retry_at(&state, "140"), None);
        assert!(state.warnings.is_empty());
    }

    #[test]
    fn warnings_are_saved_with_the_state_and_start_empty_each_run() {
        let state = State {
            status: "ok".into(),
            warnings: vec!["pv skipped: no video clips".into()],
            ..State::default()
        };
        let saved = serde_json::to_string(&state).unwrap();
        assert!(saved.contains("\"warnings\":[\"pv skipped: no video clips\"]"));
        let back: State = serde_json::from_str(&saved).unwrap();
        assert_eq!(back.warnings, state.warnings);
        assert!(State::default().warnings.is_empty());
    }

    /// Serves `body` over plain HTTP on 127.0.0.1, one connection per request. The first request
    /// without a Range header is cut off halfway; ranged requests get 206, or 416 past the end
    /// Returns the base URL and the Range start of every request seen (None = no Range)
    fn flaky_server(
        body: Vec<u8>,
    ) -> (String, std::sync::Arc<std::sync::Mutex<Vec<Option<usize>>>>) {
        use std::io::{BufRead, BufReader, Write};
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let url = format!("http://{}/game.apk", listener.local_addr().unwrap());
        let seen = std::sync::Arc::new(std::sync::Mutex::new(Vec::new()));
        let log = seen.clone();
        std::thread::spawn(move || {
            let mut cut = true;
            for stream in listener.incoming() {
                let mut stream = stream.unwrap();
                let mut range = None;
                let mut reader = BufReader::new(stream.try_clone().unwrap());
                loop {
                    let mut line = String::new();
                    if reader.read_line(&mut line).unwrap() == 0 || line == "\r\n" {
                        break;
                    }
                    if let Some(v) = line.to_ascii_lowercase().strip_prefix("range: bytes=") {
                        range = v.trim().trim_end_matches('-').parse::<usize>().ok();
                    }
                }
                log.lock().unwrap().push(range);
                let head = match range {
                    Some(from) if from >= body.len() => {
                        format!("HTTP/1.1 416 Range Not Satisfiable\r\nContent-Range: bytes */{}\r\nContent-Length: 0\r\nConnection: close\r\n\r\n", body.len())
                    }
                    Some(from) => format!(
                        "HTTP/1.1 206 Partial Content\r\nContent-Range: bytes {from}-{}/{}\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
                        body.len() - 1,
                        body.len(),
                        body.len() - from
                    ),
                    None => format!("HTTP/1.1 200 OK\r\nContent-Length: {}\r\nConnection: close\r\n\r\n", body.len()),
                };
                let _ = stream.write_all(head.as_bytes());
                let slice = match range {
                    Some(from) if from >= body.len() => &body[..0],
                    Some(from) => &body[from..],
                    None if cut => {
                        cut = false;
                        &body[..body.len() / 2]
                    }
                    None => &body[..],
                };
                let _ = stream.write_all(slice);
            }
        });
        (url, seen)
    }

    #[test]
    fn download_resumes_after_a_drop_and_discards_unusable_partials() {
        if Command::new("curl").arg("--version").output().is_err() {
            return;
        }
        let mut zip = zip::ZipWriter::new(std::io::Cursor::new(Vec::new()));
        zip.start_file("payload.bin", zip::write::FileOptions::default())
            .unwrap();
        std::io::Write::write_all(&mut zip, &vec![7u8; 300_000]).unwrap();
        let payload = zip.finish().unwrap().into_inner();
        let (url, seen) = flaky_server(payload.clone());
        let dir = std::env::temp_dir().join(format!("phi-download-test-{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();
        let dest = dir.join("game.apk");
        // A partial file from another version must not be resumed
        fs::write(dir.join("game.apk.part"), b"stale bytes of version 139").unwrap();
        fs::write(dir.join("part.txt"), "139").unwrap();
        download(&url, &dest, "140", Duration::ZERO).unwrap();
        assert_eq!(fs::read(&dest).unwrap(), payload);
        assert_eq!(fs::read_to_string(dir.join("version.txt")).unwrap(), "140");
        assert!(!dir.join("game.apk.part").exists() && !dir.join("part.txt").exists());
        // Cut off halfway, then resumed from exactly where it stopped
        assert_eq!(*seen.lock().unwrap(), vec![None, Some(payload.len() / 2)]);
        // Same version again: nothing is fetched
        download(&url, &dest, "140", Duration::ZERO).unwrap();
        assert_eq!(seen.lock().unwrap().len(), 2);
        // A full-length partial that is not a zip gets 416 on resume and is fetched again from zero
        fs::write(dir.join("game.apk.part"), vec![0u8; payload.len()]).unwrap();
        fs::write(dir.join("part.txt"), "141").unwrap();
        download(&url, &dest, "141", Duration::ZERO).unwrap();
        assert_eq!(fs::read(&dest).unwrap(), payload);
        assert_eq!(seen.lock().unwrap()[2..], [Some(payload.len()), None]);
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn the_run_lock_excludes_a_second_holder() {
        let root = std::env::temp_dir().join(format!("phi-lock-test-{}", std::process::id()));
        let paths = Paths { root: root.clone() };
        let held = paths.try_lock().unwrap();
        assert!(held.is_some());
        assert!(paths.try_lock().unwrap().is_none());
        drop(held);
        // A process another test is spawning right now can hold a copy of the descriptor until its exec
        // closes it, which keeps the lock a moment longer
        let again = (0..200).find_map(|_| {
            let lock = paths.try_lock().unwrap();
            if lock.is_none() {
                std::thread::sleep(Duration::from_millis(5));
            }
            lock
        });
        assert!(again.is_some());
        let _ = fs::remove_dir_all(root);
    }
}
