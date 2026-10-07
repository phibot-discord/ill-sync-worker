use crate::catalog::Want;
use crate::files::write_if_changed;
use serde_json::Value;
use std::fs;
use std::path::{Path, PathBuf};

const LEVELS: [&str; 4] = ["EZ", "HD", "IN", "AT"];

pub fn should_skip(check: bool, prev_code: &str, prev_status: &str, next_code: &str) -> bool {
    check && !prev_code.is_empty() && prev_code == next_code && prev_status == "ok"
}

/// Song id from a track folder: one trailing ".0" removed, as the web UI does (`id.replace(/\.0$/, "")`)
pub fn song_id(folder: &str) -> &str {
    folder.strip_suffix(".0").unwrap_or(folder)
}

/// Where extract writes a bundle's output, relative to the output root
pub fn output_path(want: &Want) -> String {
    match want {
        Want::Chart { folder, diff } => format!("chart/{folder}/{diff}.json"),
        Want::Jacket { folder } => format!("illustration/{}.png", song_id(folder)),
        Want::Music { folder } => format!("music/{}.ogg", song_id(folder)),
        Want::Avatar { name } => format!("avatar/{name}.png"),
    }
}

pub fn write_info_tsv(output: &Path) -> Result<usize, String> {
    let songs: Value = serde_json::from_reader(
        fs::File::open(output.join("info/songs.json")).map_err(|e| e.to_string())?,
    )
    .map_err(|e| e.to_string())?;
    let songs = songs.as_array().ok_or("songs.json is not a list")?;
    let mut info =
        String::from("id\tsong\tcomposer\tillustrator\tEZ\tHD\tIN\tAT\tEZC\tHDC\tINC\tATC\n");
    let mut difficulty = String::from("id\tEZ\tHD\tIN\tAT\n");
    let mut count = 0;
    for song in songs {
        let id = text(&song["id"]);
        let id = song_id(&id);
        if id.is_empty() {
            continue;
        }
        let mut diffs = Vec::new();
        let mut charters = Vec::new();
        for level in LEVELS {
            let entry = &song["levels"][level];
            diffs.push(fmt_diff(&entry["difficulty"]));
            charters.push(text(&entry["charter"]));
        }
        info.push_str(&format!(
            "{id}\t{}\t{}\t{}\t{}\t{}\n",
            lang(&song["name"]),
            text(&song["composer"]),
            text(&song["illustrator"]),
            diffs.join("\t"),
            charters.join("\t"),
        ));
        difficulty.push_str(&format!("{id}\t{}\n", diffs.join("\t")));
        count += 1;
    }
    let dir = output.join("info");
    fs::create_dir_all(&dir).map_err(|e| e.to_string())?;
    write_if_changed(&dir.join("info.tsv"), info.as_bytes())?;
    write_if_changed(&dir.join("difficulty.tsv"), difficulty.as_bytes())?;
    Ok(count)
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
        for value in obj.values() {
            if let Some(s) = value.as_str() {
                if !s.is_empty() {
                    return s.to_string();
                }
            }
        }
        return String::new();
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

fn walk(dir: &Path, f: &mut dyn FnMut(&Path) -> Result<(), String>) -> Result<(), String> {
    if !dir.exists() {
        return Ok(());
    }
    let mut stack = vec![dir.to_path_buf()];
    while let Some(cur) = stack.pop() {
        for ent in fs::read_dir(&cur).map_err(|e| e.to_string())? {
            let ent = ent.map_err(|e| e.to_string())?;
            let path = ent.path();
            if path.is_dir() {
                stack.push(path);
            } else {
                f(&path)?;
            }
        }
    }
    Ok(())
}

pub fn list_pngs(dir: &Path) -> Vec<PathBuf> {
    let mut out = Vec::new();
    let _ = walk(dir, &mut |path| {
        if path.extension().and_then(|e| e.to_str()) == Some("png") {
            out.push(path.to_path_buf());
        }
        Ok(())
    });
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn skip_only_when_the_same_version_already_finished() {
        assert!(should_skip(true, "140", "ok", "140"));
        assert!(!should_skip(true, "140", "ok", "141"));
        assert!(!should_skip(true, "140", "error", "140"));
        assert!(!should_skip(false, "140", "ok", "140"));
    }

    #[test]
    fn maps_bundles_to_output_paths() {
        let folder = "Stasis.Maozon.0".to_string();
        assert_eq!(
            output_path(&Want::Chart {
                folder: folder.clone(),
                diff: "IN".into()
            }),
            "chart/Stasis.Maozon.0/IN.json"
        );
        assert_eq!(
            output_path(&Want::Music {
                folder: folder.clone()
            }),
            "music/Stasis.Maozon.ogg"
        );
        assert_eq!(
            output_path(&Want::Jacket { folder }),
            "illustration/Stasis.Maozon.png"
        );
        assert_eq!(
            output_path(&Want::Avatar {
                name: "Burn".into()
            }),
            "avatar/Burn.png"
        );
        assert_eq!(song_id("Re.0ne.0"), "Re.0ne");
        assert_eq!(song_id("Foo.0.0"), "Foo.0");
        assert_eq!(song_id("Introduction"), "Introduction");
    }
}
