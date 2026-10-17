//! The game's videos: Unity `VideoClip`s whose MP4 sits unchanged in a `.resource` file, so exporting one is a
//! byte slice. Written as `pv/video/<id>.mp4` with a `pv/index.json` manifest.

use crate::cursor::Cursor;
use crate::files;
use crate::serialized;
use crate::unityfs::{self, Node};
use serde_json::{json, Value};
use std::collections::{BTreeMap, HashMap, HashSet};
use std::path::{Path, PathBuf};

const VIDEO_CLIP: i32 = 329;
const BUILD_SETTINGS: i32 = 141;
pub const MANIFEST: &str = "pv/index.json";
pub const VIDEO_DIR: &str = "pv/video";

#[derive(Clone, Debug, PartialEq)]
pub struct Clip {
    pub name: String,
    pub width: u32,
    pub height: u32,
    pub fps: f64,
    pub frames: u64,
    pub audio: bool,
    pub source: String,
    pub offset: u64,
    pub size: u64,
}

#[derive(Clone, Debug)]
pub struct Entry {
    pub id: String,
    pub name: String,
    pub scene: Option<String>,
    pub group: String,
    pub clip: Clip,
}

impl Entry {
    pub fn file(&self) -> String {
        video_file(&self.id)
    }
}

pub fn video_file(id: &str) -> String {
    format!("{VIDEO_DIR}/{id}.mp4")
}

pub fn clip_id(name: &str) -> String {
    let mut out = String::new();
    let mut prev: Option<char> = None;
    for c in name.chars() {
        if !c.is_ascii_alphanumeric() {
            if !out.ends_with('-') && !out.is_empty() {
                out.push('-');
            }
        } else {
            let split = c.is_ascii_uppercase()
                && prev.is_some_and(|p| p.is_ascii_lowercase() || p.is_ascii_digit());
            if split && !out.ends_with('-') {
                out.push('-');
            }
            out.push(c.to_ascii_lowercase());
        }
        prev = Some(c);
    }
    out.trim_matches('-').to_string()
}

/// A chapter's addressable clip, `c<N>.video.<name>`: (`Chapter<N>`, name)
pub fn addressable(key: &str) -> Option<(String, &str)> {
    let (chapter, name) = key.strip_prefix('c')?.split_once(".video.")?;
    let valid = !chapter.is_empty()
        && chapter.bytes().all(|b| b.is_ascii_digit())
        && !name.is_empty()
        && !name.contains('/');
    valid.then(|| (format!("Chapter{chapter}"), name))
}

fn group_of(scene: &str) -> String {
    let parts: Vec<&str> = scene.split('/').collect();
    if let Some(i) = parts.iter().position(|p| *p == "TrackEventFile") {
        if let Some(group) = parts.get(i + 1).filter(|_| i + 2 < parts.len()) {
            return group.to_string();
        }
    }
    if let Some(chapter) = parts.iter().find(|p| p.starts_with("Chapter")) {
        return chapter.to_string();
    }
    parts
        .len()
        .checked_sub(2)
        .and_then(|i| parts.get(i))
        .map(|p| p.to_string())
        .unwrap_or_default()
}

pub fn parse_clip(object: &[u8]) -> Result<Clip, String> {
    let mut r = Cursor::new(object);
    let name = r.string()?;
    let _original_path = r.string()?;
    let _proxy = (r.u32()?, r.u32()?);
    let width = r.u32()?;
    let height = r.u32()?;
    let _aspect = (r.u32()?, r.u32()?);
    let fps = f64::from_bits(r.u64()?);
    let frames = r.u64()?;
    let _format = r.i32()?;
    let channels = r.i32()?;
    if !(0..=16).contains(&channels) {
        return Err(format!("video clip audio track count {channels}"));
    }
    r.bytes(channels as usize * 2)?;
    r.align(4);
    let rates = r.i32()?;
    if !(0..=16).contains(&rates) {
        return Err(format!("video clip sample rate count {rates}"));
    }
    r.bytes(rates as usize * 4)?;
    let languages = r.i32()?;
    if !(0..=16).contains(&languages) {
        return Err(format!("video clip language count {languages}"));
    }
    for _ in 0..languages {
        r.string()?;
    }
    let shaders = r.i32()?;
    if !(0..=64).contains(&shaders) {
        return Err(format!("video clip shader count {shaders}"));
    }
    r.bytes(shaders as usize * 12)?;
    let source = r.string()?;
    let offset = r.u64()?;
    let size = r.u64()?;
    // two booleans end the object, with no padding after them
    let _flags = r.bytes(2)?;
    if r.pos() != object.len() {
        return Err(format!(
            "video clip object has {} bytes, parsed {}",
            object.len(),
            r.pos()
        ));
    }
    if width == 0 || height == 0 || !fps.is_finite() || fps <= 0.0 || size == 0 {
        return Err(format!("video clip {name:?} has no picture"));
    }
    Ok(Clip {
        name,
        width,
        height,
        fps,
        frames,
        audio: channels > 0,
        source: source.rsplit('/').next().unwrap_or_default().to_string(),
        offset,
        size,
    })
}

