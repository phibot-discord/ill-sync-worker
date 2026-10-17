use crate::collection_art;
use crate::extract;
use crate::files::write_if_changed;
use crate::layout::{self, song_id, LEVELS};
use crate::par;
use crate::pv;
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

/// Concurrent curl uploads. Upload speed, not CPU, bounds this
const UPLOAD_WORKERS: usize = 8;
/// Cloudflare's API allows 1,200 requests per 5 minutes per user, shared with the web UI and bot
const DEFAULT_RPS: f64 = 3.0;
const KV_KEY: &str = "phi:infoFile";
const API: &str = "https://api.cloudflare.com/client/v4";
/// R2's list API returns at most 1,000 keys a page; the largest prefix here, chart/, holds about 1,060
const LIST_PAGE_CAP: usize = 50;
/// Extract's chart tree, uploaded under the same name: `chart/<folder>/<LEVEL>.json`
const CHART_DIR: &str = "chart";
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
    md5: String,
    size: u64,
    #[serde(rename = "type")]
    content_type: String,
}

impl Published {
    fn of_file(path: &Path, content_type: &str) -> Result<Self, String> {
        let fail = |e: std::io::Error| format!("{}: {e}", path.display());
        let mut file = File::open(path).map_err(fail)?;
        let mut hasher = Md5::new();
        let size = std::io::copy(&mut file, &mut hasher).map_err(fail)?;
        Ok(Self {
            md5: format!("{:x}", hasher.finalize()),
            size,
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

/// A local file and the key and content type it goes up as
pub struct Upload {
    pub path: PathBuf,
    pub key: String,
    pub mime: &'static str,
}

/// Upload APK-derived jackets, avatars, packs, music and catalog files, then the KV catalog. Community data
/// stays on the sync Worker. R2 is listed first and every object whose size and etag (MD5) already
/// match is skipped, so objects another writer deleted or overwrote are sent again; KV is rewritten
/// unless it already holds this info.csv. If the listing fails, everything is uploaded. Set
/// UNPACK_PUBLISH_ALL=1 to send everything anyway. `rebuild_notes` recomputes notesInfo.json from the
/// charts on disk (for `--publish`; extract writes it after a successful decode). The collection, its art,
/// the videos and the chart JSON go last, after KV, and a failure there is a warning, not a
/// failed publish. Returns the number of objects uploaded and those warnings
pub fn publish(
    output: &Path,
    manifest_path: &Path,
    rebuild_notes: bool,
) -> Result<(usize, Vec<String>), String> {
    let creds = Creds::from_env()?;
    let assets = assets(output);
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
    let mut info: Vec<Upload> = [
        ("info.csv", "text/tab-separated-values; charset=utf-8"),
        ("notesInfo.json", "application/json"),
        ("tips.txt", "text/plain; charset=utf-8"),
        ("avatar.txt", "text/plain; charset=utf-8"),
        // Display name → addressable key: a few avatars' files are named after the key, not the name
        ("avatars.json", "application/json"),
    ]
    .into_iter()
    .map(|(name, mime)| Upload {
        path: info_dir.join(name),
        key: format!("info/{name}"),
        mime,
    })
    .collect();
    // After the jackets it lists, so the web UI never reads a manifest ahead of its objects. An output
    // tree from before per-difficulty jackets has none
    info.extend(json_file(output, extract::LEVELS_MANIFEST));
    info.extend(json_file(output, crate::gameinfo::TIPS_MANIFEST));
    let collection_files = collect(output.join("collection"), "collection", "application/json");
    let art_files = collect_media(
        output.join(collection_art::DIR),
        collection_art::DIR,
        &[("png", "image/png"), ("jpg", "image/jpeg")],
    );
    let art_index: Vec<Upload> = json_file(output, collection_art::MANIFEST)
        .into_iter()
        .collect();
    let videos = collect(output.join(pv::VIDEO_DIR), pv::VIDEO_DIR, "video/mp4");
    let video_index: Vec<Upload> = json_file(output, pv::MANIFEST).into_iter().collect();
    let chart_files = charts(output);

    let force = std::env::var("UNPACK_PUBLISH_ALL").is_ok_and(|v| v == "1" || v == "true");
    let pacer = Pacer::from_env();
    let remote = if force {
        None
    } else {
        let prefixes = list_prefixes(
            assets
                .iter()
                .chain(&info)
                .chain(&collection_files)
                .chain(&art_files)
                .chain(&art_index)
                .chain(&videos)
                .chain(&video_index)
                .chain(&chart_files),
        );
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
    let put = |batch: &[Upload]| -> Result<(usize, usize), String> {
        let result = uploader.put_all(batch);
        manifest
            .lock()
            .map_err(|e| e.to_string())?
            .save(manifest_path)?;
        result
    };
    // Assets first, then the catalog files that point at them, then KV
    let mut sent = 0;
    let mut skipped = 0;
    for batch in [&assets[..], &info[..]] {
        let (n, s) = put(batch)?;
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
    // The charts come last: the packs already carry them, and their first upload (about 2.7 GB) then holds up
    // none of the groups before
    let (n, s, warnings) = put_groups(
        &[
            ("collection", vec![&collection_files[..]]),
            ("collection art", vec![&art_files[..], &art_index[..]]),
            ("videos", vec![&videos[..], &video_index[..]]),
            ("charts", vec![&chart_files[..]]),
        ],
        put,
    );
    sent += n;
    skipped += s;
    eprintln!("publish: {sent} uploaded, {skipped} already in R2");
    let mut warnings = warnings;
    if warnings.is_empty() {
        let produced: BTreeSet<String> = assets
            .iter()
            .chain(&info)
            .chain(&collection_files)
            .chain(&art_files)
            .chain(&art_index)
            .chain(&videos)
            .chain(&video_index)
            .chain(&chart_files)
            .map(|u| u.key.clone())
            .collect();
        if let Err(err) = prune(&creds, &pacer, &produced, &manifest, manifest_path) {
            let warning = format!("prune stopped: {err}");
            eprintln!("publish: {warning}");
            warnings.push(warning);
        }
    } else {
        eprintln!("prune: skipped, the publish had warnings");
    }
    Ok((sent, warnings))
}

/// Upload `batches` in order, each after the one before it, to the bucket the daemon publishes to (the same
/// settings and skip rule: an object R2 holds with the same size and etag is left alone). For tools that publish what
/// extract does not; `manifest_path` is their own record. With `dry_run` nothing is sent. Returns each key and
/// whether it was (or would be) uploaded
pub fn upload(
    batches: &[&[Upload]],
    manifest_path: &Path,
    dry_run: bool,
) -> Result<Vec<(String, bool)>, String> {
    let creds = Creds::from_env()?;
    let pacer = Pacer::from_env();
    let remote = list_remote(
        &creds,
        &list_prefixes(batches.iter().copied().flatten()),
        &pacer,
    )?;
    let manifest = Mutex::new(Manifest::load(manifest_path, &creds));
    let mut report = Vec::new();
    for batch in batches {
        if dry_run {
            let recorded = manifest.lock().map_err(|e| e.to_string())?;
            for file in batch.iter() {
                let local = Published::of_file(&file.path, file.mime)?;
                let current = up_to_date(
                    &local,
                    remote.get(&file.key),
                    recorded.objects.get(&file.key),
                );
                report.push((file.key.clone(), !current));
            }
            continue;
        }
        let uploader = Uploader {
            creds: &creds,
            remote: Some(&remote),
            manifest: &manifest,
            manifest_path,
            saved_at: Mutex::new(Instant::now()),
            pacer: &pacer,
        };
        let before = manifest.lock().map_err(|e| e.to_string())?.objects.clone();
        let result = uploader.put_all(batch);
        manifest
            .lock()
            .map_err(|e| e.to_string())?
            .save(manifest_path)?;
        result?;
        for file in batch.iter() {
            let local = Published::of_file(&file.path, file.mime)?;
            let current = up_to_date(&local, remote.get(&file.key), before.get(&file.key));
            report.push((file.key.clone(), !current));
        }
    }
    Ok(report)
}

/// The groups that go after KV, each in order with its list after its files. A group that fails stops there and
/// becomes a warning; the next group still goes. Returns (uploaded, already in R2, warnings)
fn put_groups(
    groups: &[(&str, Vec<&[Upload]>)],
    put: impl Fn(&[Upload]) -> Result<(usize, usize), String>,
) -> (usize, usize, Vec<String>) {
    let mut sent = 0;
    let mut skipped = 0;
    let mut warnings = Vec::new();
    for (name, group) in groups {
        for batch in group {
            match put(batch).map_err(|err| format!("{name} not published: {err}")) {
                Ok((n, s)) => {
                    sent += n;
                    skipped += s;
                }
                Err(warning) => {
                    eprintln!("publish: {warning}");
                    warnings.push(warning);
                    break;
                }
            }
        }
    }
    (sent, skipped, warnings)
}

/// What to list in R2 before uploading `uploads`: each key's directory, but chart/ as one prefix, since a
/// folder per song would cost a list request each (about 330). A listing is recursive, so a directory
/// inside one already listed adds nothing
fn list_prefixes<'a>(uploads: impl IntoIterator<Item = &'a Upload>) -> Vec<String> {
    let charts = format!("{CHART_DIR}/");
    let dirs: BTreeSet<String> = uploads
        .into_iter()
        .filter_map(|u| {
            if u.key.starts_with(&charts) {
                return Some(charts.clone());
            }
            u.key.rsplit_once('/').map(|(dir, _)| format!("{dir}/"))
        })
        .collect();
    let mut prefixes: Vec<String> = Vec::new();
    for dir in dirs {
        if !prefixes
            .last()
            .is_some_and(|kept| dir.starts_with(kept.as_str()))
        {
            prefixes.push(dir);
        }
    }
    prefixes
}

/// Jackets, avatars, every level's packs (`phira/<LEVEL>/`) and music
fn assets(output: &Path) -> Vec<Upload> {
    let mut assets = Vec::new();
    for (dir, key_prefix) in [
        ("original_ill/ill", "original_ill/ill"),
        ("original_ill/illLow", "original_ill/illLow"),
        ("original_ill/illBlur", "original_ill/illBlur"),
        ("avatar", "html/avatar"),
        ("phira", "phira"),
        ("music", "music"),
    ] {
        let mime = match key_prefix {
            "music" => "audio/ogg",
            "phira" => "application/octet-stream",
            _ => "image/png",
        };
        assets.extend(collect(output.join(dir), key_prefix, mime));
    }
    assets
}

/// Every chart extract exported, every level's, keyed like the packs and music: `chart/<id>/<LEVEL>.json`, the track
/// folder without its trailing `.0`. JSON with no disposition, like the other JSON objects
fn charts(output: &Path) -> Vec<Upload> {
    collect(output.join(CHART_DIR), CHART_DIR, "application/json")
        .into_iter()
        .map(|upload| Upload {
            key: chart_key(&upload.key),
            ..upload
        })
        .collect()
}

/// `chart/<folder>.0/<file>` → `chart/<folder>/<file>`; other folders (a song's `.1` variant) keep their name
fn chart_key(key: &str) -> String {
    let mut parts: Vec<&str> = key.splitn(3, '/').collect();
    if let [_, folder, _] = parts.as_mut_slice() {
        if let Some(stem) = folder.strip_suffix(".0") {
            *folder = stem;
        }
    }
    parts.join("/")
}

/// The prefixes no other writer touches. After a publish with no warnings, a key under one of them that the run did
/// not produce is stale: a song, chart or jacket the game no longer ships (the old APK stays in the archive, see
/// job::archive_apk). UNPACK_PRUNE=off skips this, `report` only lists what would go, `force` lifts the cap
const OWNED_PREFIXES: [&str; 6] = [
    "phira/",
    "chart/",
    "music/",
    "original_ill/ill/",
    "original_ill/illLow/",
    "original_ill/illBlur/",
];
/// A run deletes at most this many stale keys, or 2% of the owned ones if that is more, unless forced
const PRUNE_CAP: usize = 25;

fn prune(
    creds: &Creds,
    pacer: &Pacer,
    produced: &BTreeSet<String>,
    manifest: &Mutex<Manifest>,
    manifest_path: &Path,
) -> Result<usize, String> {
    let mode = std::env::var("UNPACK_PRUNE").unwrap_or_default();
    if mode == "off" {
        return Ok(0);
    }
    let prefixes: Vec<String> = OWNED_PREFIXES.iter().map(|p| p.to_string()).collect();
    let remote = list_remote(creds, &prefixes, pacer)?;
    let mut stale: Vec<&String> = remote.keys().filter(|k| !produced.contains(*k)).collect();
    stale.sort();
    let cap = PRUNE_CAP.max(remote.len() / 50);
    eprintln!(
        "prune: {} of {} owned keys are stale",
        stale.len(),
        remote.len()
    );
    if stale.is_empty() {
        return Ok(0);
    }
    if mode == "report" || (stale.len() > cap && mode != "force") {
        for key in &stale {
            eprintln!("prune: stale {key}");
        }
        if mode != "report" {
            eprintln!(
                "prune: {} stale keys is more than the {cap} a run deletes; nothing deleted (UNPACK_PRUNE=force deletes them)",
                stale.len()
            );
        }
        return Ok(0);
    }
    for key in &stale {
        pacer.wait();
        api_delete(creds, key)?;
        manifest
            .lock()
            .map_err(|e| e.to_string())?
            .objects
            .remove(*key);
        eprintln!("prune: deleted {key}");
    }
    manifest
        .lock()
        .map_err(|e| e.to_string())?
        .save(manifest_path)?;
    Ok(stale.len())
}

fn json_file(output: &Path, key: &str) -> Option<Upload> {
    let path = output.join(key);
    path.is_file().then(|| Upload {
        path,
        key: key.to_string(),
        mime: "application/json",
    })
}

/// Every object R2 holds under `prefixes`: key -> size, etag, content type
fn list_remote(
    creds: &Creds,
    prefixes: &[String],
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

/// Every output file under `dir`, keyed by its path below `key_prefix`: the per-difficulty jackets
/// sit one level down (`original_ill/ill/IN/<id>.png`)
fn collect(dir: PathBuf, key_prefix: &str, mime: &'static str) -> Vec<Upload> {
    let mut found = Vec::new();
    let _ = layout::walk(&dir, &mut |path| {
        // Dotfiles are temporaries (link_or_copy's `.<name>.link`), never outputs
        let hidden = path
            .file_name()
            .and_then(|n| n.to_str())
            .is_none_or(|n| n.starts_with('.'));
        if hidden || !path.is_file() {
            return Ok(());
        }
        if let Some(rel) = path.strip_prefix(&dir).ok().and_then(|p| p.to_str()) {
            found.push(Upload {
                path: path.to_path_buf(),
                key: format!("{key_prefix}/{rel}"),
                mime,
            });
        }
        Ok(())
    });
    found.sort_by(|a, b| a.key.cmp(&b.key));
    found
}

/// Like `collect`, typed by extension; files of other types (a manifest uploaded after them) are left out
fn collect_media(dir: PathBuf, key_prefix: &str, types: &[(&str, &'static str)]) -> Vec<Upload> {
    collect(dir, key_prefix, "")
        .into_iter()
        .filter_map(|upload| {
            let ext = upload.path.extension().and_then(|e| e.to_str())?;
            let (_, mime) = types.iter().find(|(known, _)| *known == ext)?;
            Some(Upload { mime, ..upload })
        })
        .collect()
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

/// DELETE one object; curl retries timeouts, 429 and 5xx. A key that is already gone counts as deleted
fn api_delete(creds: &Creds, key: &str) -> Result<(), String> {
    let url = format!(
        "{API}/accounts/{}/r2/buckets/{}/objects/{}",
        creds.account,
        encode(&creds.bucket),
        object_path(key),
    );
    let out = Command::new("curl")
        .args([
            "-sS",
            "-o",
            "/dev/null",
            "-w",
            "%{http_code}",
            "--connect-timeout",
            "15",
            "--max-time",
            "60",
            "--retry",
            "3",
            "-X",
            "DELETE",
        ])
        .args(["-H", &format!("Authorization: Bearer {}", creds.token)])
        .arg(&url)
        .stdin(Stdio::null())
        .output()
        .map_err(|e| format!("curl: {e}"))?;
    let code = String::from_utf8_lossy(&out.stdout).trim().to_string();
    if out.status.success() && (code.starts_with('2') || code == "404") {
        Ok(())
    } else {
        Err(format!(
            "delete {key}: HTTP {code} {}",
            String::from_utf8_lossy(&out.stderr).trim()
        ))
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

/// curl retries 429/5xx with exponential backoff (honouring Retry-After). A connection is dropped when
/// it stalls (under 32 KiB/s for two minutes), so a video can take its time but a hung upload cannot
/// hold the job
const CURL_PUT: [&str; 14] = [
    "-fsS",
    "--connect-timeout",
    "15",
    "--max-time",
    "3600",
    "--speed-limit",
    "32768",
    "--speed-time",
    "120",
    "--retry",
    "5",
    "--retry-all-errors",
    "--retry-max-time",
    "7200",
];

fn put_file(creds: &Creds, path: &Path, key: &str, mime: &str) -> Result<(), String> {
    let url = format!(
        "{API}/accounts/{}/r2/buckets/{}/objects/{}",
        creds.account,
        encode(&creds.bucket),
        object_path(key),
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

/// A key as a path in the R2 API: each segment percent-encoded, the slashes kept. Every upload goes through
/// here, so a song folder encodes alike under phira/ and chart/
fn object_path(key: &str) -> String {
    key.split('/').map(encode).collect::<Vec<_>>().join("/")
}

/// Audio and video (`.ogg`, `.mp4`) download as files from the public domain, where a cross-origin `<a download>` is
/// ignored; `<audio>` and `<video>` still play them
fn disposition(key: &str) -> Option<String> {
    let name = key
        .rsplit('/')
        .next()
        .filter(|name| name.ends_with(".ogg") || name.ends_with(".mp4"))?;
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
    fn collects_the_per_difficulty_jackets_below_their_directory() {
        let dir = std::env::temp_dir().join(format!("phi-collect-test-{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        for file in [
            "a.png",
            "IN/a.png",
            "AT/a.png",
            "IN/.a.png.link",
            ".b.png.link",
        ] {
            let path = dir.join(file);
            fs::create_dir_all(path.parent().unwrap()).unwrap();
            fs::write(path, b"x").unwrap();
        }
        let out = collect(dir.clone(), "original_ill/ill", "image/png");
        let keys: Vec<&str> = out.iter().map(|u| u.key.as_str()).collect();
        assert_eq!(
            keys,
            [
                "original_ill/ill/AT/a.png",
                "original_ill/ill/IN/a.png",
                "original_ill/ill/a.png",
            ]
        );
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn uploads_the_packs_of_every_level_including_the_unlock_only_ones() {
        let dir = std::env::temp_dir().join(format!("phi-assets-test-{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        for file in [
            "phira/IN/a-IN.pez",
            "phira/SP/a-SP.pez",
            "phira/EZ_Error/b-EZ_Error.pez",
            "phira/IN/.b-IN.pez.link",
            "music/a.ogg",
            "avatar/x.png",
        ] {
            let path = dir.join(file);
            fs::create_dir_all(path.parent().unwrap()).unwrap();
            fs::write(path, b"x").unwrap();
        }
        let found = assets(&dir);
        let keys: Vec<(&str, &str)> = found.iter().map(|u| (u.key.as_str(), u.mime)).collect();
        assert_eq!(
            keys,
            [
                ("html/avatar/x.png", "image/png"),
                ("phira/EZ_Error/b-EZ_Error.pez", "application/octet-stream"),
                ("phira/IN/a-IN.pez", "application/octet-stream"),
                ("phira/SP/a-SP.pez", "application/octet-stream"),
                ("music/a.ogg", "audio/ogg"),
            ]
        );
        for (key, _) in &keys[1..4] {
            assert_eq!(disposition(key), None, "{key}");
        }
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn uploads_every_chart_as_json_without_a_disposition_apart_from_the_assets() {
        let dir = std::env::temp_dir().join(format!("phi-charts-test-{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        for file in [
            "chart/Message.くるぶっこちゃん.0/IN.json",
            "chart/Message.くるぶっこちゃん.0/SP.json",
            "chart/望影の方舟Six.SeURa.0/Legacy.json",
            "chart/望影の方舟Six.SeURa.0/EZ_Error.json",
            "chart/望影の方舟Six.SeURa.0/HD_Error.json",
            "chart/望影の方舟Six.SeURa.0/IN_Error.json",
            "chart/望影の方舟Six.SeURa.0/.IN.json.link",
            "phira/SP/Message.くるぶっこちゃん-SP.pez",
        ] {
            let path = dir.join(file);
            fs::create_dir_all(path.parent().unwrap()).unwrap();
            fs::write(path, b"{}").unwrap();
        }
        let found = charts(&dir);
        let keys: Vec<(&str, &str)> = found.iter().map(|u| (u.key.as_str(), u.mime)).collect();
        assert_eq!(
            keys,
            [
                ("chart/Message.くるぶっこちゃん/IN.json", "application/json"),
                ("chart/Message.くるぶっこちゃん/SP.json", "application/json"),
                (
                    "chart/望影の方舟Six.SeURa/EZ_Error.json",
                    "application/json"
                ),
                (
                    "chart/望影の方舟Six.SeURa/HD_Error.json",
                    "application/json"
                ),
                (
                    "chart/望影の方舟Six.SeURa/IN_Error.json",
                    "application/json"
                ),
                ("chart/望影の方舟Six.SeURa/Legacy.json", "application/json"),
            ]
        );
        for upload in &found {
            let rel = upload.path.strip_prefix(&dir).unwrap().to_str().unwrap();
            assert_eq!(
                chart_key(rel),
                upload.key,
                "the key is the output path less the .0"
            );
            assert_eq!(disposition(&upload.key), None, "{}", upload.key);
        }
        // The assets go before KV and fail the publish; the charts are a group of their own after it
        assert!(assets(&dir).iter().all(|u| !u.key.starts_with("chart/")));
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn chart_keys_drop_only_a_trailing_dot_zero() {
        assert_eq!(
            chart_key("chart/Glaciaxion.SunsetRay.0/EZ.json"),
            "chart/Glaciaxion.SunsetRay/EZ.json"
        );
        assert_eq!(
            chart_key("chart/Random.SobremSilentroom.1/IN.json"),
            "chart/Random.SobremSilentroom.1/IN.json"
        );
        assert_eq!(
            chart_key("chart/Introduction/EZ.json"),
            "chart/Introduction/EZ.json"
        );
    }

    #[test]
    fn encodes_tricky_song_folders_alike_under_chart_and_phira() {
        let dir = std::env::temp_dir().join(format!("phi-chart-keys-test-{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        // Paths worked out apart from `encode` (Python's urllib.parse.quote with safe=""). The first five are
        // real folders; none has an apostrophe, a space or a #, so the last two pin those
        let cases = [
            (
                "chart/望影の方舟Six.SeURa.0/EZ_Error.json",
                "chart/%E6%9C%9B%E5%BD%B1%E3%81%AE%E6%96%B9%E8%88%9FSix.SeURa.0/EZ_Error.json",
            ),
            (
                "phira/EZ_Error/望影の方舟Six.SeURa-EZ_Error.pez",
                "phira/EZ_Error/%E6%9C%9B%E5%BD%B1%E3%81%AE%E6%96%B9%E8%88%9FSix.SeURa-EZ_Error.pez",
            ),
            (
                "chart/Message.くるぶっこちゃん.0/SP.json",
                "chart/Message.%E3%81%8F%E3%82%8B%E3%81%B6%E3%81%A3%E3%81%93%E3%81%A1%E3%82%83%E3%82%93.0/SP.json",
            ),
            (
                "phira/SP/Message.くるぶっこちゃん-SP.pez",
                "phira/SP/Message.%E3%81%8F%E3%82%8B%E3%81%B6%E3%81%A3%E3%81%93%E3%81%A1%E3%82%83%E3%82%93-SP.pez",
            ),
            (
                "chart/Rrharil.TeamGrimoire.0/IN.json",
                "chart/Rrharil.TeamGrimoire.0/IN.json",
            ),
            (
                "chart/彼方へ名もなき海辺より.打打だいず&月乃.0/IN.json",
                "chart/%E5%BD%BC%E6%96%B9%E3%81%B8%E5%90%8D%E3%82%82%E3%81%AA%E3%81%8D%E6%B5%B7%E8%BE%BA%E3%82%88%E3%82%8A.%E6%89%93%E6%89%93%E3%81%A0%E3%81%84%E3%81%9A%26%E6%9C%88%E4%B9%83.0/IN.json",
            ),
            (
                "phira/IN/彼方へ名もなき海辺より.打打だいず&月乃-IN.pez",
                "phira/IN/%E5%BD%BC%E6%96%B9%E3%81%B8%E5%90%8D%E3%82%82%E3%81%AA%E3%81%8D%E6%B5%B7%E8%BE%BA%E3%82%88%E3%82%8A.%E6%89%93%E6%89%93%E3%81%A0%E3%81%84%E3%81%9A%26%E6%9C%88%E4%B9%83-IN.pez",
            ),
            (
                "chart/Rrhar'il.Team Grimoire.0/AT.json",
                "chart/Rrhar%27il.Team%20Grimoire.0/AT.json",
            ),
            (
                "chart/#1 Song+100%?.x.0/HD.json",
                "chart/%231%20Song%2B100%25%3F.x.0/HD.json",
            ),
        ];
        for (key, _) in cases {
            let path = dir.join(key);
            fs::create_dir_all(path.parent().unwrap()).unwrap();
            fs::write(path, b"{}").unwrap();
        }
        // Keys stay raw, as R2's listing reports them; only the API path is encoded. Chart folders lose their `.0`
        let found: BTreeMap<String, &str> = charts(&dir)
            .into_iter()
            .chain(assets(&dir))
            .map(|u| (u.key, u.mime))
            .collect();
        let expected: BTreeMap<String, &str> = cases
            .iter()
            .map(|(key, _)| {
                if key.starts_with("chart/") {
                    (chart_key(key), "application/json")
                } else {
                    (key.to_string(), "application/octet-stream")
                }
            })
            .collect();
        assert!(found.contains_key("chart/Rrharil.TeamGrimoire/IN.json"));
        assert_eq!(found, expected);
        for (key, path) in cases {
            assert_eq!(object_path(key), path, "{key}");
            assert_eq!(disposition(key), None, "{key}");
        }
        let _ = fs::remove_dir_all(&dir);
    }

    fn upload(key: &str) -> Upload {
        Upload {
            path: PathBuf::from("/nonexistent").join(key),
            key: key.into(),
            mime: "application/json",
        }
    }

    #[test]
    fn lists_the_chart_tree_with_one_prefix_and_the_rest_by_directory() {
        let uploads: Vec<Upload> = [
            "chart/Message.くるぶっこちゃん.0/SP.json",
            "chart/望影の方舟Six.SeURa.0/EZ_Error.json",
            "chart/彼方へ名もなき海辺より.打打だいず&月乃.0/IN.json",
            "phira/IN/a-IN.pez",
            "phira/SP/a-SP.pez",
            "original_ill/ill/a.png",
            "original_ill/ill/IN/a.png",
            "original_ill/levels.json",
            "collection/index.json",
            "collection/text/en.json",
            "collection-art/a.jpg",
            "info/info.csv",
            "html/avatar/x.png",
            "music/a.ogg",
        ]
        .into_iter()
        .map(upload)
        .collect();
        assert_eq!(
            list_prefixes(&uploads),
            [
                "chart/",
                "collection-art/",
                "collection/",
                "html/avatar/",
                "info/",
                "music/",
                "original_ill/",
                "phira/IN/",
                "phira/SP/",
            ]
        );
        assert_eq!(list_prefixes(&uploads[3..5]), ["phira/IN/", "phira/SP/"]);
    }

    #[test]
    fn a_failed_chart_upload_is_a_warning_and_never_stops_the_other_groups() {
        let collection = [upload("collection/index.json")];
        let art = [upload("collection-art/a.jpg")];
        let art_index = [upload("collection-art/index.json")];
        let videos = [upload("pv/video/a.mp4")];
        let video_index = [upload("pv/index.json")];
        let chart_files = [
            upload("chart/A.0/IN.json"),
            upload("chart/望影の方舟Six.SeURa.0/EZ_Error.json"),
        ];
        let groups = [
            ("collection", vec![&collection[..]]),
            ("collection art", vec![&art[..], &art_index[..]]),
            ("videos", vec![&videos[..], &video_index[..]]),
            ("charts", vec![&chart_files[..]]),
        ];
        let run = |failing: &[&str]| {
            let tried = Mutex::new(Vec::new());
            let (sent, skipped, warnings) = put_groups(&groups, |batch: &[Upload]| {
                let first = batch[0].key.clone();
                tried.lock().unwrap().push(first.clone());
                match failing.iter().find(|prefix| first.starts_with(*prefix)) {
                    Some(_) => Err(format!("{first}: r2 put exited exit status: 22")),
                    None => Ok((batch.len(), 1)),
                }
            });
            (sent, skipped, warnings, tried.into_inner().unwrap())
        };

        let (sent, skipped, warnings, tried) = run(&["chart/"]);
        assert_eq!(
            warnings,
            ["charts not published: chart/A.0/IN.json: r2 put exited exit status: 22"]
        );
        assert_eq!(tried.len(), 6);
        assert_eq!((sent, skipped), (5, 5));

        // A group failing before the charts stops only itself
        let (_, _, warnings, tried) = run(&["pv/", "chart/"]);
        assert_eq!(
            warnings,
            [
                "videos not published: pv/video/a.mp4: r2 put exited exit status: 22",
                "charts not published: chart/A.0/IN.json: r2 put exited exit status: 22",
            ]
        );
        assert!(!tried.contains(&"pv/index.json".to_string()));
        assert!(tried.contains(&"chart/A.0/IN.json".to_string()));

        // The art's list waits for its images, and the charts still go
        let (sent, skipped, warnings, tried) = run(&["collection-art/a.jpg"]);
        assert_eq!(
            warnings,
            ["collection art not published: collection-art/a.jpg: r2 put exited exit status: 22"]
        );
        assert!(!tried.contains(&"collection-art/index.json".to_string()));
        assert_eq!(tried.last().map(String::as_str), Some("chart/A.0/IN.json"));
        assert_eq!((sent, skipped), (5, 4));
    }

    #[test]
    fn collects_the_collection_art_typed_by_extension() {
        let dir = std::env::temp_dir().join(format!("phi-media-test-{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        for file in [
            "b.png",
            "b.jpg",
            "a.jpg",
            "index.json",
            ".a.png.link",
            "notes.txt",
        ] {
            let path = dir.join(file);
            fs::create_dir_all(path.parent().unwrap()).unwrap();
            fs::write(path, b"x").unwrap();
        }
        let out = collect_media(
            dir.clone(),
            "collection-art",
            &[("png", "image/png"), ("jpg", "image/jpeg")],
        );
        let keys: Vec<(&str, &str)> = out.iter().map(|u| (u.key.as_str(), u.mime)).collect();
        assert_eq!(
            keys,
            [
                ("collection-art/a.jpg", "image/jpeg"),
                ("collection-art/b.jpg", "image/jpeg"),
                ("collection-art/b.png", "image/png"),
            ]
        );
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn only_audio_and_video_download_as_files() {
        assert_eq!(disposition("phira/IN/a-IN.pez"), None);
        assert_eq!(disposition("pv/index.json"), None);
        assert_eq!(disposition("collection-art/liangrenhuihe0.png"), None);
        assert_eq!(disposition("collection-art/blackhole0-tile.png"), None);
        assert_eq!(disposition("c9s6/chart.json"), None);
        assert_eq!(disposition("c9s6/index.json"), None);
        assert_eq!(disposition("c9s6/C9S6-SP.pez"), None);
        assert_eq!(
            disposition("c9s6/loop.mp4").as_deref(),
            Some("attachment; filename=\"loop.mp4\"; filename*=UTF-8''loop.mp4")
        );
        assert_eq!(
            disposition("c9s6/cutscene-typing.ogg").as_deref(),
            Some("attachment; filename=\"cutscene-typing.ogg\"; filename*=UTF-8''cutscene-typing.ogg")
        );
        assert_eq!(
            disposition("c9s6/music.ogg").as_deref(),
            Some("attachment; filename=\"music.ogg\"; filename*=UTF-8''music.ogg")
        );
        assert_eq!(
            disposition("pv/video/spas-unlock.mp4").as_deref(),
            Some("attachment; filename=\"spas-unlock.mp4\"; filename*=UTF-8''spas-unlock.mp4")
        );
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
