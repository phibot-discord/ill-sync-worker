use crate::cursor::Cursor;
use serde::Serialize;
use serde_json::{json, Map, Value};
use std::collections::BTreeMap;

#[derive(Clone, Debug)]
pub struct Song {
    pub id: String,
    pub name: String,
    pub composer: String,
    pub illustrator: String,
    pub preview_start: f32,
    pub preview_end: f32,
    pub levels: Vec<Level>,
}

#[derive(Clone, Debug)]
pub struct Level {
    pub name: String,
    pub charter: String,
    pub difficulty: f32,
}

#[derive(Clone, Debug)]
pub struct Avatar {
    pub name: String,
    pub key: String,
}

const LEVEL_ORDER: [&str; 5] = ["EZ", "HD", "IN", "AT", "Legacy"];

pub fn songs_json(songs: &[Song]) -> Value {
    let list: Vec<Value> = songs
        .iter()
        .map(|song| {
            let mut levels = Map::new();
            for level in &song.levels {
                if level.difficulty <= 0.001 && level.charter.is_empty() {
                    continue;
                }
                if level.difficulty <= 0.001 {
                    continue;
                }
                levels.insert(
                    level.name.clone(),
                    json!({
                        "charter": level.charter,
                        "difficulty": round1(level.difficulty),
                    }),
                );
            }
            json!({
                "id": song.id,
                "key": song.id.split_once('.').map(|(k, _)| k).unwrap_or(song.id.as_str()),
                "name": song.name,
                "composer": song.composer,
                "illustrator": song.illustrator,
                "preview_time": round2(song.preview_start),
                "preview_end_time": round2(song.preview_end),
                "levels": levels,
            })
        })
        .collect();
    Value::Array(list)
}

pub fn tips_json(tips: &[String]) -> Value {
    json!({ "zh_cn": tips })
}

pub fn avatars_json(avatars: &[Avatar]) -> Value {
    Value::Array(
        avatars
            .iter()
            .map(|a| json!({ "name": a.name, "addressable_key": a.key }))
            .collect(),
    )
}

fn round1(v: f32) -> f64 {
    (v as f64 * 10.0).round() / 10.0
}

fn round2(v: f32) -> f64 {
    (v as f64 * 100.0).round() / 100.0
}

pub fn parse_songs(data: &[u8]) -> Result<Vec<Song>, String> {
    let mut off = 0;
    while off + 32 < data.len() {
        if let Some(songs) = parse_run(data, off, 3) {
            if songs[0].id.ends_with(".0") {
                let all = parse_until_end(data, off)?;
                if all.len() >= 3 {
                    return Ok(all);
                }
            }
        }
        off += 4;
    }
    Err("no song list in level0".into())
}

fn parse_until_end(data: &[u8], start: usize) -> Result<Vec<Song>, String> {
    let mut songs = Vec::new();
    let mut at = start;
    loop {
        let (song, after) = parse_one(data, at)?;
        let next = next_song_at(data, after);
        songs.push(song);
        let Some(next) = next else {
            break;
        };
        at = next;
        if songs.len() > 5000 {
            return Err("song list did not end".into());
        }
    }
    Ok(songs)
}

fn next_song_at(data: &[u8], after: usize) -> Option<usize> {
    let mut off = 0;
    while off < 2000 {
        let at = after + off;
        if at + 16 >= data.len() {
            return None;
        }
        if parse_one(data, at).is_ok() {
            return Some(at);
        }
        off += 4;
    }
    None
}

fn parse_run(data: &[u8], start: usize, n: usize) -> Option<Vec<Song>> {
    let mut songs = Vec::new();
    let mut at = start;
    for _ in 0..n {
        let (song, after) = parse_one(data, at).ok()?;
        let next = next_song_at(data, after)?;
        songs.push(song);
        at = next;
    }
    Some(songs)
}

