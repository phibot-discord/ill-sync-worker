use crate::files::{self, write_if_changed};
use crate::par;
use std::collections::HashSet;
use std::fs;
use std::io::{Cursor, Read, Write};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Mutex;
use zip::write::FileOptions;
use zip::{CompressionMethod, DateTime, ZipArchive, ZipWriter};

const LEVELS: [&str; 4] = ["EZ", "HD", "IN", "AT"];

pub fn info_text(
    id: &str,
    name: &str,
    level: &str,
    difficulty: &str,
    composer: &str,
    illustrator: &str,
    charter: &str,
) -> String {
    format!(
        "#\nName: {name}\nSong: {id}.ogg\nPicture: {id}.png\nChart: {id}.json\nLevel: {level} Lv.{difficulty}\nComposer: {composer}\nIllustrator: {illustrator}\nCharter: {charter}"
    )
}

fn opts(method: CompressionMethod) -> FileOptions {
    let when = DateTime::from_date_and_time(2025, 1, 1, 0, 0, 0).unwrap_or_default();
    FileOptions::default()
        .compression_method(method)
        .last_modified_time(when)
        .unix_permissions(0o644)
}

pub fn pack_one(
    path: &Path,
    id: &str,
    info: &str,
    chart: &[u8],
    picture: &[u8],
    song: &[u8],
) -> Result<(), String> {
    let json = format!("{id}.json");
    let png = format!("{id}.png");
    let ogg = format!("{id}.ogg");
    let entries = [
        Entry("info.txt", CompressionMethod::Deflated, info.as_bytes()),
        Entry(&json, CompressionMethod::Deflated, chart),
        Entry(&png, CompressionMethod::Stored, picture),
        Entry(&ogg, CompressionMethod::Stored, song),
    ];
    // Deflating the chart is most of the cost of a pack; reading the old pack back is several times cheaper
    if pack_matches(path, &entries) {
        return Ok(());
    }
    write_if_changed(path, &pez_bytes(&entries)?).map(|_| ())
}

/// One file in a pack: name, compression, contents
struct Entry<'a>(&'a str, CompressionMethod, &'a [u8]);

/// The pack is built in memory so an unchanged pack is not rewritten (its bytes are deterministic)
fn pez_bytes(entries: &[Entry]) -> Result<Vec<u8>, String> {
    let capacity = entries.iter().map(|e| e.2.len()).sum::<usize>() + 1024;
    let mut zip = ZipWriter::new(Cursor::new(Vec::with_capacity(capacity)));
    for Entry(name, method, bytes) in entries {
        zip.start_file(*name, opts(*method))
            .map_err(|e| e.to_string())?;
        zip.write_all(bytes).map_err(|e| e.to_string())?;
    }
    Ok(zip.finish().map_err(|e| e.to_string())?.into_inner())
}

/// Whether `path` is already a pack of exactly `entries`, in order, with the same compression and
/// contents (zip also checks each entry's CRC while reading). Other zip metadata is not compared
fn pack_matches(path: &Path, entries: &[Entry]) -> bool {
    let Ok(file) = fs::File::open(path) else {
        return false;
    };
    let Ok(mut zip) = ZipArchive::new(file) else {
        return false;
    };
    if zip.len() != entries.len() {
        return false;
    }
    let mut buf = Vec::new();
    entries
        .iter()
        .enumerate()
        .all(|(i, Entry(name, method, bytes))| {
            let Ok(mut file) = zip.by_index(i) else {
                return false;
            };
            if file.name() != *name
                || file.compression() != *method
                || file.size() != bytes.len() as u64
            {
                return false;
            }
            buf.clear();
            file.read_to_end(&mut buf).is_ok() && buf == *bytes
        })
}

struct Song {
    id: String,
    name: String,
    composer: String,
    illustrator: String,
    charters: [String; 4],
    difficulty: [String; 4],
}

