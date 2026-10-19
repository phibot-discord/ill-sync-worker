use crate::audio;
use crate::catalog::{self, Want};
use crate::collection;
use crate::collection_art;
use crate::files::{self, write_if_changed};
use crate::gameinfo::{self, VersionFile};
use crate::jackets;
use crate::layout::{self, song_id, LEVELS};
use crate::par;
use crate::pv;
use crate::serialized;
use crate::texture;
use crate::unity;
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

const DATA_DIR: &str = "assets/bin/Data/";
const PACKED_DATA: &str = "assets/bin/Data/data.unity3d";
/// The il2cpp string table: what the game's code says, byte for byte
pub const METADATA: &str = "assets/bin/Data/Managed/Metadata/global-metadata.dat";

/// Relative to the output root and the R2 bucket: `{ v: 1, songs: { <id>: [<level>, …] } }` for the songs
/// whose jackets exist per difficulty (`original_ill/{ill,illLow,illBlur}/<level>/<id>.png`)
pub const LEVELS_MANIFEST: &str = "original_ill/levels.json";

/// Directories extract owns. Files in them that this run did not produce are deleted afterwards
const OWNED_DIRS: [&str; 10] = [
    "chart",
    "music",
    "illustration",
    "illustrationLowRes",
    "illustrationBlur",
    "avatar",
    "original_ill",
    "pv",
    "collection",
    collection_art::DIR,
];

/// Note counts `[tap, drag, hold, flick]` and the last note end, in chart time units
pub type NoteCounts = ([u32; 4], f64);
/// NoteCounts per track folder and level
type ChartStats = BTreeMap<String, BTreeMap<String, NoteCounts>>;

pub struct Exported {
    pub jackets: usize,
    pub bundles: usize,
    pub clips: usize,
    pub collection: usize,
    pub arts: usize,
    pub warnings: Vec<String>,
}

/// The videos, the Collection and its art. A part that cannot be read is skipped, not a failed run: its old output
/// is kept and its directory is not pruned
#[derive(Default)]
struct Extras {
    written: HashSet<PathBuf>,
    keep: Vec<&'static str>,
    warnings: Vec<String>,
    clips: usize,
    collection: usize,
    arts: usize,
}

impl Extras {
    fn failed(&mut self, dir: &'static str, err: &str) {
        let warning = format!("{dir} skipped: {err}");
        eprintln!("{warning}");
        self.warnings.push(warning);
        self.keep.push(dir);
    }

    /// A part that was exported without something it could not read
    fn warn(&mut self, dir: &str, warning: &str) {
        let warning = format!("{dir}: {warning}");
        eprintln!("{warning}");
        self.warnings.push(warning);
    }
}

fn export_extras(
    zip: &mut ZipArchive<File>,
    names: &[String],
    packed: Option<&[u8]>,
    videos: &[catalog::BundleJob],
    arts: &[catalog::BundleJob],
    output: &Path,
) -> Extras {
    let mut out = Extras::default();
    let parsed = game_nodes(zip, names, packed)
        .and_then(|nodes| pv::scenes_in(&nodes).map(|scenes| (nodes, scenes)));
    match parsed {
        Ok((nodes, scenes)) => {
            match export_pv(zip, &nodes, &scenes, videos, output, &mut out.written) {
                Ok(clips) => out.clips = clips,
                Err(err) => out.failed("pv", &err),
            }
            match export_collection(&nodes, &scenes, output, &mut out.written) {
                Ok(entries) => out.collection = entries,
                Err(err) => out.failed("collection", &err),
            }
            match export_collection_art(
                zip,
                names,
                packed,
                &nodes,
                &scenes,
                arts,
                output,
                &mut out.written,
            ) {
                Ok(art) => {
                    out.arts = art.images;
                    for warning in &art.warnings {
                        out.warn(collection_art::DIR, warning);
                    }
                }
                Err(err) => out.failed(collection_art::DIR, &err),
            }
        }
        Err(err) => {
            out.failed("pv", &err);
            out.failed("collection", &err);
            out.failed(collection_art::DIR, &err);
        }
    }
    out
}

