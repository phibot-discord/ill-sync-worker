use crate::extract;
use crate::files::write_if_changed;
use crate::layout::song_id;
use crate::par;
use md5::Md5;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use sha2::{Digest, Sha256};
use std::collections::{BTreeMap, BTreeSet, HashMap};
use std::fs::{self, File};
use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Mutex;
use std::time::{Duration, Instant};

const LEVELS: [&str; 4] = ["EZ", "HD", "IN", "AT"];
/// Concurrent curl uploads. Upload speed, not CPU, bounds this
const UPLOAD_WORKERS: usize = 8;
/// Cloudflare's API allows 1,200 requests per 5 minutes per user, shared with the web UI and bot
const DEFAULT_RPS: f64 = 3.0;
const KV_KEY: &str = "phi:infoFile";
const API: &str = "https://api.cloudflare.com/client/v4";
/// R2's list API returns at most 1,000 keys a page; no prefix here holds more than a few hundred
const LIST_PAGE_CAP: usize = 50;
/// How often put_all saves published.json while uploading, so a crash loses little
const MANIFEST_SAVE_EVERY: Duration = Duration::from_secs(30);

struct Creds {
    account: String,
    token: String,
    bucket: String,
    kv: String,
}

impl Creds {
    fn from_env() -> Result<Self, String> {
        let account = std::env::var("CLOUDFLARE_ACCOUNT_ID").unwrap_or_default();
        let token = std::env::var("CLOUDFLARE_API_TOKEN").unwrap_or_default();
        let bucket =
            std::env::var("CLOUDFLARE_R2_BUCKET").unwrap_or_else(|_| "phi-web-assets".into());
        let kv = std::env::var("CLOUDFLARE_KV_NAMESPACE_ID").unwrap_or_default();
        if account.is_empty() || token.is_empty() || kv.is_empty() {
            return Err(
                "R2 upload needs CLOUDFLARE_ACCOUNT_ID, CLOUDFLARE_API_TOKEN, and CLOUDFLARE_KV_NAMESPACE_ID".into(),
            );
        }
        Ok(Self {
            account,
            token,
            bucket,
            kv,
        })
    }
}

/// Fail fast, before the download and extract, when publish could not run: missing settings, no curl,
/// or a token Cloudflare does not accept. User tokens and account-owned tokens have separate verify
/// endpoints; either answering "active" is enough
pub fn check_env() -> Result<(), String> {
    let creds = Creds::from_env()?;
    let mut last = String::new();
    for url in [
        format!("{API}/user/tokens/verify"),
        format!("{API}/accounts/{}/tokens/verify", creds.account),
    ] {
        match api_get(&creds, &url) {
            Ok(body) => {
                let reply: Value = serde_json::from_slice(&body).unwrap_or_default();
                let status = reply["result"]["status"].as_str().unwrap_or_default();
                if status == "active" {
                    return Ok(());
                }
                last = format!("token status {status:?}");
            }
            Err(err) => last = err,
        }
    }
    Err(format!("Cloudflare API token check failed: {last}"))
}

/// What this daemon last uploaded for each key. R2's own listing decides what is skipped; this record
/// only stands in for an etag or content type the listing leaves out
#[derive(Default, Serialize, Deserialize)]
struct Manifest {
    #[serde(default)]
    account: String,
    #[serde(default)]
    bucket: String,
    #[serde(default)]
    objects: BTreeMap<String, Published>,
}

#[derive(Clone, PartialEq, Serialize, Deserialize)]
struct Published {
    /// R2's etag for a single-part upload is the MD5 of the body
    md5: String,
    size: u64,
    #[serde(rename = "type")]
    content_type: String,
}

impl Published {
    fn of_file(path: &Path, content_type: &str) -> Result<Self, String> {
        let bytes = fs::read(path).map_err(|e| format!("{}: {e}", path.display()))?;
        Ok(Self {
            md5: format!("{:x}", Md5::digest(&bytes)),
            size: bytes.len() as u64,
            content_type: content_type.to_string(),
        })
    }
}

