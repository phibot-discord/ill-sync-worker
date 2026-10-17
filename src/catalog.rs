use crate::collection_art;
use crate::layout::LEVELS;
use crate::pv;
use serde_json::Value;
use std::collections::{BTreeMap, HashSet};

const DEFAULT_JACKET_LEVELS: [&str; 4] = ["IN", "HD", "EZ", "AT"];

#[derive(Clone, Debug)]
pub enum Want {
    Chart {
        folder: String,
        diff: String,
    },
    Jacket {
        folder: String,
        level: Option<String>,
    },
    Music {
        folder: String,
    },
    Avatar {
        name: String,
    },
    Video {
        group: String,
        name: String,
    },
    CollectionArt {
        id: String,
        source: String,
    },
}

#[derive(Clone, Debug)]
pub struct BundleJob {
    pub zip_path: String,
    pub want: Want,
}

pub fn jobs(catalog: &Value) -> Result<Vec<BundleJob>, String> {
    let mut jobs: Vec<BundleJob> = locate(catalog, classify)?
        .into_iter()
        .map(|(want, zip_path)| BundleJob { zip_path, want })
        .collect();
    if jobs.is_empty() {
        return Err("catalog listed no track bundles".into());
    }
    let defaults = default_jackets(&jobs);
    jobs.extend(defaults);
    Ok(jobs)
}

pub fn locate<T>(
    catalog: &Value,
    mut select: impl FnMut(&str) -> Option<T>,
) -> Result<Vec<(T, String)>, String> {
    let loc = find_locator(catalog).ok_or("catalog has no addressable locator")?;
    let keys = decode_b64(loc["m_KeyDataString"].as_str().ok_or("m_KeyDataString")?)?;
    let buckets = decode_b64(
        loc["m_BucketDataString"]
            .as_str()
            .ok_or("m_BucketDataString")?,
    )?;
    let entries = decode_b64(
        loc["m_EntryDataString"]
            .as_str()
            .ok_or("m_EntryDataString")?,
    )?;
    let internals = loc["m_InternalIds"].as_array().ok_or("m_InternalIds")?;
    let key_at = parse_keys(&keys)?;
    let (key_entry, firsts) = parse_buckets(&buckets, &key_at)?;
    let mut found = Vec::new();
    for (key, entry_index) in key_entry {
        let Some(want) = select(&key) else { continue };
        let entry = read_entry(&entries, entry_index)?;
        let bundle_entry = if entry[1] == 0 || entry[1] == 1 {
            entry
        } else if entry[2] >= 0 {
            // An asset's dependency key lists its own bundle first, then the bundles it needs (a scene's art, say)
            match firsts.get(entry[2] as usize) {
                Some(&first) if first >= 0 => read_entry(&entries, first)?,
                Some(_) => continue,
                None => return Err(format!("{key}: dependency key index {}", entry[2])),
            }
        } else {
            continue;
        };
        let internal = internals
            .get(bundle_entry[0] as usize)
            .and_then(|v| v.as_str())
            .unwrap_or("");
        if internal.is_empty() {
            continue;
        }
        found.push((want, bundle_zip_path(internal)));
    }
    Ok(found)
}

/// A song with per-difficulty jackets and no plain `Illustration.jpg` (its base file is a
/// `.c9Locked` placeholder) still gets the plain jacket the web UI looks up first: its IN jacket,
/// else the next level in `DEFAULT_JACKET_LEVELS`
fn default_jackets(jobs: &[BundleJob]) -> Vec<BundleJob> {
    let mut plain = HashSet::new();
    let mut levelled: BTreeMap<&str, Vec<(&str, &str)>> = BTreeMap::new();
    for job in jobs {
        let Want::Jacket { folder, level } = &job.want else {
            continue;
        };
        match level {
            None => {
                plain.insert(folder.as_str());
            }
            Some(level) => levelled
                .entry(folder.as_str())
                .or_default()
                .push((level.as_str(), job.zip_path.as_str())),
        }
    }
    levelled
        .into_iter()
        .filter(|(folder, _)| !plain.contains(folder))
        .filter_map(|(folder, found)| {
            let (_, zip_path) = DEFAULT_JACKET_LEVELS
                .iter()
                .find_map(|want| found.iter().find(|(level, _)| level == want))?;
            Some(BundleJob {
                zip_path: zip_path.to_string(),
                want: Want::Jacket {
                    folder: folder.to_string(),
                    level: None,
                },
            })
        })
        .collect()
}