fn export_pv(
    zip: &mut ZipArchive<File>,
    nodes: &HashMap<String, Vec<u8>>,
    scenes: &[String],
    videos: &[catalog::BundleJob],
    output: &Path,
    written: &mut HashSet<PathBuf>,
) -> Result<usize, String> {
    let mut entries = pv::export_player_clips(
        nodes,
        scenes,
        &mut |path| read_zip(zip, path),
        output,
        written,
    )?;
    for job in videos {
        let Want::Video { group, name } = &job.want else {
            continue;
        };
        let bytes = read_zip(zip, &job.zip_path)?;
        let bundle = unityfs::decode(&bytes).map_err(|e| format!("{}: {e}", job.zip_path))?;
        drop(bytes);
        let (clip, mp4) = pv::clip_in_bundle(&bundle).map_err(|e| format!("{name}: {e}"))?;
        let entry = pv::Entry {
            id: pv::clip_id(name),
            name: name.clone(),
            scene: None,
            group: group.clone(),
            clip,
        };
        files::write_and_keep(output.join(entry.file()), mp4, written)?;
        entries.push(entry);
    }
    let mut ids = HashSet::new();
    if let Some(dup) = entries.iter().find(|e| !ids.insert(e.id.clone())) {
        return Err(format!("two clips are called {}", dup.id));
    }
    if entries.is_empty() {
        return Err("no video clips".into());
    }
    files::write_and_keep(output.join(pv::MANIFEST), &pv::manifest(&entries)?, written)?;
    Ok(entries.len())
}

fn export_collection(
    nodes: &HashMap<String, Vec<u8>>,
    scenes: &[String],
    output: &Path,
    written: &mut HashSet<PathBuf>,
) -> Result<usize, String> {
    let at = scenes
        .iter()
        .position(|s| s.ends_with("/SaturnOS.unity"))
        .ok_or("no SaturnOS scene")?;
    let scene = nodes
        .get(&format!("level{at}"))
        .ok_or("no SaturnOS scene data")?;
    let content = nodes
        .get(&format!("sharedassets{at}.assets"))
        .ok_or("no SaturnOS streamed data")?;
    let level0 = nodes.get("level0").ok_or("no level0")?;
    let collection = collection::parse(level0, scene, content)?;
    let mut outputs = vec![(collection::INDEX.to_string(), collection.index()?)];
    for (n, lang) in collection::LANGS.iter().enumerate() {
        outputs.push((collection::text_path(lang), collection.texts(n)?));
    }
    for (rel, body) in outputs {
        files::write_and_keep(output.join(rel), &body, written)?;
    }
    Ok(collection.entries())
}

struct ArtExport {
    images: usize,
    warnings: Vec<String>,
}