impl Manifest {
    /// A manifest written for another account or bucket (or in an older format) says nothing here
    fn load(path: &Path, creds: &Creds) -> Self {
        let loaded: Option<Manifest> = fs::read(path)
            .ok()
            .and_then(|b| serde_json::from_slice(&b).ok());
        match loaded {
            Some(m) if m.account == creds.account && m.bucket == creds.bucket => m,
            _ => Manifest {
                account: creds.account.clone(),
                bucket: creds.bucket.clone(),
                ..Manifest::default()
            },
        }
    }

    fn save(&self, path: &Path) -> Result<(), String> {
        let body = serde_json::to_vec(self).map_err(|e| e.to_string())?;
        let tmp = path.with_extension("json.tmp");
        let mut file = File::create(&tmp).map_err(|e| format!("{}: {e}", tmp.display()))?;
        file.write_all(&body)
            .and_then(|_| file.sync_all())
            .map_err(|e| format!("{}: {e}", tmp.display()))?;
        fs::rename(&tmp, path).map_err(|e| format!("{}: {e}", path.display()))?;
        // Make the rename itself durable. Best effort: not every platform can sync a directory
        if let Some(dir) = path.parent().filter(|d| !d.as_os_str().is_empty()) {
            let _ = File::open(dir).and_then(|d| d.sync_all());
        }
        Ok(())
    }
}

/// An object as R2's list API reports it
#[derive(Debug, PartialEq)]
struct Remote {
    size: u64,
    etag: Option<String>,
    content_type: Option<String>,
}

#[derive(Deserialize)]
struct ListPage {
    #[serde(default)]
    result: Vec<Listed>,
    #[serde(default)]
    result_info: Option<ListInfo>,
}

#[derive(Deserialize)]
struct Listed {
    key: String,
    #[serde(default)]
    size: Value,
    #[serde(default)]
    etag: Option<String>,
    #[serde(default, alias = "httpMetadata")]
    http_metadata: Option<HttpMetadata>,
}

#[derive(Deserialize)]
struct HttpMetadata {
    #[serde(default, rename = "contentType", alias = "content_type")]
    content_type: Option<String>,
}

#[derive(Deserialize)]
struct ListInfo {
    #[serde(default)]
    cursor: Option<String>,
    #[serde(default)]
    is_truncated: Value,
}

impl ListPage {
    fn next_cursor(&self) -> Option<String> {
        let info = self.result_info.as_ref()?;
        let truncated = info.is_truncated.as_bool() == Some(true) || info.is_truncated == "true";
        info.cursor.clone().filter(|c| truncated && !c.is_empty())
    }
}

impl From<Listed> for Remote {
    fn from(obj: Listed) -> Self {
        let size = obj
            .size
            .as_u64()
            .or_else(|| obj.size.as_str().and_then(|s| s.parse().ok()));
        Remote {
            // An unknown size never matches, so the object is uploaded again
            size: size.unwrap_or(u64::MAX),
            etag: obj
                .etag
                .map(|e| e.trim_matches('"').to_ascii_lowercase())
                .filter(|e| !e.is_empty()),
            content_type: obj.http_metadata.and_then(|m| m.content_type),
        }
    }
}

/// Whether R2 already holds `local` under a key. Size and etag come from R2's listing; when the
/// listing has no etag or content type, published.json's record of the last upload stands in
fn up_to_date(local: &Published, remote: Option<&Remote>, recorded: Option<&Published>) -> bool {
    let Some(remote) = remote else {
        return false;
    };
    let same_bytes = match &remote.etag {
        Some(etag) => *etag == local.md5,
        None => recorded.is_some_and(|r| r.md5 == local.md5),
    };
    let same_type = remote
        .content_type
        .as_deref()
        .or(recorded.map(|r| r.content_type.as_str()))
        .is_none_or(|t| same_mime(t, &local.content_type));
    remote.size == local.size && same_bytes && same_type
}