fn parse_one(data: &[u8], at: usize) -> Result<(Song, usize), String> {
    let mut c = Cursor::new(data);
    c.seek(at)?;
    let id = c.string()?;
    if id.is_empty() || id.len() > 400 || id.chars().any(|ch| ch.is_control()) {
        return Err("bad id".into());
    }
    let mut names = Vec::new();
    for _ in 0..3 {
        names.push(c.string()?);
    }
    let count = c.i32()?;
    if !(3..=6).contains(&count) {
        return Err("bad difficulty count".into());
    }
    let mut diffs = Vec::new();
    for _ in 0..count {
        let d = c.f32()?;
        if !(0.0..=25.0).contains(&d) {
            return Err("bad difficulty".into());
        }
        diffs.push(d);
    }
    let illustrator = c.string()?;
    let charters_n = c.i32()?;
    if !(3..=6).contains(&charters_n) {
        return Err("bad charter count".into());
    }
    let mut charters = Vec::new();
    for _ in 0..charters_n {
        charters.push(c.string()?);
    }
    let composer = c.string()?;
    let levels_n = c.i32()?;
    if !(3..=6).contains(&levels_n) {
        return Err("bad level count".into());
    }
    let mut level_names = Vec::new();
    for _ in 0..levels_n {
        let name = c.string()?;
        if !LEVEL_ORDER.contains(&name.as_str()) && name.len() > 12 {
            return Err("bad level name".into());
        }
        level_names.push(name);
    }
    let preview_start = c.f32()?;
    let preview_end = c.f32()?;
    if !(0.0..10_000.0).contains(&preview_start) || !(0.0..10_000.0).contains(&preview_end) {
        return Err("bad preview".into());
    }
    let n = count.min(charters_n).min(levels_n) as usize;
    let mut levels = Vec::new();
    for i in 0..n {
        levels.push(Level {
            name: level_names[i].clone(),
            charter: charters[i].clone(),
            difficulty: diffs[i],
        });
    }
    let name = names
        .into_iter()
        .find(|s| !s.is_empty())
        .unwrap_or_default();
    Ok((
        Song {
            id,
            name,
            composer,
            illustrator,
            preview_start,
            preview_end,
            levels,
        },
        c.pos(),
    ))
}

/// Unity's `SystemLanguage` values that key `TipsProvider`'s lists, as the Collection's language codes
const TIP_LANGS: [(i32, &str); 5] = [
    (40, "zh-CN"),
    (41, "zh-TW"),
    (10, "en"),
    (22, "ja"),
    (23, "ko"),
];

/// The loading-screen tips in Simplified Chinese
pub fn parse_tips(data: &[u8]) -> Result<Vec<String>, String> {
    parse_tip_lists(data)?
        .remove("zh-CN")
        .ok_or_else(|| "tips list not found".into())
}

/// `TipsProvider`'s list for each game language: a `SystemLanguage` value, then a string array that starts with
/// the same `!!` tip in every language
pub fn parse_tip_lists(data: &[u8]) -> Result<BTreeMap<&'static str, Vec<String>>, String> {
    let needle = b"\x02\x00\x00\x00!!";
    let mut lists = BTreeMap::new();
    let mut from = 0;
    while let Some(rel) = data[from..].windows(needle.len()).position(|w| w == needle) {
        let at = from + rel;
        from = at + 1;
        if at < 8 {
            continue;
        }
        let count = i32::from_le_bytes(data[at - 4..at].try_into().unwrap());
        if !(20..=500).contains(&count) {
            continue;
        }
        let language = i32::from_le_bytes(data[at - 8..at - 4].try_into().unwrap());
        let Some((_, lang)) = TIP_LANGS.iter().find(|(value, _)| *value == language) else {
            continue;
        };
        if let Ok(tips) = read_string_array(data, at - 4) {
            if tips.iter().any(|t| t.contains("Sound Only")) {
                lists.entry(*lang).or_insert(tips);
            }
        }
    }
    if !lists.contains_key("zh-CN") {
        return Err("tips list not found".into());
    }
    Ok(lists)
}

pub const TIPS_MANIFEST: &str = "tips/index.json";

/// `tips/index.json`: every game language's loading tips
pub fn tip_lists_json(lists: &BTreeMap<&'static str, Vec<String>>) -> Value {
    json!({ "v": 1, "langs": lists })
}

fn read_string_array(data: &[u8], at: usize) -> Result<Vec<String>, String> {
    let mut c = Cursor::new(data);
    c.seek(at)?;
    let count = c.i32()?;
    if !(1..=500).contains(&count) {
        return Err("bad tip count".into());
    }
    let mut tips = Vec::with_capacity(count as usize);
    for _ in 0..count {
        let s = c.string()?;
        if s.len() > 400 {
            return Err("tip too long".into());
        }
        tips.push(s);
    }
    Ok(tips)
}