/// The Chapter 9 maps' illustrations and the panel each collection entry opens in. Every image is decoded and
/// encoded before anything is written, and the manifest goes last
fn export_collection_art(
    zip: &mut ZipArchive<File>,
    names: &[String],
    packed: Option<&[u8]>,
    nodes: &HashMap<String, Vec<u8>>,
    scenes: &[String],
    arts: &[catalog::BundleJob],
    output: &Path,
    written: &mut HashSet<PathBuf>,
) -> Result<ArtExport, String> {
    let items = collection::parse_items(nodes.get("level0").ok_or("no level0")?)?;
    let mut calls = Vec::new();
    let mut maps = Vec::new();
    for (n, scene) in scenes.iter().enumerate() {
        if let Some(level) = nodes.get(&format!("level{n}")) {
            let found = collection_art::scene_calls(level);
            if !found.is_empty() {
                maps.push(n);
            }
            calls.extend(found.into_iter().map(|call| (scene.clone(), call)));
        }
    }
    let (tiles, mut warnings) = map_tiles(zip, names, packed, nodes, &maps);
    let mut found = Vec::new();
    let mut images = HashMap::new();
    for job in arts {
        let Want::CollectionArt { id, source } = &job.want else {
            continue;
        };
        let ctx = |e: String| format!("{source}: {e}");
        let (rgb, _) = art_rgb(&read_zip(zip, &job.zip_path)?).map_err(ctx)?;
        let png = texture::encode_rgb_png(&rgb).map_err(ctx)?;
        let jpg = texture::encode_rgb_jpeg(&rgb, collection_art::JPEG_QUALITY).map_err(ctx)?;
        found.push(collection_art::Art {
            id: id.clone(),
            source: source.clone(),
            width: rgb.width(),
            height: rgb.height(),
        });
        images.insert(id.clone(), (png, jpg));
    }
    let (mut entries, more) = collection_art::place(&items, calls, found)?;
    warnings.extend(more);
    let mut tile_pngs = HashMap::new();
    let mut placed_tiles = Vec::new();
    for (tile, tex) in tiles {
        match texture::encode_png(&tex) {
            Ok(png) => {
                tile_pngs.insert(tile.id.clone(), png);
                placed_tiles.push(tile);
            }
            Err(err) => warnings.push(format!("map tile {}: {err}", tile.id)),
        }
    }
    warnings.extend(collection_art::attach_tiles(&mut entries, placed_tiles));
    let shown: Vec<&collection_art::Art> = entries.iter().filter_map(|e| e.art.as_ref()).collect();
    if shown.is_empty() {
        return Err("no collection art".into());
    }
    for art in &shown {
        let (png, jpg) = &images[&art.id];
        files::write_and_keep(output.join(collection_art::png_file(&art.id)), png, written)?;
        files::write_and_keep(output.join(collection_art::jpg_file(&art.id)), jpg, written)?;
    }
    for tile in entries.iter().filter_map(|e| e.tile.as_ref()) {
        if let Some(png) = tile_pngs.get(&tile.id) {
            files::write_and_keep(
                output.join(collection_art::tile_file(&tile.id)),
                png,
                written,
            )?;
        }
    }
    files::write_and_keep(
        output.join(collection_art::MANIFEST),
        &collection_art::manifest(&entries)?,
        written,
    )?;
    Ok(ArtExport {
        images: shown.len(),
        warnings,
    })
}

/// The tiles of the map scenes (`maps`, build indexes): each scene read with the shared files its objects reference
/// and the streamed texture data of its own shared assets
fn map_tiles(
    zip: &mut ZipArchive<File>,
    names: &[String],
    packed: Option<&[u8]>,
    nodes: &HashMap<String, Vec<u8>>,
    maps: &[usize],
) -> collection_art::SceneTiles {
    let mut tiles = Vec::new();
    let mut warnings = Vec::new();
    for &n in maps {
        let own = format!("sharedassets{n}.assets");
        let ours = |name: &str| name.starts_with(&own) && name.ends_with(".resS");
        let streamed = match packed {
            Some(packed) => {
                unityfs::decode_where(packed, |name| unityfs::is_resource(name) && ours(name))
            }
            None => loose_files(names, ours)
                .into_iter()
                .map(|(name, parts)| {
                    Ok(unityfs::Node {
                        name,
                        bytes: read_parts(zip, &parts)?,
                    })
                })
                .collect(),
        };
        let streamed = match streamed {
            Ok(found) => found,
            Err(err) => {
                warnings.push(format!("map tiles of scene {n}: {err}"));
                continue;
            }
        };
        let mut scene: Vec<unityfs::Node> = [
            format!("level{n}"),
            own.clone(),
            "globalgamemanagers.assets".into(),
        ]
        .into_iter()
        .filter_map(|name| {
            let bytes = nodes.get(&name)?.clone();
            Some(unityfs::Node { name, bytes })
        })
        .collect();
        scene.extend(streamed);
        match collection_art::scene_tiles(&scene) {
            Ok((found, more)) => {
                tiles.extend(found);
                warnings.extend(more);
            }
            Err(err) => warnings.push(format!("map tiles of scene {n}: {err}")),
        }
    }
    (tiles, warnings)
}

/// An art bundle's largest texture, and the sprites the bundle holds by serialized file and path ID
pub fn art_rgb(bundle: &[u8]) -> Result<(image::RgbImage, Vec<(String, i64)>), String> {
    let nodes = unityfs::decode(bundle)?;
    let rgb = texture::texture_from_bundle(&nodes)?
        .into_rgb()
        .ok_or("unexpected texture layout")?;
    let mut sprites = Vec::new();
    for node in nodes.iter().filter(|n| !unityfs::is_resource(&n.name)) {
        for object in serialized::objects(&node.bytes).unwrap_or_default() {
            if object.class_id == unity::SPRITE {
                sprites.push((node.name.clone(), object.path_id));
            }
        }
    }
    Ok((rgb, sprites))
}

