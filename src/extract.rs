use crate::audio;
use crate::catalog::{self, Want};
use crate::files::{self, write_if_changed};
use crate::gameinfo::{self, VersionFile};
use crate::jackets;
use crate::layout::{self, song_id};
use crate::par;
use crate::texture;
use crate::unityfs;
use serde::Deserialize;
use serde_json::Value;
use std::collections::{BTreeMap, HashMap, HashSet};
use std::fs::{self, File};
use std::io::Read;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Mutex;
use zip::ZipArchive;

const LEVELS: [&str; 4] = ["EZ", "HD", "IN", "AT"];

/// Directories extract owns. Files in them that this run did not produce are deleted afterwards
const OWNED_DIRS: [&str; 7] = [
    "chart",
    "music",
    "illustration",
    "illustrationLowRes",
    "illustrationBlur",
    "avatar",
    "original_ill",
];

/// Note counts `[tap, drag, hold, flick]` and the last note end, in chart time units
type NoteCounts = ([u32; 4], f64);
/// NoteCounts per track folder and level
type ChartStats = BTreeMap<String, BTreeMap<String, NoteCounts>>;

pub struct Exported {
    pub jackets: usize,
    pub bundles: usize,
}

/// Read an APK and write chart/, music/, illustration*/, avatar/, original_ill/ and info/ in their final
/// layout. Bundles are decoded in parallel; unchanged files are left untouched so their mtimes survive
pub fn export(
    apk: &Path,
    output: &Path,
    version_code: &str,
    version_name: &str,
) -> Result<Exported, String> {
    // Older builds wrote into asset/ and renamed out of it
    let legacy = output.join("asset");
    if legacy.exists() {
        fs::remove_dir_all(&legacy).map_err(|e| e.to_string())?;
    }
    let file = File::open(apk).map_err(|e| e.to_string())?;
    let mut zip = ZipArchive::new(file).map_err(|e| format!("apk is not a zip: {e}"))?;
    let names: Vec<String> = zip.file_names().map(|s| s.to_string()).collect();
    let level0 = read_level0(&mut zip, &names)?;
    let songs = gameinfo::parse_songs(&level0)?;
    let tips = gameinfo::parse_tips(&level0)?;
    let avatars = gameinfo::parse_avatars(&level0)?;
    eprintln!(
        "level0: {} songs, {} tips, {} avatars",
        songs.len(),
        tips.len(),
        avatars.len()
    );
    let info = output.join("info");
    fs::create_dir_all(&info).map_err(|e| e.to_string())?;
    write_json(&info.join("songs.json"), &gameinfo::songs_json(&songs))?;
    write_json(&info.join("tips.json"), &gameinfo::tips_json(&tips))?;
    write_json(
        &info.join("avatars.json"),
        &gameinfo::avatars_json(&avatars),
    )?;
    let code = version_code.parse::<i64>().unwrap_or(0);
    write_json(
        &info.join("version.json"),
        &serde_json::to_value(VersionFile {
            code,
            name: version_name.to_string(),
        })
        .map_err(|e| e.to_string())?,
    )?;

    let catalog_name = names
        .iter()
        .find(|n| n.ends_with("aa/catalog.json"))
        .cloned()
        .ok_or("apk has no addressables catalog")?;
    let catalog_bytes = read_zip(&mut zip, &catalog_name)?;
    let catalog: Value = serde_json::from_slice(&catalog_bytes).map_err(|e| e.to_string())?;
    drop(catalog_bytes);
    let jobs = unique_outputs(catalog::jobs(&catalog)?);
    drop(catalog);
    let threads = par::threads();
    eprintln!("addressables: {} bundles on {threads} threads", jobs.len());

    let written: Mutex<HashSet<PathBuf>> = Mutex::new(HashSet::new());
    let stats: Mutex<ChartStats> = Mutex::new(BTreeMap::new());
    let done = AtomicUsize::new(0);
    let open = || {
        let file = File::open(apk).map_err(|e| e.to_string())?;
        ZipArchive::new(file).map_err(|e| format!("apk is not a zip: {e}"))
    };
    par::for_each(&jobs, threads, open, |zip, _, job| {
        let n = done.fetch_add(1, Ordering::Relaxed);
        if n.is_multiple_of(200) {
            eprintln!("extract {n}/{}", jobs.len());
        }
        let ctx = |e: String| format!("{}: {e}", job.zip_path);
        let bytes = read_zip(zip, &job.zip_path).map_err(ctx)?;
        let nodes = unityfs::decode(&bytes).map_err(ctx)?;
        drop(bytes);
        let dest = output.join(layout::output_path(&job.want));
        let mut paths = vec![dest.clone()];
        match &job.want {
            Want::Chart { folder, diff } => {
                let (body, counts) = carve_chart(&nodes).map_err(ctx)?;
                write_if_changed(&dest, &body)?;
                let mut stats = stats.lock().map_err(|e| e.to_string())?;
                stats
                    .entry(folder.clone())
                    .or_default()
                    .insert(diff.clone(), counts);
            }
            Want::Jacket { .. } => {
                let tex = texture::texture_from_bundle(&nodes).map_err(ctx)?;
                drop(nodes);
                write_if_changed(&dest, &texture::encode_png(&tex).map_err(ctx)?)?;
                let rgb = tex
                    .into_rgb()
                    .ok_or_else(|| ctx("unexpected texture layout".into()))?;
                let name = dest
                    .file_name()
                    .and_then(|n| n.to_str())
                    .ok_or("jacket without a name")?;
                paths.extend(jackets::derive(output, name, &rgb).map_err(ctx)?);
            }
            Want::Music { .. } => {
                write_if_changed(&dest, &audio::ogg_from_nodes(&nodes).map_err(ctx)?)?;
            }
            Want::Avatar { .. } => {
                write_if_changed(&dest, &texture::png_from_bundle(&nodes).map_err(ctx)?)?;
            }
        }
        written.lock().map_err(|e| e.to_string())?.extend(paths);
        Ok(())
    })?;
    let written = written.into_inner().map_err(|e| e.to_string())?;
    let mut stale = 0;
    for dir in OWNED_DIRS {
        stale += files::prune(&output.join(dir), &written)?;
    }
    let stats = stats.into_inner().map_err(|e| e.to_string())?;
    write_if_changed(
        &info.join("notesInfo.json"),
        notes_info_json(&stats)?.as_bytes(),
    )?;
    let jackets = jobs
        .iter()
        .filter(|j| matches!(j.want, Want::Jacket { .. }))
        .count();
    eprintln!("extract done; removed {stale} stale files");
    Ok(Exported {
        jackets,
        bundles: jobs.len(),
    })
}