/// `text/plain; charset=utf-8` and `text/plain;charset=UTF-8` are the same type
fn same_mime(a: &str, b: &str) -> bool {
    let norm = |s: &str| {
        s.chars()
            .filter(|c| !c.is_whitespace())
            .collect::<String>()
            .to_ascii_lowercase()
    };
    norm(a) == norm(b)
}

struct Upload {
    path: PathBuf,
    key: String,
    mime: &'static str,
}

/// Upload APK-derived jackets, avatars, packs, music and catalog files, then the KV catalog. Community data
/// stays on the sync Worker. R2 is listed first and every object whose size and etag (MD5) already
/// match is skipped, so objects another writer deleted or overwrote are sent again; KV is rewritten
/// unless it already holds this info.csv. If the listing fails, everything is uploaded. Set
/// UNPACK_PUBLISH_ALL=1 to send everything anyway. `rebuild_notes` recomputes notesInfo.json from the
/// charts on disk (for `--publish`; extract writes it after a successful decode). Returns the number of
/// objects uploaded
pub fn publish(output: &Path, manifest_path: &Path, rebuild_notes: bool) -> Result<usize, String> {
    let creds = Creds::from_env()?;
    let mut assets = Vec::new();
    for (dir, key_prefix) in [
        ("original_ill/ill", "original_ill/ill"),
        ("original_ill/illLow", "original_ill/illLow"),
        ("original_ill/illBlur", "original_ill/illBlur"),
        ("avatar", "html/avatar"),
        ("phira/EZ", "phira/EZ"),
        ("phira/HD", "phira/HD"),
        ("phira/IN", "phira/IN"),
        ("phira/AT", "phira/AT"),
        ("music", "music"),
    ] {
        let mime = match key_prefix {
            "music" => "audio/ogg",
            p if p.starts_with("phira/") => "application/octet-stream",
            _ => "image/png",
        };
        collect(output.join(dir), key_prefix, mime, &mut assets);
    }
    let info_dir = output.join("info");
    fs::create_dir_all(&info_dir).map_err(|e| e.to_string())?;
    let csv = info_csv(&info_dir.join("songs.json"))?;
    let notes_path = info_dir.join("notesInfo.json");
    if rebuild_notes || !notes_path.is_file() {
        write_if_changed(
            &notes_path,
            extract::notes_info_from_charts(output)?.as_bytes(),
        )?;
    }
    let tips = tips_text(&info_dir.join("tips.json"))?;
    let avatars = avatar_text(&info_dir.join("avatars.json"))?;
    write_if_changed(&info_dir.join("info.csv"), csv.as_bytes())?;
    write_if_changed(&info_dir.join("tips.txt"), tips.as_bytes())?;
    write_if_changed(&info_dir.join("avatar.txt"), avatars.as_bytes())?;
    let info = [
        ("info.csv", "text/tab-separated-values; charset=utf-8"),
        ("notesInfo.json", "application/json"),
        ("tips.txt", "text/plain; charset=utf-8"),
        ("avatar.txt", "text/plain; charset=utf-8"),
    ]
    .map(|(name, mime)| Upload {
        path: info_dir.join(name),
        key: format!("info/{name}"),
        mime,
    });

    let force = std::env::var("UNPACK_PUBLISH_ALL").is_ok_and(|v| v == "1" || v == "true");
    let pacer = Pacer::from_env();
    let remote = if force {
        None
    } else {
        let prefixes: BTreeSet<String> = assets
            .iter()
            .chain(&info)
            .filter_map(|u| u.key.rsplit_once('/').map(|(dir, _)| format!("{dir}/")))
            .collect();
        match list_remote(&creds, &prefixes, &pacer) {
            Ok(found) => Some(found),
            Err(err) => {
                eprintln!("publish: {err}; uploading every object");
                None
            }
        }
    };
    let manifest = Mutex::new(Manifest::load(manifest_path, &creds));
    let uploader = Uploader {
        creds: &creds,
        remote: remote.as_ref(),
        manifest: &manifest,
        manifest_path,
        saved_at: Mutex::new(Instant::now()),
        pacer: &pacer,
    };
    // Assets first, then the catalog files that point at them, then KV
    let mut sent = 0;
    let mut skipped = 0;
    for batch in [&assets[..], &info[..]] {
        let result = uploader.put_all(batch);
        manifest
            .lock()
            .map_err(|e| e.to_string())?
            .save(manifest_path)?;
        let (n, s) = result?;
        sent += n;
        skipped += s;
    }
    let csv_sha = format!("{:x}", Sha256::digest(csv.as_bytes()));
    pacer.wait();
    if force || !kv_holds(&creds, &csv, &csv_sha) {
        pacer.wait();
        put_kv(&creds, &csv, &csv_sha)?;
        eprintln!("publish: wrote KV {KV_KEY}");
    }
    eprintln!("publish: {sent} uploaded, {skipped} already in R2");
    Ok(sent)
}