/// Only the videos, the Collection and its art, into any directory: for trying a new APK
pub fn export_game_extras(apk: &Path, output: &Path) -> Result<(usize, usize, usize), String> {
    let (mut zip, names) = open_apk(apk)?;
    let packed = read_packed(&mut zip, &names)?;
    let jobs = bundle_jobs(&mut zip, &names)?;
    let extras = export_extras(
        &mut zip,
        &names,
        packed.as_deref(),
        &jobs.videos,
        &jobs.arts,
        output,
    );
    if let Some(dir) = extras.keep.first() {
        return Err(format!("{dir} was skipped"));
    }
    Ok((extras.clips, extras.collection, extras.arts))
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
    let (mut zip, names) = open_apk(apk)?;
    let packed = read_packed(&mut zip, &names)?;
    let level0 = read_level0(&mut zip, &names, packed.as_deref())?;
    let songs = gameinfo::parse_songs(&level0)?;
    let tips = gameinfo::parse_tips(&level0)?;
    let tip_lists = gameinfo::parse_tip_lists(&level0)?;
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
    let tips_manifest = output.join(gameinfo::TIPS_MANIFEST);
    if let Some(dir) = tips_manifest.parent() {
        fs::create_dir_all(dir).map_err(|e| e.to_string())?;
    }
    write_json(&tips_manifest, &gameinfo::tip_lists_json(&tip_lists))?;
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

    let Jobs {
        videos,
        arts,
        rest: jobs,
    } = bundle_jobs(&mut zip, &names)?;
    let extras = export_extras(&mut zip, &names, packed.as_deref(), &videos, &arts, output);
    drop(packed);
    eprintln!(
        "videos: {}, collection entries: {}, collection art: {}",
        extras.clips, extras.collection, extras.arts
    );
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
                    .strip_prefix(output.join("illustration"))
                    .ok()
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
            Want::Video { .. } => return Err(ctx("videos are exported separately".into())),
            Want::CollectionArt { .. } => {
                return Err(ctx("collection art is exported separately".into()))
            }
        }
        written.lock().map_err(|e| e.to_string())?.extend(paths);
        Ok(())
    })?;
    let mut written = written.into_inner().map_err(|e| e.to_string())?;
    written.extend(extras.written);
    // Which songs have a jacket per difficulty, for the web UI; under original_ill/, which is pruned
    files::write_and_keep(
        output.join(LEVELS_MANIFEST),
        jacket_levels_json(&jobs)?.as_bytes(),
        &mut written,
    )?;
    let mut stale = 0;
    for dir in OWNED_DIRS {
        if extras.keep.contains(&dir) {
            continue;
        }
        stale += files::prune(&output.join(dir), &written)?;
    }
    let stats = stats.into_inner().map_err(|e| e.to_string())?;
    write_if_changed(
        &info.join("notesInfo.json"),
        notes_info_json(&stats)?.as_bytes(),
    )?;
    let jackets = jobs
        .iter()
        .filter(|j| matches!(j.want, Want::Jacket { level: None, .. }))
        .count();
    eprintln!("extract done; removed {stale} stale files");
    Ok(Exported {
        jackets,
        bundles: jobs.len(),
        clips: extras.clips,
        collection: extras.collection,
        arts: extras.arts,
        warnings: extras.warnings,
    })
}