/// When two bundles map to one output file, keep the later one (the old serial loop overwrote earlier
/// files), so parallel workers never write the same path
fn unique_outputs(jobs: Vec<catalog::BundleJob>) -> Vec<catalog::BundleJob> {
    let mut last = HashMap::new();
    for (i, job) in jobs.iter().enumerate() {
        last.insert(layout::output_path(&job.want), i);
    }
    jobs.into_iter()
        .enumerate()
        .filter(|(i, job)| last.get(&layout::output_path(&job.want)) == Some(i))
        .map(|(_, job)| job)
        .collect()
}

fn write_json(path: &Path, value: &Value) -> Result<(), String> {
    let body = serde_json::to_vec(value).map_err(|e| e.to_string())?;
    write_if_changed(path, &body).map(|_| ())
}

fn read_level0(zip: &mut ZipArchive<File>, names: &[String]) -> Result<Vec<u8>, String> {
    if names.iter().any(|n| n == "assets/bin/Data/data.unity3d") {
        let packed = read_zip(zip, "assets/bin/Data/data.unity3d")?;
        return unityfs::read_named(&packed, "level0");
    }
    let mut parts: Vec<&String> = names
        .iter()
        .filter(|n| n.ends_with("/level0") || n.contains("/level0.split"))
        .collect();
    parts.sort_by_key(|n| split_ord(n));
    if parts.is_empty() {
        return Err("apk has no level0 and no data.unity3d".into());
    }
    let mut all = Vec::new();
    for name in parts {
        all.extend(read_zip(zip, name)?);
    }
    Ok(all)
}

fn split_ord(name: &str) -> u32 {
    name.rsplit_once(".split")
        .and_then(|(_, n)| n.parse::<u32>().ok())
        .map(|n| n + 1)
        .unwrap_or(0)
}

fn read_zip(zip: &mut ZipArchive<File>, name: &str) -> Result<Vec<u8>, String> {
    let mut file = zip
        .by_name(name)
        .map_err(|e| format!("missing {name}: {e}"))?;
    let mut buf = Vec::new();
    file.read_to_end(&mut buf).map_err(|e| e.to_string())?;
    Ok(buf)
}