/// Every object R2 holds under `prefixes`: key -> size, etag, content type
fn list_remote(
    creds: &Creds,
    prefixes: &BTreeSet<String>,
    pacer: &Pacer,
) -> Result<HashMap<String, Remote>, String> {
    let mut found = HashMap::new();
    for prefix in prefixes {
        let mut cursor: Option<String> = None;
        for page in 0.. {
            if page == LIST_PAGE_CAP {
                return Err(format!("r2 list {prefix}: more than {LIST_PAGE_CAP} pages"));
            }
            let mut url = format!(
                "{API}/accounts/{}/r2/buckets/{}/objects?prefix={}&per_page=1000",
                creds.account,
                encode(&creds.bucket),
                encode(prefix),
            );
            if let Some(cursor) = &cursor {
                url.push_str(&format!("&cursor={}", encode(cursor)));
            }
            pacer.wait();
            let body = api_get(creds, &url).map_err(|e| format!("r2 list {prefix}: {e}"))?;
            let page: ListPage =
                serde_json::from_slice(&body).map_err(|e| format!("r2 list {prefix}: {e}"))?;
            cursor = page.next_cursor();
            found.extend(
                page.result
                    .into_iter()
                    .map(|obj| (obj.key.clone(), obj.into())),
            );
            if cursor.is_none() {
                break;
            }
        }
    }
    Ok(found)
}

/// Whether KV `phi:infoFile` already holds this info.csv. Any failure to read it means "no"
fn kv_holds(creds: &Creds, csv: &str, sha: &str) -> bool {
    let Ok(body) = api_get(creds, &kv_url(creds)) else {
        return false;
    };
    let Ok(value) = serde_json::from_slice::<Value>(&body) else {
        return false;
    };
    value["sha"].as_str() == Some(sha) && value["csv"].as_str() == Some(csv)
}

fn collect(dir: PathBuf, key_prefix: &str, mime: &'static str, out: &mut Vec<Upload>) {
    let Ok(entries) = fs::read_dir(&dir) else {
        return;
    };
    let mut found = Vec::new();
    for ent in entries.flatten() {
        let path = ent.path();
        if !path.is_file() {
            continue;
        }
        // Dotfiles are temporaries (link_or_copy's `.<name>.link`), never outputs
        let Some(name) = path
            .file_name()
            .and_then(|n| n.to_str())
            .filter(|n| !n.starts_with('.'))
            .map(|n| n.to_string())
        else {
            continue;
        };
        found.push(Upload {
            path,
            key: format!("{key_prefix}/{name}"),
            mime,
        });
    }
    found.sort_by(|a, b| a.key.cmp(&b.key));
    out.extend(found);
}