fn classify(key: &str) -> Option<Want> {
    if let Some(rest) = key.strip_prefix("Assets/Tracks/") {
        let (folder, file) = rest.rsplit_once('/')?;
        if let Some(diff) = file
            .strip_prefix("Chart_")
            .and_then(|s| s.strip_suffix(".json"))
        {
            // Any level the game ships (EZ…AT, Legacy, an unlock's SP or `<LV>_Error`): it becomes a path and a key
            if !diff.is_empty() && diff.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'_') {
                return Some(Want::Chart {
                    folder: folder.to_string(),
                    diff: diff.to_string(),
                });
            }
        }
        if file == "Illustration.jpg" || file == "Illustration.png" {
            return Some(Want::Jacket {
                folder: folder.to_string(),
                level: None,
            });
        }
        // `IllustrationLowRes_<LV>` and `IllustrationBlur_<LV>` do not match: those are derived
        if let Some(level) = file
            .strip_prefix("Illustration_")
            .and_then(|s| s.strip_suffix(".jpg").or_else(|| s.strip_suffix(".png")))
            .filter(|level| LEVELS.contains(level))
        {
            return Some(Want::Jacket {
                folder: folder.to_string(),
                level: Some(level.to_string()),
            });
        }
        if file.starts_with("music.") {
            return Some(Want::Music {
                folder: folder.to_string(),
            });
        }
        return None;
    }
    if let Some(name) = key.strip_prefix("avatar.") {
        if !name.is_empty() && !name.contains('/') {
            return Some(Want::Avatar {
                name: name.to_string(),
            });
        }
    }
    if let Some((group, name)) = pv::addressable(key) {
        return Some(Want::Video {
            group,
            name: name.to_string(),
        });
    }
    if let Some(id) = collection_art::addressable(key) {
        return Some(Want::CollectionArt {
            id: id.to_string(),
            source: key.to_string(),
        });
    }
    None
}

fn bundle_zip_path(internal: &str) -> String {
    internal.replace(
        "{UnityEngine.AddressableAssets.Addressables.RuntimePath}",
        "assets/aa",
    )
}

fn find_locator(value: &Value) -> Option<&Value> {
    if value.get("m_KeyDataString").is_some() {
        return Some(value);
    }
    match value {
        Value::Object(map) => map.values().find_map(find_locator),
        Value::Array(list) => list.iter().find_map(find_locator),
        _ => None,
    }
}

fn parse_keys(keys: &[u8]) -> Result<Vec<(i32, String)>, String> {
    if keys.len() < 4 {
        return Err("empty key blob".into());
    }
    let count = i32::from_le_bytes(keys[0..4].try_into().unwrap());
    if count < 0 {
        return Err("bad key count".into());
    }
    let mut i = 4;
    let mut out = Vec::with_capacity(count as usize);
    for _ in 0..count {
        let off = i as i32;
        let kind = *keys.get(i).ok_or("truncated key")?;
        i += 1;
        match kind {
            0 | 1 => {
                let len = u32::from_le_bytes(
                    keys.get(i..i + 4)
                        .ok_or("truncated key")?
                        .try_into()
                        .unwrap(),
                ) as usize;
                i += 4;
                let raw = keys.get(i..i + len).ok_or("truncated key string")?;
                i += len;
                let text = if kind == 0 {
                    String::from_utf8_lossy(raw).into_owned()
                } else {
                    let units: Vec<u16> = raw
                        .chunks_exact(2)
                        .map(|c| u16::from_le_bytes([c[0], c[1]]))
                        .collect();
                    String::from_utf16_lossy(&units)
                };
                out.push((off, text));
            }
            4 => {
                i += 4;
            }
            other => return Err(format!("unknown addressable key type {other}")),
        }
    }
    Ok(out)
}