pub fn parse_avatars(data: &[u8]) -> Result<Vec<Avatar>, String> {
    let mut avatars = Vec::new();
    let mut from = 0;
    let needle = b"avatar.";
    while let Some(rel) = data[from..].windows(needle.len()).position(|w| w == needle) {
        let at = from + rel;
        if at >= 4 {
            let len = i32::from_le_bytes(data[at - 4..at].try_into().unwrap());
            if (8..80).contains(&len) {
                let end = at + len as usize;
                if end <= data.len() && !data[at..end].contains(&0) {
                    if let Ok(key) = std::str::from_utf8(&data[at..end]) {
                        if let Some(name) = previous_string(data, at - 4) {
                            avatars.push(Avatar {
                                name,
                                key: key.to_string(),
                            });
                        }
                    }
                }
            }
        }
        from = at + 1;
    }
    if avatars.is_empty() {
        return Err("no avatars in level0".into());
    }
    Ok(avatars)
}

fn previous_string(data: &[u8], aligned_end: usize) -> Option<String> {
    let mut len_pos = aligned_end.saturating_sub(80);
    while len_pos + 4 < aligned_end {
        let len = i32::from_le_bytes(data.get(len_pos..len_pos + 4)?.try_into().ok()?);
        if (1..=60).contains(&len) {
            let data_at = len_pos + 4;
            let end = data_at + len as usize;
            let aligned = (end + 3) & !3;
            if aligned == aligned_end {
                let raw = data.get(data_at..end)?;
                if !raw.contains(&0) {
                    if let Ok(name) = std::str::from_utf8(raw) {
                        return Some(name.to_string());
                    }
                }
            }
        }
        len_pos += 1;
    }
    None
}

#[derive(Serialize)]
pub struct VersionFile {
    pub code: i64,
    pub name: String,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reads_the_401_level0_when_present() {
        let path = "/tmp/level0.bin";
        if !std::path::Path::new(path).exists() {
            return;
        }
        let data = std::fs::read(path).unwrap();
        let songs = parse_songs(&data).unwrap();
        assert!(songs.len() >= 328, "{}", songs.len());
        assert_eq!(songs[0].id, "Glaciaxion.SunsetRay.0");
        assert_eq!(songs[0].illustrator, "艾若拉");
        assert!(songs.iter().any(|s| s.id == "Introduction"));
        assert!(songs.iter().any(|s| s.id.starts_with("Ametrine.")));
        let exo = songs
            .iter()
            .find(|s| s.id.starts_with("ExoplanetaryMirage."))
            .unwrap();
        let inn = exo.levels.iter().find(|l| l.name == "IN").unwrap();
        assert!((inn.difficulty - 16.8).abs() < 0.05, "{}", inn.difficulty);
        let tips = parse_tips(&data).unwrap();
        assert!(tips.len() >= 100, "{}", tips.len());
        assert_eq!(tips[0], "!!");
        let avatars = parse_avatars(&data).unwrap();
        assert_eq!(avatars.len(), 109, "{}", avatars.len());
        assert_eq!(avatars[0].key, "avatar.Glaciaxion");
    }

    #[test]
    fn reads_the_tips_of_every_language_in_the_401_apk() {
        let Ok(level0) = std::fs::read("/tmp/phi-fixtures/level0") else {
            return;
        };
        let lists = parse_tip_lists(&level0).unwrap();
        let counts: Vec<(&str, usize)> = lists.iter().map(|(k, v)| (*k, v.len())).collect();
        assert_eq!(
            counts,
            [
                ("en", 130),
                ("ja", 192),
                ("ko", 187),
                ("zh-CN", 192),
                ("zh-TW", 192)
            ]
        );
        assert_eq!(lists["en"][2], "(•▴• )Coo coo~");
        assert_eq!(parse_tips(&level0).unwrap(), lists["zh-CN"]);
        let json = tip_lists_json(&lists);
        assert_eq!(json["v"], 1);
        assert_eq!(json["langs"]["ko"][2], "(•▴• )구구~");
    }
}