fn info_csv(songs_path: &Path) -> Result<String, String> {
    let songs: Value =
        serde_json::from_reader(fs::File::open(songs_path).map_err(|e| e.to_string())?)
            .map_err(|e| e.to_string())?;
    let songs = songs.as_array().ok_or("songs.json is not a list")?;
    let mut rows =
        vec!["id\tsong\tcomposer\tillustrator\tEZC\tHDC\tINC\tATC\tEZ\tHD\tIN\tAT".to_string()];
    for song in songs {
        let id = text(&song["id"]);
        let id = song_id(&id);
        if id.is_empty() {
            continue;
        }
        let mut charters = Vec::new();
        let mut diffs = Vec::new();
        for level in LEVELS {
            let entry = &song["levels"][level];
            charters.push(cell(&text(&entry["charter"])));
            diffs.push(cell(&fmt_diff(&entry["difficulty"])));
        }
        rows.push(format!(
            "{}\t{}\t{}\t{}\t{}\t{}",
            cell(id),
            cell(&lang(&song["name"])),
            cell(&text(&song["composer"])),
            cell(&text(&song["illustrator"])),
            charters.join("\t"),
            diffs.join("\t"),
        ));
    }
    if rows.len() < 2 {
        return Err("songs.json produced no catalog rows".into());
    }
    Ok(format!("{}\n", rows.join("\n")))
}

fn tips_text(path: &Path) -> Result<String, String> {
    let tips: Value = serde_json::from_reader(fs::File::open(path).map_err(|e| e.to_string())?)
        .map_err(|e| e.to_string())?;
    let list = tips
        .get("zh_cn")
        .and_then(|v| v.as_array())
        .ok_or("tips.json has no zh_cn list")?;
    let mut lines = Vec::new();
    for tip in list {
        if let Some(s) = tip.as_str() {
            lines.push(s.to_string());
        }
    }
    Ok(format!("{}\n", lines.join("\n")))
}

fn avatar_text(path: &Path) -> Result<String, String> {
    let avatars: Value = serde_json::from_reader(fs::File::open(path).map_err(|e| e.to_string())?)
        .map_err(|e| e.to_string())?;
    let list = avatars.as_array().ok_or("avatars.json is not a list")?;
    let mut lines = Vec::new();
    for avatar in list {
        let name = text(&avatar["name"]);
        if !name.is_empty() {
            lines.push(name);
        }
    }
    Ok(format!("{}\n", lines.join("\n")))
}

/// Spaces request starts `gap` apart across all upload workers. UNPACK_PUBLISH_RPS overrides the
/// rate; 0 turns pacing off
struct Pacer {
    next: Mutex<Instant>,
    gap: Duration,
}

impl Pacer {
    fn from_env() -> Self {
        let rps = std::env::var("UNPACK_PUBLISH_RPS")
            .ok()
            .and_then(|s| s.trim().parse::<f64>().ok())
            .unwrap_or(DEFAULT_RPS);
        let gap = if rps > 0.0 {
            Duration::from_secs_f64(1.0 / rps)
        } else {
            Duration::ZERO
        };
        Self {
            next: Mutex::new(Instant::now()),
            gap,
        }
    }

    fn wait(&self) {
        if self.gap.is_zero() {
            return;
        }
        let now = Instant::now();
        let at = {
            let mut next = self.next.lock().unwrap_or_else(|e| e.into_inner());
            let at = (*next).max(now);
            *next = at + self.gap;
            at
        };
        std::thread::sleep(at - now);
    }
}

/// What one publish shares across its upload workers
struct Uploader<'a> {
    creds: &'a Creds,
    /// R2's listing, or None when it failed (or UNPACK_PUBLISH_ALL is set) and everything is sent
    remote: Option<&'a HashMap<String, Remote>>,
    manifest: &'a Mutex<Manifest>,
    manifest_path: &'a Path,
    saved_at: Mutex<Instant>,
    pacer: &'a Pacer,
}