/// Each string key with its first entry, and every bucket's first entry by index (-1 for none)
type Buckets = (Vec<(String, i32)>, Vec<i32>);

fn parse_buckets(buckets: &[u8], keys: &[(i32, String)]) -> Result<Buckets, String> {
    let count = i32::from_le_bytes(buckets.get(0..4).ok_or("buckets")?.try_into().unwrap());
    let mut i = 4;
    let mut map = std::collections::HashMap::new();
    for (off, text) in keys {
        map.insert(*off, text.clone());
    }
    let mut out = Vec::new();
    let mut firsts = Vec::new();
    for _ in 0..count {
        let off = i32::from_le_bytes(buckets.get(i..i + 4).ok_or("bucket")?.try_into().unwrap());
        i += 4;
        let n = i32::from_le_bytes(buckets.get(i..i + 4).ok_or("bucket")?.try_into().unwrap());
        i += 4;
        if n < 0 {
            return Err("bad bucket".into());
        }
        let first = if n == 0 {
            firsts.push(-1);
            continue;
        } else {
            let idx = i32::from_le_bytes(
                buckets
                    .get(i..i + 4)
                    .ok_or("bucket idx")?
                    .try_into()
                    .unwrap(),
            );
            i += 4 * n as usize;
            idx
        };
        firsts.push(first);
        if let Some(text) = map.get(&off) {
            out.push((text.clone(), first));
        }
    }
    Ok((out, firsts))
}

fn read_entry(entries: &[u8], index: i32) -> Result<[i32; 7], String> {
    let at = 4 + index as usize * 28;
    let raw = entries.get(at..at + 28).ok_or("entry index")?;
    let mut nums = [0i32; 7];
    for (i, chunk) in raw.chunks_exact(4).enumerate() {
        nums[i] = i32::from_le_bytes(chunk.try_into().unwrap());
    }
    Ok(nums)
}