fn scene_list(build_settings: &[u8]) -> Result<Vec<String>, String> {
    let mut r = Cursor::new(build_settings);
    let count = r.i32()?;
    if !(1..=10_000).contains(&count) {
        return Err(format!("build settings list {count} scenes"));
    }
    let mut scenes = Vec::with_capacity(count as usize);
    for _ in 0..count {
        scenes.push(r.string()?);
    }
    if !scenes.iter().any(|s| s.ends_with(".unity")) {
        return Err("build settings list no scenes".into());
    }
    Ok(scenes)
}

fn scene_index(node: &str) -> Option<usize> {
    node.strip_prefix("sharedassets")
        .and_then(|n| n.strip_suffix(".assets"))
        .and_then(|n| n.parse().ok())
}

fn video_clips(file: &[u8]) -> Result<Vec<Clip>, String> {
    serialized::objects(file)?
        .iter()
        .filter(|o| o.class_id == VIDEO_CLIP)
        .map(|o| parse_clip(o.bytes(file).ok_or("video clip object past the end")?))
        .collect()
}

pub fn mp4<'a>(resource: &'a [u8], clip: &Clip) -> Result<&'a [u8], String> {
    let start = usize::try_from(clip.offset).map_err(|_| "clip offset")?;
    let end = start
        .checked_add(usize::try_from(clip.size).map_err(|_| "clip size")?)
        .ok_or("clip size")?;
    let bytes = resource
        .get(start..end)
        .ok_or_else(|| format!("{}: clip is past the end of {}", clip.name, clip.source))?;
    if bytes.get(4..8) != Some(b"ftyp") {
        return Err(format!("{}: not an MP4", clip.name));
    }
    Ok(bytes)
}

pub fn clip_in_bundle(nodes: &[Node]) -> Result<(Clip, &[u8]), String> {
    for node in nodes.iter().filter(|n| !unityfs::is_resource(&n.name)) {
        let Some(clip) = video_clips(&node.bytes)
            .ok()
            .and_then(|c| c.into_iter().next())
        else {
            continue;
        };
        let resource = nodes
            .iter()
            .find(|n| n.name.rsplit('/').next() == Some(clip.source.as_str()))
            .ok_or_else(|| format!("bundle has no {}", clip.source))?;
        let bytes = mp4(&resource.bytes, &clip)?;
        return Ok((clip, bytes));
    }
    Err("bundle has no video clip".into())
}

fn unique_ids(entries: &mut [Entry]) {
    let mut seen: HashMap<String, usize> = HashMap::new();
    for entry in entries.iter_mut() {
        let n = seen.entry(entry.id.clone()).or_insert(0);
        *n += 1;
        if *n > 1 {
            entry.id = format!("{}-{}", entry.id, n);
        }
    }
}

pub fn scenes_in(nodes: &HashMap<String, Vec<u8>>) -> Result<Vec<String>, String> {
    let settings = nodes
        .get("globalgamemanagers")
        .ok_or("no globalgamemanagers")?;
    let build = serialized::objects(settings)?
        .into_iter()
        .find(|o| o.class_id == BUILD_SETTINGS)
        .ok_or("no build settings")?;
    scene_list(build.bytes(settings).ok_or("build settings past the end")?)
}