impl Uploader<'_> {
    /// Upload `files` from a shared queue. Returns (uploaded, already in R2). Successful uploads are
    /// recorded in the manifest even when another file fails
    fn put_all(&self, files: &[Upload]) -> Result<(usize, usize), String> {
        let sent = AtomicUsize::new(0);
        let skipped = AtomicUsize::new(0);
        par::for_each(
            files,
            UPLOAD_WORKERS,
            || Ok(()),
            |_, _, file| {
                let local = Published::of_file(&file.path, file.mime)?;
                let current = match self.remote {
                    Some(remote) => {
                        let manifest = self.manifest.lock().map_err(|e| e.to_string())?;
                        up_to_date(
                            &local,
                            remote.get(&file.key),
                            manifest.objects.get(&file.key),
                        )
                    }
                    None => false,
                };
                if current {
                    skipped.fetch_add(1, Ordering::Relaxed);
                } else {
                    self.pacer.wait();
                    put_file(self.creds, &file.path, &file.key, file.mime)
                        .map_err(|e| format!("{}: {e}", file.key))?;
                    sent.fetch_add(1, Ordering::Relaxed);
                }
                let mut manifest = self.manifest.lock().map_err(|e| e.to_string())?;
                manifest.objects.insert(file.key.clone(), local);
                let mut saved_at = self.saved_at.lock().map_err(|e| e.to_string())?;
                if saved_at.elapsed() >= MANIFEST_SAVE_EVERY {
                    manifest.save(self.manifest_path)?;
                    *saved_at = Instant::now();
                }
                Ok(())
            },
        )?;
        Ok((sent.into_inner(), skipped.into_inner()))
    }
}

/// GET from the Cloudflare API; curl retries timeouts, 429 and 5xx. Returns the body
fn api_get(creds: &Creds, url: &str) -> Result<Vec<u8>, String> {
    let out = Command::new("curl")
        .args([
            "-fsS",
            "--connect-timeout",
            "15",
            "--max-time",
            "60",
            "--retry",
            "3",
        ])
        .args(["-H", &format!("Authorization: Bearer {}", creds.token)])
        .arg(url)
        .stdin(Stdio::null())
        .output()
        .map_err(|e| format!("curl: {e}"))?;
    if out.status.success() {
        Ok(out.stdout)
    } else {
        Err(format!(
            "curl exited {}: {}",
            out.status,
            String::from_utf8_lossy(&out.stderr).trim()
        ))
    }
}

/// curl retries 429/5xx with exponential backoff (honouring Retry-After); timeouts keep a stalled
/// connection from holding the job forever
const CURL_PUT: [&str; 10] = [
    "-fsS",
    "--connect-timeout",
    "15",
    "--max-time",
    "600",
    "--retry",
    "5",
    "--retry-all-errors",
    "--retry-max-time",
    "900",
];

fn put_file(creds: &Creds, path: &Path, key: &str, mime: &str) -> Result<(), String> {
    let url = format!(
        "{API}/accounts/{}/r2/buckets/{}/objects/{}",
        creds.account,
        encode(&creds.bucket),
        key.split('/').map(encode).collect::<Vec<_>>().join("/"),
    );
    let status = Command::new("curl")
        .args(CURL_PUT)
        .args([
            "-X",
            "PUT",
            "-H",
            &format!("Authorization: Bearer {}", creds.token),
        ])
        .args(["-H", &format!("Content-Type: {mime}")])
        .args(
            disposition(key)
                .map(|d| ["-H".to_string(), format!("Content-Disposition: {d}")])
                .into_iter()
                .flatten(),
        )
        .args(["--data-binary", &format!("@{}", path.display()), &url])
        .status()
        .map_err(|e| e.to_string())?;
    if status.success() {
        Ok(())
    } else {
        Err(format!("r2 put exited {status}"))
    }
}

/// Music is saved as a file from the public domain, where a cross-origin `<a download>` is ignored;
/// `<audio>` plays it all the same
fn disposition(key: &str) -> Option<String> {
    let name = key.strip_prefix("music/")?;
    let ascii: String = name
        .chars()
        .map(|c| {
            if c == ' ' || (c.is_ascii_graphic() && c != '"' && c != '\\') {
                c
            } else {
                '_'
            }
        })
        .collect();
    Some(format!(
        "attachment; filename=\"{ascii}\"; filename*=UTF-8''{}",
        encode(name)
    ))
}