fn decode_b64(text: &str) -> Result<Vec<u8>, String> {
    fn val(c: u8) -> Result<u8, String> {
        Ok(match c {
            b'A'..=b'Z' => c - b'A',
            b'a'..=b'z' => c - b'a' + 26,
            b'0'..=b'9' => c - b'0' + 52,
            b'+' | b'-' => 62,
            b'/' | b'_' => 63,
            _ => return Err("bad base64".into()),
        })
    }
    let bytes: Vec<u8> = text.bytes().filter(|b| !b.is_ascii_whitespace()).collect();
    let mut out = Vec::with_capacity(bytes.len() * 3 / 4);
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'=' {
            break;
        }
        let b0 = val(bytes[i])?;
        let b1 = val(*bytes.get(i + 1).ok_or("bad base64")?)?;
        let b2 = if i + 2 < bytes.len() && bytes[i + 2] != b'=' {
            Some(val(bytes[i + 2])?)
        } else {
            None
        };
        let b3 = if i + 3 < bytes.len() && bytes[i + 3] != b'=' {
            Some(val(bytes[i + 3])?)
        } else {
            None
        };
        out.push((b0 << 2) | (b1 >> 4));
        if let Some(b2) = b2 {
            out.push((b1 << 4) | (b2 >> 2));
            if let Some(b3) = b3 {
                out.push((b2 << 6) | b3);
            }
        }
        i += 4;
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    const HAPPY: &str = "Assets/Tracks/Happy.Apollo.0";

    fn jacket(key: &str) -> Option<(String, Option<String>)> {
        match classify(&format!("{HAPPY}/{key}")) {
            Some(Want::Jacket { folder, level }) => Some((folder, level)),
            _ => None,
        }
    }

    #[test]
    fn classifies_plain_and_per_difficulty_jackets() {
        let folder = "Happy.Apollo.0".to_string();
        assert_eq!(jacket("Illustration.jpg"), Some((folder.clone(), None)));
        assert_eq!(jacket("Illustration.png"), Some((folder.clone(), None)));
        for level in ["EZ", "HD", "IN", "AT"] {
            assert_eq!(
                jacket(&format!("Illustration_{level}.jpg")),
                Some((folder.clone(), Some(level.to_string())))
            );
        }
        // Derived, locked or unknown files are not jackets
        for other in [
            "Illustration.jpg.c9Locked",
            "IllustrationLowRes_IN.jpg",
            "IllustrationBlur_IN.jpg",
            "IllustrationLowRes.jpg",
            "Illustration_Legacy.jpg",
            "Illustration_.jpg",
        ] {
            assert_eq!(jacket(other), None, "{other}");
        }
    }

    #[test]
    fn classifies_every_chart_level_as_a_chart() {
        let chart = |key: &str| match classify(key) {
            Some(Want::Chart { folder, diff }) => Some((folder, diff)),
            _ => None,
        };
        for (folder, diff) in [
            ("Message.くるぶっこちゃん.0", "SP"),
            ("望影の方舟Six.SeURa.0", "EZ_Error"),
            ("望影の方舟Six.SeURa.0", "HD_Error"),
            ("望影の方舟Six.SeURa.0", "IN_Error"),
            ("Happy.Apollo.0", "IN"),
            ("Happy.Apollo.0", "Legacy"),
            ("Happy.Apollo.0", "AT_Error"),
            ("Happy.Apollo.0", "Error"),
        ] {
            let want = Want::Chart {
                folder: folder.into(),
                diff: diff.into(),
            };
            assert_eq!(
                chart(&format!("Assets/Tracks/{folder}/Chart_{diff}.json")),
                Some((folder.to_string(), diff.to_string()))
            );
            assert_eq!(
                crate::layout::output_path(&want),
                format!("chart/{folder}/{diff}.json")
            );
        }
        for file in [
            "Chart_.json",
            "Chart_SP.json.c9Locked",
            "Chart_a b.json",
            "Chart_x.y.json",
            "Chart.json",
        ] {
            assert_eq!(chart(&format!("{HAPPY}/{file}")), None, "{file}");
        }
    }

    #[test]
    fn classifies_the_intro_clips() {
        match classify("c9.video.Entrance to the Chaos_Intro") {
            Some(Want::Video { group, name }) => {
                assert_eq!(group, "Chapter9");
                assert_eq!(name, "Entrance to the Chaos_Intro");
            }
            other => panic!("{other:?}"),
        }
        for key in [
            "c9.video.",
            "c9.video.a/b",
            "c.video.a",
            "cx.video.a",
            "c9s.test",
            "chapter9.sound.glitch",
        ] {
            assert!(!matches!(classify(key), Some(Want::Video { .. })), "{key}");
        }
    }

    #[test]
    fn classifies_collection_art() {
        for (key, want) in [
            ("Assets/Chapter9/Arts/liangrenhuihe0.jpg", "liangrenhuihe0"),
            ("Assets/Chapter12/Arts/x0.png", "x0"),
        ] {
            match classify(key) {
                Some(Want::CollectionArt { id, source }) => {
                    assert_eq!((id.as_str(), source.as_str()), (want, key));
                }
                other => panic!("{key}: {other:?}"),
            }
        }
        for key in [
            "Assets/Chapter9/Arts/",
            "Assets/Chapter9/Arts/a/b.jpg",
            "Assets/ChapterX/Arts/a.jpg",
            "Assets/Chapter9/Art/a.jpg",
            "c9s.test",
            "c9s.vK7mQp2H",
        ] {
            assert!(classify(key).is_none(), "{key}");
        }
    }

    fn job(folder: &str, level: Option<&str>) -> BundleJob {
        BundleJob {
            zip_path: format!("{folder}/{}", level.unwrap_or("plain")),
            want: Want::Jacket {
                folder: folder.into(),
                level: level.map(String::from),
            },
        }
    }

    #[test]
    fn a_song_with_only_per_difficulty_jackets_gets_a_plain_one() {
        let jobs = vec![
            job("Levelled.A.0", Some("EZ")),
            job("Levelled.A.0", Some("IN")),
            job("Levelled.A.0", Some("AT")),
            job("NoIn.B.0", Some("AT")),
            job("NoIn.B.0", Some("HD")),
            // Has its own plain jacket: nothing is added
            job("Both.C.0", None),
            job("Both.C.0", Some("IN")),
            job("Plain.D.0", None),
        ];
        let added = default_jackets(&jobs);
        let pairs: Vec<_> = added
            .iter()
            .map(|j| match &j.want {
                Want::Jacket { folder, level } => (folder.as_str(), level.clone(), &j.zip_path),
                _ => unreachable!(),
            })
            .collect();
        assert_eq!(
            pairs,
            vec![
                ("Levelled.A.0", None, &"Levelled.A.0/IN".to_string()),
                ("NoIn.B.0", None, &"NoIn.B.0/HD".to_string()),
            ]
        );
    }

    #[test]
    fn resolves_a_chart_bundle_from_the_401_catalog() {
        let path = "/tmp/catalog.json";
        if !std::path::Path::new(path).exists() {
            return;
        }
        let value: Value = serde_json::from_slice(&std::fs::read(path).unwrap()).unwrap();
        let jobs = jobs(&value).unwrap();
        assert!(jobs.len() > 1000, "{}", jobs.len());
        assert!(jobs.iter().any(|j| matches!(j.want, Want::Chart { .. })
            && j.zip_path.contains("assets/aa/Android/")
            && j.zip_path.ends_with(".bundle")));
        assert!(jobs.iter().any(|j| matches!(&j.want, Want::Jacket { folder, .. } if folder.contains("Exoplanetary") || folder.contains("Glaciaxion"))));
    }

    #[test]
    fn the_401_catalogs_levels_beyond_the_song_table_each_sit_in_a_bundle_of_their_own() {
        let path = "/tmp/catalog.json";
        if !std::path::Path::new(path).exists() {
            return;
        }
        let value: Value = serde_json::from_slice(&std::fs::read(path).unwrap()).unwrap();
        let jobs = jobs(&value).unwrap();
        let charts: Vec<(&str, &str, &str)> = jobs
            .iter()
            .filter_map(|j| match &j.want {
                Want::Chart { folder, diff } => {
                    Some((folder.as_str(), diff.as_str(), j.zip_path.as_str()))
                }
                _ => None,
            })
            .collect();
        let mut unlock: Vec<(&str, &str)> = charts
            .iter()
            .filter(|(_, diff, _)| !LEVELS.contains(diff) && *diff != "Legacy")
            .map(|(folder, diff, _)| (*folder, *diff))
            .collect();
        unlock.sort();
        assert_eq!(
            unlock,
            [
                ("Message.くるぶっこちゃん.0", "SP"),
                ("望影の方舟Six.SeURa.0", "EZ_Error"),
                ("望影の方舟Six.SeURa.0", "HD_Error"),
                ("望影の方舟Six.SeURa.0", "IN_Error"),
            ]
        );
        for (folder, diff) in unlock {
            let bundle = charts
                .iter()
                .find(|(f, d, _)| (*f, *d) == (folder, diff))
                .unwrap()
                .2;
            let sharing: Vec<_> = charts.iter().filter(|(_, _, z)| *z == bundle).collect();
            assert_eq!(sharing.len(), 1, "{folder} {diff}: {sharing:?}");
        }
    }
}