pub fn export_player_clips(
    nodes: &HashMap<String, Vec<u8>>,
    scenes: &[String],
    read: &mut dyn FnMut(&str) -> Result<Vec<u8>, String>,
    output: &Path,
    written: &mut HashSet<PathBuf>,
) -> Result<Vec<Entry>, String> {
    let mut names: Vec<&String> = nodes
        .keys()
        .filter(|n| n.starts_with("sharedassets") && n.ends_with(".assets"))
        .collect();
    names.sort_by_key(|n| (scene_index(n).unwrap_or(usize::MAX), n.to_string()));
    let mut entries = Vec::new();
    for name in names {
        let scene = scene_index(name).and_then(|n| scenes.get(n));
        let group = scene.map(|s| group_of(s)).unwrap_or_default();
        for clip in video_clips(&nodes[name]).map_err(|e| format!("{name}: {e}"))? {
            entries.push(Entry {
                id: clip_id(&clip.name),
                name: clip.name.clone(),
                scene: scene.cloned(),
                group: group.clone(),
                clip,
            });
        }
    }
    unique_ids(&mut entries);
    entries.sort_by_key(|e| {
        (
            e.scene
                .as_ref()
                .and_then(|s| scenes.iter().position(|x| x == s))
                .unwrap_or(usize::MAX),
            e.name.clone(),
        )
    });
    let mut by_source: BTreeMap<&str, Vec<&Entry>> = BTreeMap::new();
    for entry in &entries {
        by_source.entry(&entry.clip.source).or_default().push(entry);
    }
    for (source, clips) in by_source {
        let resource = read(&format!("assets/bin/Data/{source}"))?;
        for entry in clips {
            files::write_and_keep(
                output.join(entry.file()),
                mp4(&resource, &entry.clip)?,
                written,
            )?;
        }
    }
    Ok(entries)
}