fn json_end(slice: &[u8]) -> Result<usize, String> {
    let mut depth = 0i32;
    let mut in_str = false;
    let mut esc = false;
    for (i, &b) in slice.iter().enumerate() {
        if in_str {
            if esc {
                esc = false;
            } else if b == b'\\' {
                esc = true;
            } else if b == b'"' {
                in_str = false;
            }
            continue;
        }
        match b {
            b'"' => in_str = true,
            b'{' | b'[' => depth += 1,
            b'}' | b']' => {
                depth -= 1;
                if depth == 0 {
                    return Ok(i + 1);
                }
            }
            _ => {}
        }
    }
    Err("chart json did not close".into())
}

/// Only the chart fields notesInfo needs. Parsing these is about 3x faster than a full `Value` on a
/// 2 MB chart; any chart they do not fit is parsed as a `Value` instead
#[derive(Deserialize)]
struct ChartNotes {
    #[serde(rename = "judgeLineList")]
    lines: Option<Vec<LineNotes>>,
}

#[derive(Deserialize)]
struct LineNotes {
    #[serde(rename = "notesAbove", default)]
    above: Vec<NoteTime>,
    #[serde(rename = "notesBelow", default)]
    below: Vec<NoteTime>,
}

#[derive(Deserialize)]
struct NoteTime {
    #[serde(rename = "type", default)]
    kind: u64,
    #[serde(default)]
    time: f64,
    #[serde(rename = "holdTime", default)]
    hold_time: f64,
}

fn carve_chart(nodes: &[unityfs::Node]) -> Result<(Vec<u8>, NoteCounts), String> {
    let needle = b"{\"formatVersion\"";
    for node in nodes {
        if let Some(rel) = find(&node.bytes, needle) {
            let slice = &node.bytes[rel..];
            let mut stream = serde_json::Deserializer::from_slice(slice).into_iter::<ChartNotes>();
            if let Some(Ok(ChartNotes { lines: Some(lines) })) = stream.next() {
                let end = stream.byte_offset();
                return Ok((slice[..end].to_vec(), line_counts(&lines)));
            }
            let end = json_end(slice)?;
            let value: Value = serde_json::from_slice(&slice[..end]).map_err(|e| e.to_string())?;
            if value.get("judgeLineList").is_none() {
                continue;
            }
            return Ok((slice[..end].to_vec(), count_notes(&value)));
        }
    }
    Err("bundle has no chart json".into())
}

fn find(haystack: &[u8], needle: &[u8]) -> Option<usize> {
    let first = *needle.first()?;
    let mut at = 0;
    while let Some(i) = haystack.get(at..)?.iter().position(|b| *b == first) {
        if haystack[at + i..].starts_with(needle) {
            return Some(at + i);
        }
        at += i + 1;
    }
    None
}

/// Note counts by type (tap, drag, hold, flick) and the latest note end, in chart time units
pub fn count_notes(chart: &Value) -> NoteCounts {
    let mut counts = [0u32; 4];
    let mut max_time = 0.0f64;
    let Some(lines) = chart.get("judgeLineList").and_then(|v| v.as_array()) else {
        return (counts, max_time);
    };
    for line in lines {
        for side in ["notesAbove", "notesBelow"] {
            let Some(notes) = line.get(side).and_then(|v| v.as_array()) else {
                continue;
            };
            for note in notes {
                let kind = note.get("type").and_then(|v| v.as_u64()).unwrap_or(0);
                if (1..=4).contains(&kind) {
                    counts[(kind - 1) as usize] += 1;
                }
                let end = note.get("time").and_then(|v| v.as_f64()).unwrap_or(0.0)
                    + note.get("holdTime").and_then(|v| v.as_f64()).unwrap_or(0.0);
                max_time = max_time.max(end);
            }
        }
    }
    (counts, max_time)
}

/// The same counts as `count_notes`, from the typed parse
fn line_counts(lines: &[LineNotes]) -> NoteCounts {
    let mut counts = [0u32; 4];
    let mut max_time = 0.0f64;
    for note in lines
        .iter()
        .flat_map(|line| line.above.iter().chain(&line.below))
    {
        if (1..=4).contains(&note.kind) {
            counts[(note.kind - 1) as usize] += 1;
        }
        max_time = max_time.max(note.time + note.hold_time);
    }
    (counts, max_time)
}