/// Write `phi:infoFile` as raw JSON `{sha, csv, songs, maxDifficulty, updatedAt}` (not the web UI's
/// `{d,e}` envelope). The body goes through curl's stdin, so concurrent runs share no temp file
fn put_kv(creds: &Creds, csv: &str, sha: &str) -> Result<(), String> {
    let mut max_difficulty = 0.0f64;
    let mut songs = 0usize;
    for line in csv.lines().skip(1) {
        songs += 1;
        for cell in line.split('\t').rev().take(4) {
            if let Ok(n) = cell.parse::<f64>() {
                max_difficulty = max_difficulty.max(n);
            }
        }
    }
    let payload = serde_json::json!({
        "sha": sha,
        "csv": csv,
        "songs": songs,
        "maxDifficulty": max_difficulty,
        "updatedAt": chrono_now(),
    })
    .to_string();
    let url = kv_url(creds);
    let mut child = Command::new("curl")
        .args(CURL_PUT)
        .args([
            "-X",
            "PUT",
            "-H",
            &format!("Authorization: Bearer {}", creds.token),
        ])
        .args([
            "-H",
            "Content-Type: application/json",
            "--data-binary",
            "@-",
            &url,
        ])
        .stdin(Stdio::piped())
        .spawn()
        .map_err(|e| e.to_string())?;
    let wrote = child
        .stdin
        .take()
        .map(|mut stdin| stdin.write_all(payload.as_bytes()));
    let status = child.wait().map_err(|e| e.to_string())?;
    if let Some(Err(err)) = wrote {
        return Err(format!("kv put stdin: {err}"));
    }
    if status.success() {
        Ok(())
    } else {
        Err(format!("kv put exited {status}"))
    }
}

fn kv_url(creds: &Creds) -> String {
    format!(
        "{API}/accounts/{}/storage/kv/namespaces/{}/values/{}",
        creds.account,
        creds.kv,
        encode(KV_KEY),
    )
}

fn chrono_now() -> String {
    // Unix seconds; the web UI treats updatedAt as an opaque stamp
    let secs = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0);
    format!("{secs}")
}

fn encode(part: &str) -> String {
    let mut out = String::new();
    for b in part.bytes() {
        match b {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => {
                out.push(b as char)
            }
            _ => out.push_str(&format!("%{b:02X}")),
        }
    }
    out
}

fn text(v: &Value) -> String {
    match v {
        Value::String(s) => s.trim().to_string(),
        Value::Number(n) => n.to_string(),
        _ => String::new(),
    }
}

fn lang(v: &Value) -> String {
    if let Some(obj) = v.as_object() {
        for key in ["zh_cn", "en"] {
            if let Some(s) = obj.get(key).and_then(|x| x.as_str()) {
                if !s.is_empty() {
                    return s.to_string();
                }
            }
        }
    }
    text(v)
}

fn fmt_diff(v: &Value) -> String {
    match v {
        Value::Number(n) => n.as_f64().map(|n| format!("{n:.1}")).unwrap_or_default(),
        Value::String(s) if !s.is_empty() => s
            .parse::<f64>()
            .map(|n| format!("{n:.1}"))
            .unwrap_or_default(),
        _ => String::new(),
    }
}

fn cell(value: &str) -> String {
    value.replace(['\t', '\r', '\n'], " ")
}

#[cfg(test)]
mod tests {
    use super::*;

    fn local(md5: &str, size: u64, content_type: &str) -> Published {
        Published {
            md5: md5.into(),
            size,
            content_type: content_type.into(),
        }
    }