pub fn manifest(entries: &[Entry]) -> Result<Vec<u8>, String> {
    let clips: Vec<Value> = entries
        .iter()
        .map(|e| {
            json!({
                "id": e.id,
                "name": e.name,
                "group": e.group,
                "scene": e.scene,
                "file": e.file(),
                "size": e.clip.size,
                "width": e.clip.width,
                "height": e.clip.height,
                "fps": e.clip.fps,
                "frames": e.clip.frames,
                "duration": (e.clip.frames as f64 / e.clip.fps * 100.0).round() / 100.0,
                "audio": e.clip.audio,
            })
        })
        .collect();
    serde_json::to_vec(&json!({ "v": 1, "clips": clips })).map_err(|e| e.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn object(name: &str, audio: &[u16], source: &str, offset: u64, size: u64) -> Vec<u8> {
        fn s(out: &mut Vec<u8>, text: &str) {
            out.extend((text.len() as i32).to_le_bytes());
            out.extend(text.as_bytes());
            while !out.len().is_multiple_of(4) {
                out.push(0);
            }
        }
        let mut b = Vec::new();
        s(&mut b, name);
        s(&mut b, "");
        for v in [0u32, 0, 1920, 1080, 1, 1] {
            b.extend(v.to_le_bytes());
        }
        b.extend(30.0f64.to_le_bytes());
        b.extend(828u64.to_le_bytes());
        b.extend(1i32.to_le_bytes());
        b.extend((audio.len() as i32).to_le_bytes());
        for c in audio {
            b.extend(c.to_le_bytes());
        }
        while !b.len().is_multiple_of(4) {
            b.push(0);
        }
        b.extend((audio.len() as i32).to_le_bytes());
        for _ in audio {
            b.extend(44100u32.to_le_bytes());
        }
        b.extend(0i32.to_le_bytes());
        b.extend(0i32.to_le_bytes());
        s(&mut b, source);
        b.extend(offset.to_le_bytes());
        b.extend(size.to_le_bytes());
        b.extend([0u8, 0]);
        b
    }

    #[test]
    fn reads_a_video_clip_and_rejects_a_changed_layout() {
        let bytes = object(
            "Spas_Unlock",
            &[2],
            "archive:/CAB-1/sharedassets14.resource",
            1005344,
            18053964,
        );
        let clip = parse_clip(&bytes).unwrap();
        assert_eq!(clip.name, "Spas_Unlock");
        assert_eq!(
            (clip.width, clip.height, clip.fps, clip.frames),
            (1920, 1080, 30.0, 828)
        );
        assert!(clip.audio);
        assert_eq!(
            (clip.source.as_str(), clip.offset, clip.size),
            ("sharedassets14.resource", 1005344, 18053964)
        );
        assert!(
            !parse_clip(&object("Silent", &[], "x.resource", 0, 10))
                .unwrap()
                .audio
        );
        let mut longer = bytes.clone();
        longer.extend([0u8; 4]);
        assert!(parse_clip(&longer).is_err());
        assert!(parse_clip(&bytes[..bytes.len() - 4]).is_err());
    }

    #[test]
    fn slices_an_mp4_out_of_a_shared_resource_file() {
        let mut resource = vec![0u8; 8];
        resource.extend(b"\0\0\0 ftypisom");
        resource.extend([7u8; 20]);
        let clip = Clip {
            name: "a".into(),
            width: 1,
            height: 1,
            fps: 30.0,
            frames: 1,
            audio: true,
            source: "r".into(),
            offset: 8,
            size: 32,
        };
        assert_eq!(mp4(&resource, &clip).unwrap().len(), 32);
        assert!(mp4(
            &resource,
            &Clip {
                offset: 9,
                ..clip.clone()
            }
        )
        .is_err());
        assert!(mp4(
            &resource,
            &Clip {
                size: 10_000,
                ..clip
            }
        )
        .is_err());
    }

    #[test]
    fn names_files_after_their_clips() {
        for (name, id) in [
            ("Spas_Unlock", "spas-unlock"),
            ("ds unlockIntro_Sound", "ds-unlock-intro-sound"),
            ("321_Unlock_Final_Plus", "321-unlock-final-plus"),
            ("AfterDeductionVideo", "after-deduction-video"),
            ("DF_Touch2Start", "df-touch2-start"),
            ("Entrance to the Chaos_Intro", "entrance-to-the-chaos-intro"),
            ("QD", "qd"),
        ] {
            assert_eq!(clip_id(name), id, "{name}");
        }
        let entry = |id: &str| Entry {
            id: id.into(),
            name: id.into(),
            scene: None,
            group: String::new(),
            clip: Clip {
                name: id.into(),
                width: 1,
                height: 1,
                fps: 1.0,
                frames: 1,
                audio: false,
                source: String::new(),
                offset: 0,
                size: 1,
            },
        };
        let mut list = vec![entry("a"), entry("b"), entry("a"), entry("a")];
        unique_ids(&mut list);
        let ids: Vec<&str> = list.iter().map(|e| e.id.as_str()).collect();
        assert_eq!(ids, ["a", "b", "a-2", "a-3"]);
    }

    #[test]
    fn groups_clips_by_the_event_or_chapter_of_their_scene() {
        assert_eq!(
            group_of("Assets/TrackEventFile/SpasmodicEvent/FakeAboutUs.unity"),
            "SpasmodicEvent"
        );
        assert_eq!(
            group_of("Assets/TrackEventFile/321Event/321Unlock.unity"),
            "321Event"
        );
        assert_eq!(
            group_of("Assets/Phigros2/Chapter9/Deduction/AfterDeduction.unity"),
            "Chapter9"
        );
        assert_eq!(
            group_of("Assets/Phigros2/Chapter9/FirstEnterPhase2VideoPlayer.unity"),
            "Chapter9"
        );
        assert_eq!(
            group_of("Assets/Phigros2/SaturnOS/SaturnOS.unity"),
            "SaturnOS"
        );
        assert_eq!(scene_index("sharedassets22.assets"), Some(22));
        assert_eq!(scene_index("globalgamemanagers"), None);
    }

    // Real objects from the 4.0.1 APK (see /tmp/phi-fixtures, written by the fixture script)
    fn fixtures() -> Option<serde_json::Value> {
        let raw = std::fs::read("/tmp/phi-fixtures/meta.json").ok()?;
        serde_json::from_slice(&raw).ok()
    }

    #[test]
    fn parses_every_clip_and_the_scene_list_of_the_401_apk() {
        let Some(meta) = fixtures() else {
            return;
        };
        let clips = meta["clips"].as_array().unwrap();
        assert_eq!(clips.len(), 17);
        for want in clips {
            let hex = want["hex"].as_str().unwrap();
            let bytes: Vec<u8> = (0..hex.len())
                .step_by(2)
                .map(|i| u8::from_str_radix(&hex[i..i + 2], 16).unwrap())
                .collect();
            let clip = parse_clip(&bytes).unwrap();
            assert_eq!(clip.name, want["name"].as_str().unwrap());
            assert_eq!(clip.width as u64, want["width"].as_u64().unwrap());
            assert_eq!(clip.height as u64, want["height"].as_u64().unwrap());
            assert_eq!(clip.frames, want["frames"].as_u64().unwrap());
            assert_eq!(clip.offset, want["off"].as_u64().unwrap());
            assert_eq!(clip.size, want["size"].as_u64().unwrap());
            assert_eq!(clip.audio, !want["audio"].as_array().unwrap().is_empty());
        }
        let settings = std::fs::read("/tmp/phi-fixtures/globalgamemanagers").unwrap();
        let scenes = scenes_in(&HashMap::from([(
            "globalgamemanagers".to_string(),
            settings,
        )]))
        .unwrap();
        let want: Vec<String> = meta["scenes"]
            .as_array()
            .unwrap()
            .iter()
            .map(|s| s.as_str().unwrap().to_string())
            .collect();
        assert_eq!(scenes, want);
        assert_eq!(scenes[22], "Assets/Phigros2/SaturnOS/SaturnOS.unity");
    }
}