/// One `phira/<LEVEL>/<id>-<LEVEL>.pez` per charted level, packed in parallel. Packs for songs or
/// levels that no longer exist are removed
pub fn pack_all(output: &Path) -> Result<usize, String> {
    let info = fs::read_to_string(output.join("info/info.tsv")).map_err(|e| e.to_string())?;
    let difficulty =
        fs::read_to_string(output.join("info/difficulty.tsv")).map_err(|e| e.to_string())?;
    let mut songs =
        std::collections::BTreeMap::<String, (String, String, String, [String; 4])>::new();
    for line in info.lines().skip(1) {
        let c: Vec<&str> = line.split('\t').collect();
        if c.len() < 12 {
            continue;
        }
        songs.insert(
            c[0].to_string(),
            (
                c[1].into(),
                c[2].into(),
                c[3].into(),
                [c[8].into(), c[9].into(), c[10].into(), c[11].into()],
            ),
        );
    }
    let mut diffs = std::collections::BTreeMap::<String, [String; 4]>::new();
    for line in difficulty.lines().skip(1) {
        let c: Vec<&str> = line.split('\t').collect();
        if c.len() < 5 {
            continue;
        }
        diffs.insert(
            c[0].into(),
            [c[1].into(), c[2].into(), c[3].into(), c[4].into()],
        );
    }
    let songs: Vec<Song> = songs
        .into_iter()
        .filter_map(|(id, (name, composer, illustrator, charters))| {
            let difficulty = diffs.get(&id)?.clone();
            Some(Song {
                id,
                name,
                composer,
                illustrator,
                charters,
                difficulty,
            })
        })
        .collect();
    let count = AtomicUsize::new(0);
    let written: Mutex<HashSet<PathBuf>> = Mutex::new(HashSet::new());
    par::for_each(
        &songs,
        par::threads(),
        || Ok(()),
        |_, _, song| {
            let id = &song.id;
            let picture_path = output.join(format!("illustrationLowRes/{id}.png"));
            let song_path = output.join(format!("music/{id}.ogg"));
            if !picture_path.is_file() || !song_path.is_file() {
                return Ok(());
            }
            let picture = fs::read(&picture_path).map_err(|e| e.to_string())?;
            let audio = fs::read(&song_path).map_err(|e| e.to_string())?;
            for (i, level) in LEVELS.iter().enumerate() {
                if song.difficulty[i].is_empty() {
                    continue;
                }
                let chart_path = output.join(format!("chart/{id}.0/{level}.json"));
                if !chart_path.is_file() {
                    continue;
                }
                let chart = fs::read(&chart_path).map_err(|e| e.to_string())?;
                let text = info_text(
                    id,
                    &song.name,
                    level,
                    &song.difficulty[i],
                    &song.composer,
                    &song.illustrator,
                    &song.charters[i],
                );
                let path = output.join(format!("phira/{level}/{id}-{level}.pez"));
                pack_one(&path, id, &text, &chart, &picture, &audio)?;
                written.lock().map_err(|e| e.to_string())?.insert(path);
                count.fetch_add(1, Ordering::Relaxed);
            }
            Ok(())
        },
    )?;
    files::prune(
        &output.join("phira"),
        &written.into_inner().map_err(|e| e.to_string())?,
    )?;
    Ok(count.into_inner())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn repacks_only_when_an_entry_changes() {
        let dir = std::env::temp_dir().join(format!("phi-pez-test-{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        let path = dir.join("IN/Song-IN.pez");
        let info = info_text("Song", "Song", "IN", "12.5", "c", "i", "ch");
        let chart = br#"{"formatVersion":3,"judgeLineList":[]}"#.repeat(200);
        pack_one(&path, "Song", &info, &chart, b"png", b"ogg").unwrap();
        let first = fs::read(&path).unwrap();
        let written = fs::metadata(&path).unwrap().modified().unwrap();
        std::thread::sleep(std::time::Duration::from_millis(20));
        pack_one(&path, "Song", &info, &chart, b"png", b"ogg").unwrap();
        assert_eq!(fs::metadata(&path).unwrap().modified().unwrap(), written);
        let mut zip = ZipArchive::new(Cursor::new(&first)).unwrap();
        let mut got = String::new();
        zip.by_name("Song.json")
            .unwrap()
            .read_to_string(&mut got)
            .unwrap();
        assert_eq!(got.as_bytes(), &chart[..]);
        pack_one(&path, "Song", &info, &chart, b"png", b"ogg2").unwrap();
        let mut zip = ZipArchive::new(fs::File::open(&path).unwrap()).unwrap();
        let mut got = Vec::new();
        zip.by_name("Song.ogg")
            .unwrap()
            .read_to_end(&mut got)
            .unwrap();
        assert_eq!(got, b"ogg2");
        let _ = fs::remove_dir_all(&dir);
    }
}