fn jacket_levels_json(jobs: &[catalog::BundleJob]) -> Result<String, String> {
    let mut songs: BTreeMap<String, Vec<&str>> = BTreeMap::new();
    for job in jobs {
        if let Want::Jacket {
            folder,
            level: Some(level),
        } = &job.want
        {
            songs
                .entry(song_id(folder).to_string())
                .or_default()
                .push(level.as_str());
        }
    }
    for levels in songs.values_mut() {
        levels.sort_by_key(|l| LEVELS.iter().position(|x| x == l));
        levels.dedup();
    }
    serde_json::to_string(&serde_json::json!({ "v": 1, "songs": songs })).map_err(|e| e.to_string())
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

pub fn open_apk(apk: &Path) -> Result<(ZipArchive<File>, Vec<String>), String> {
    let file = File::open(apk).map_err(|e| e.to_string())?;
    let zip = ZipArchive::new(file).map_err(|e| format!("apk is not a zip: {e}"))?;
    let names = zip.file_names().map(|s| s.to_string()).collect();
    Ok((zip, names))
}

/// The addressable bundles to export: the videos and the collection art go to `export_extras`, the rest to the
/// parallel extract
#[derive(Default)]
struct Jobs {
    videos: Vec<catalog::BundleJob>,
    arts: Vec<catalog::BundleJob>,
    rest: Vec<catalog::BundleJob>,
}

/// The APK's addressables catalog
pub fn read_catalog(zip: &mut ZipArchive<File>, names: &[String]) -> Result<Value, String> {
    let catalog_name = names
        .iter()
        .find(|n| n.ends_with("aa/catalog.json"))
        .ok_or("apk has no addressables catalog")?;
    serde_json::from_slice(&read_zip(zip, catalog_name)?).map_err(|e| e.to_string())
}

fn bundle_jobs(zip: &mut ZipArchive<File>, names: &[String]) -> Result<Jobs, String> {
    let catalog = read_catalog(zip, names)?;
    let mut jobs = Jobs::default();
    for job in unique_outputs(catalog::jobs(&catalog)?) {
        match job.want {
            Want::Video { .. } => jobs.videos.push(job),
            Want::CollectionArt { .. } => jobs.arts.push(job),
            _ => jobs.rest.push(job),
        }
    }
    Ok(jobs)
}

/// `data.unity3d`, which holds every scene and level0, when the APK packs them
pub fn read_packed(
    zip: &mut ZipArchive<File>,
    names: &[String],
) -> Result<Option<Vec<u8>>, String> {
    if names.iter().any(|n| n == PACKED_DATA) {
        read_zip(zip, PACKED_DATA).map(Some)
    } else {
        Ok(None)
    }
}

fn read_level0(
    zip: &mut ZipArchive<File>,
    names: &[String],
    packed: Option<&[u8]>,
) -> Result<Vec<u8>, String> {
    if let Some(packed) = packed {
        return unityfs::read_named(packed, "level0");
    }
    let parts = loose_files(names, |name| name == "level0")
        .remove("level0")
        .ok_or("apk has no level0 and no data.unity3d")?;
    read_parts(zip, &parts)
}

/// The scenes and the files they share, by name: unpacked from `data.unity3d`, or (4.0.1.1 on) the loose files
fn game_nodes(
    zip: &mut ZipArchive<File>,
    names: &[String],
    packed: Option<&[u8]>,
) -> Result<HashMap<String, Vec<u8>>, String> {
    if let Some(bytes) = packed {
        let nodes = unityfs::decode_where(bytes, |n| !unityfs::is_resource(n))?;
        return Ok(nodes.into_iter().map(|n| (n.name, n.bytes)).collect());
    }
    let files = loose_files(names, is_scene_file);
    if !files.contains_key("globalgamemanagers") {
        return Err("apk has no data.unity3d and no loose globalgamemanagers".into());
    }
    files
        .into_iter()
        .map(|(name, parts)| Ok((name, read_parts(zip, &parts)?)))
        .collect()
}

/// A serialized file a scene export reads: build settings, scenes and their shared assets
fn is_scene_file(name: &str) -> bool {
    let numbered = |prefix: &str, rest: &str| {
        name.strip_prefix(prefix)
            .and_then(|n| n.strip_suffix(rest))
            .is_some_and(|n| !n.is_empty() && n.bytes().all(|b| b.is_ascii_digit()))
    };
    matches!(
        name,
        "globalgamemanagers" | "globalgamemanagers.assets" | "resources.assets"
    ) || numbered("level", "")
        || numbered("sharedassets", ".assets")
}

/// Files directly in assets/bin/Data whose name passes `keep`, each with its zip entries in order: one, or the
/// `.splitN` parts a large file is cut into
fn loose_files(names: &[String], keep: impl Fn(&str) -> bool) -> BTreeMap<String, Vec<&String>> {
    let mut files: BTreeMap<String, Vec<&String>> = BTreeMap::new();
    for name in names {
        let Some(file) = name.strip_prefix(DATA_DIR).filter(|f| !f.contains('/')) else {
            continue;
        };
        let base = file
            .rsplit_once(".split")
            .filter(|(_, n)| n.parse::<u32>().is_ok())
            .map_or(file, |(base, _)| base);
        if keep(base) {
            files.entry(base.to_string()).or_default().push(name);
        }
    }
    for parts in files.values_mut() {
        parts.sort_by_key(|n| split_ord(n));
    }
    files
}

fn read_parts(zip: &mut ZipArchive<File>, parts: &[&String]) -> Result<Vec<u8>, String> {
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

pub fn read_zip(zip: &mut ZipArchive<File>, name: &str) -> Result<Vec<u8>, String> {
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

/// The chart a TextAsset holds: the object that opens with `formatVersion`, compact or (4.0.1.1 on) indented
pub fn carve_chart(nodes: &[unityfs::Node]) -> Result<(Vec<u8>, NoteCounts), String> {
    let needle = b"\"formatVersion\"";
    for node in nodes {
        let bytes = &node.bytes;
        let mut from = 0;
        while let Some(rel) = find(&bytes[from..], needle) {
            let key = from + rel;
            from = key + needle.len();
            let Some(rel) = bytes[..key]
                .iter()
                .rposition(|b| !b.is_ascii_whitespace())
                .filter(|&open| bytes[open] == b'{')
            else {
                continue;
            };
            let slice = &bytes[rel..];
            let mut stream = serde_json::Deserializer::from_slice(slice).into_iter::<ChartNotes>();
            if let Some(Ok(ChartNotes { lines: Some(lines) })) = stream.next() {
                let end = stream.byte_offset();
                return Ok((slice[..end].to_vec(), line_counts(&lines)));
            }
            let end = json_end(slice)?;
            let value: Value = serde_json::from_slice(&slice[..end]).map_err(|e| e.to_string())?;
            if value.get("judgeLineList").is_none() {
                break;
            }
            return Ok((slice[..end].to_vec(), count_notes(&value)));
        }
    }
    Err("bundle has no chart json".into())
}

pub fn find(haystack: &[u8], needle: &[u8]) -> Option<usize> {
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
    fn a_skipped_part_keeps_its_old_output_and_is_reported() {
        let mut extras = Extras::default();
        extras.failed("pv", "no video clips");
        extras.failed("collection", "no chapters in the SaturnOS scene");
        extras.failed(collection_art::DIR, "no collection art");
        assert_eq!(extras.keep, ["pv", "collection", "collection-art"]);
        assert_eq!(
            extras.warnings,
            [
                "pv skipped: no video clips",
                "collection skipped: no chapters in the SaturnOS scene",
                "collection-art skipped: no collection art"
            ]
        );
        assert!(OWNED_DIRS.contains(&"collection-art"));
    }

    #[test]
    fn lists_the_songs_with_a_jacket_per_difficulty() {
        let job = |folder: &str, level: Option<&str>| catalog::BundleJob {
            zip_path: String::new(),
            want: Want::Jacket {
                folder: folder.into(),
                level: level.map(String::from),
            },
        };
        let jobs = vec![
            job("Song.A.0", Some("IN")),
            job("Song.A.0", Some("EZ")),
            job("Song.A.0", None),
            job("Song.A.0", Some("AT")),
            job("Plain.B.0", None),
        ];
        assert_eq!(
            jacket_levels_json(&jobs).unwrap(),
            r#"{"songs":{"Song.A":["EZ","IN","AT"]},"v":1}"#
        );
        assert_eq!(jacket_levels_json(&[]).unwrap(), r#"{"songs":{},"v":1}"#);
    }

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

    #[test]
    fn finds_indented_charts_like_the_new_sp_ones() {
        let node = unityfs::Node {
            name: "CAB".into(),
            bytes: b"\x08\0\0\0Chart_SP\xff\xff\x17\0{\n    \"formatVersion\": 2,\n    \"judgeLineList\": [{\"notesAbove\": [{\"type\": 1, \"time\": 4}], \"notesBelow\": []}]\n}tail"
                .to_vec(),
        };
        let (body, (counts, _)) = carve_chart(&[node]).unwrap();
        assert!(body.starts_with(b"{\n    \"formatVersion\": 2"));
        assert!(body.ends_with(b"]\n}"));
        assert_eq!(counts, [1, 0, 0, 0]);
    }
}