    #[test]
    fn only_music_downloads_as_a_file() {
        assert_eq!(disposition("phira/IN/a-IN.pez"), None);
        assert_eq!(
            disposition("music/Stasis.Maozon.ogg").as_deref(),
            Some("attachment; filename=\"Stasis.Maozon.ogg\"; filename*=UTF-8''Stasis.Maozon.ogg")
        );
        assert_eq!(
            disposition("music/雪 \"x\".ogg").as_deref(),
            Some("attachment; filename=\"_ _x_.ogg\"; filename*=UTF-8''%E9%9B%AA%20%22x%22.ogg")
        );
    }

    #[test]
    fn reads_r2_list_pages() {
        let body = br#"{"success":true,"result":[
            {"key":"phira/IN/a-IN.pez","size":12,"etag":"\"ABC123\"","http_metadata":{"contentType":"application/octet-stream"}},
            {"key":"phira/IN/b-IN.pez","size":"34","etag":""},
            {"key":"phira/IN/c-IN.pez"}],
            "result_info":{"cursor":"next-page","is_truncated":true,"per_page":1000}}"#;
        let page: ListPage = serde_json::from_slice(body).unwrap();
        assert_eq!(page.next_cursor().as_deref(), Some("next-page"));
        let objects: Vec<Remote> = page.result.into_iter().map(Remote::from).collect();
        assert_eq!(
            objects[0],
            Remote {
                size: 12,
                etag: Some("abc123".into()),
                content_type: Some("application/octet-stream".into()),
            }
        );
        assert_eq!((objects[1].size, objects[1].etag.as_deref()), (34, None));
        assert_eq!(objects[2].size, u64::MAX);
        // The last page: no cursor to follow, whatever else result_info says.
        for last in [
            r#"{"result":[],"result_info":{"cursor":"x","is_truncated":false}}"#,
            r#"{"result":[],"result_info":{"cursor":"","is_truncated":true}}"#,
            r#"{"result":[]}"#,
        ] {
            let page: ListPage = serde_json::from_str(last).unwrap();
            assert_eq!(page.next_cursor(), None, "{last}");
        }
        // A result that is not a list fails the listing, so publish falls back to uploading everything.
        assert!(serde_json::from_str::<ListPage>(r#"{"result":{"objects":[]}}"#).is_err());
    }

    #[test]
    fn skips_only_objects_r2_already_holds() {
        let png = local("abc", 10, "image/png");
        let remote = |size, etag: Option<&str>, ty: Option<&str>| Remote {
            size,
            etag: etag.map(String::from),
            content_type: ty.map(String::from),
        };
        // Missing from R2 (deleted by another writer): upload, whatever published.json says.
        assert!(!up_to_date(&png, None, Some(&png)));
        assert!(up_to_date(&png, Some(&remote(10, Some("abc"), None)), None));
        // Overwritten with other bytes, or another size.
        assert!(!up_to_date(
            &png,
            Some(&remote(10, Some("fff"), None)),
            Some(&png)
        ));
        assert!(!up_to_date(
            &png,
            Some(&remote(11, Some("abc"), None)),
            Some(&png)
        ));
        // No etag in the listing: published.json must vouch for the bytes.
        assert!(!up_to_date(&png, Some(&remote(10, None, None)), None));
        assert!(up_to_date(&png, Some(&remote(10, None, None)), Some(&png)));
        assert!(!up_to_date(
            &png,
            Some(&remote(10, None, None)),
            Some(&local("old", 10, "image/png"))
        ));
        // Content type: R2's word first, then the record of the last upload.
        let ty = Some("image/png");
        assert!(up_to_date(&png, Some(&remote(10, Some("abc"), ty)), None));
        let other = Some("application/octet-stream");
        assert!(!up_to_date(
            &png,
            Some(&remote(10, Some("abc"), other)),
            None
        ));
        let recorded = local("abc", 10, "text/plain");
        assert!(!up_to_date(
            &png,
            Some(&remote(10, Some("abc"), None)),
            Some(&recorded)
        ));
        assert!(same_mime(
            "text/plain; charset=utf-8",
            "text/plain;charset=UTF-8"
        ));
    }
}