/// `info/notesInfo.json`: `{ songId: { EZ|HD|IN|AT: { t: [tap, drag, hold, flick], m: seconds } } }`
fn notes_info_json(stats: &ChartStats) -> Result<String, String> {
    let mut songs = serde_json::Map::new();
    for (folder, charts) in stats {
        let mut levels = serde_json::Map::new();
        for level in LEVELS {
            let Some((counts, max_time)) = charts.get(level) else {
                continue;
            };
            if counts.iter().all(|n| *n == 0) {
                continue;
            }
            levels.insert(
                level.into(),
                serde_json::json!({ "t": counts, "m": max_time / 1000.0 }),
            );
        }
        if !levels.is_empty() {
            songs.insert(song_id(folder).into(), Value::Object(levels));
        }
    }
    serde_json::to_string(&Value::Object(songs)).map_err(|e| e.to_string())
}

/// Rebuild notesInfo.json by parsing the chart files on disk (for output trees extract did not just write)
pub fn notes_info_from_charts(output: &Path) -> Result<String, String> {
    let mut charts = Vec::new();
    if let Ok(dirs) = fs::read_dir(output.join("chart")) {
        for dir in dirs.flatten() {
            let folder = dir.file_name().to_string_lossy().to_string();
            for level in LEVELS {
                let file = dir.path().join(format!("{level}.json"));
                if file.is_file() {
                    charts.push((folder.clone(), level, file));
                }
            }
        }
    }
    let stats: Mutex<ChartStats> = Mutex::new(BTreeMap::new());
    par::for_each(
        &charts,
        par::threads(),
        || Ok(()),
        |_, _, (folder, level, file)| {
            let Some(chart) = fs::read(file)
                .ok()
                .and_then(|b| serde_json::from_slice::<Value>(&b).ok())
            else {
                return Ok(());
            };
            let counts = count_notes(&chart);
            let mut stats = stats.lock().map_err(|e| e.to_string())?;
            stats
                .entry(folder.clone())
                .or_default()
                .insert(level.to_string(), counts);
            Ok(())
        },
    )?;
    notes_info_json(&stats.into_inner().map_err(|e| e.to_string())?)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn carves_the_sample_chart() {
        let path = "/tmp/sample.bundle";
        if !std::path::Path::new(path).exists() {
            return;
        }
        let nodes = unityfs::decode(&std::fs::read(path).unwrap()).unwrap();
        let (chart, (counts, max_time)) = carve_chart(&nodes).unwrap();
        let value: Value = serde_json::from_slice(&chart).unwrap();
        assert!(value.get("judgeLineList").is_some());
        assert!(counts.iter().sum::<u32>() > 0 && max_time > 0.0);
        // The typed fast path ends where the brace matcher does and counts what the Value path counts
        let node = nodes
            .iter()
            .find(|n| find(&n.bytes, b"{\"formatVersion\"").is_some());
        let bytes = &node.unwrap().bytes;
        let start = find(bytes, b"{\"formatVersion\"").unwrap();
        assert_eq!(chart.len(), json_end(&bytes[start..]).unwrap());
        assert_eq!((counts, max_time), count_notes(&value));
        assert_eq!(
            find(b"xx{\"formatVersion\":3}", b"{\"formatVersion\""),
            Some(2)
        );
        assert_eq!(find(b"{{{\"format", b"{\"formatVersion\""), None);
    }

    #[test]
    fn charts_the_typed_parse_cannot_read_fall_back_to_the_value_path() {
        let chart = |body: &str| unityfs::Node {
            name: "CAB".into(),
            bytes: format!("junk{{\"formatVersion\":3,{body}}}tail").into_bytes(),
        };
        // A float note type counts as no type, null notes are skipped, a null line list still counts
        let odd = r#""judgeLineList":[{"notesAbove":[{"type":1.0,"time":5},{"type":2,"time":7,"holdTime":1}],"notesBelow":null}]"#;
        let (body, (counts, max_time)) = carve_chart(&[chart(odd)]).unwrap();
        assert_eq!(body, format!("{{\"formatVersion\":3,{odd}}}").into_bytes());
        assert_eq!((counts, max_time), ([0, 1, 0, 0], 8.0));
        let (_, (counts, _)) = carve_chart(&[chart(r#""judgeLineList":null"#)]).unwrap();
        assert_eq!(counts, [0; 4]);
        let plain = r#""judgeLineList":[{"notesAbove":[{"type":3,"time":2,"holdTime":4}],"notesBelow":[{"type":4,"time":9}]}]"#;
        let (_, stats) = carve_chart(&[chart(plain)]).unwrap();
        assert_eq!(stats, ([0, 0, 1, 1], 9.0));
        assert!(carve_chart(&[chart(r#""other":1"#)]).is_err());
    }
}
