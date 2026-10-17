use crate::collection::Item;
use crate::cursor::Cursor;
use crate::texture::{self, Texture};
use crate::unity::{self, Assets};
use crate::unityfs::Node;
use serde_json::{json, Value};
use std::collections::HashMap;

pub const DIR: &str = "collection-art";
pub const MANIFEST: &str = "collection-art/index.json";
pub const JPEG_QUALITY: u8 = 85;

/// `PersistentCall.m_Mode` for a call with a string argument
const STRING_MODE: i32 = 5;
const METHODS: [(&str, Layout); 3] = [
    ("OpenCollectionHorizontal", Layout::Text),
    ("OpenCollectionHorizontalWithImage", Layout::Banner),
    ("OpenCollectionVertical", Layout::Side),
];

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Layout {
    Side,
    Banner,
    Text,
}

impl Layout {
    pub fn as_str(self) -> &'static str {
        match self {
            Layout::Side => "side",
            Layout::Banner => "banner",
            Layout::Text => "text",
        }
    }
}

pub fn png_file(id: &str) -> String {
    format!("{DIR}/{id}.png")
}

pub fn jpg_file(id: &str) -> String {
    format!("{DIR}/{id}.jpg")
}

pub fn tile_file(id: &str) -> String {
    format!("{DIR}/{id}-tile.png")
}

/// The component of a map tile: the collection entry it opens, the sprite it shows once unlocked and its label
pub const TILE_SCRIPT: &str = "Chapter9CollectionUnlockSprite";

/// A map tile: the collection entry it opens and the label on it
#[derive(Clone, Debug, PartialEq)]
pub struct Tile {
    pub id: String,
    pub width: u32,
    pub height: u32,
    pub label: String,
}

/// `Chapter9CollectionUnlockSprite`'s fields
pub struct TileFields {
    pub key: String,
    pub sub: i32,
    pub unlocked: unity::PPtr,
    pub label: unity::PPtr,
}

impl TileFields {
    /// The collection id it opens: `collectionKey + collectionSubIndex`, the index counting from 0
    pub fn id(&self) -> String {
        format!("{}{}", self.key, self.sub)
    }
}

pub fn tile_fields(object: &[u8]) -> Result<TileFields, String> {
    let (_, _, mut c) = unity::behaviour_header(object)?;
    let links = c.i32()?;
    if !(0..=10_000).contains(&links) {
        return Err(format!("map link count {links}"));
    }
    c.bytes(links as usize * 12)?;
    let _scan_effect = unity::PPtr::read(&mut c)?;
    let _image = unity::PPtr::read(&mut c)?;
    let key = c.string()?;
    let sub = c.i32()?;
    c.u8()?;
    c.align(4);
    let unlocked = unity::PPtr::read(&mut c)?;
    let _locked = unity::PPtr::read(&mut c)?;
    let label = unity::PPtr::read(&mut c)?;
    let _icon = unity::PPtr::read(&mut c)?;
    let _cover = unity::PPtr::read(&mut c)?;
    c.u8()?;
    c.align(4);
    unity::end(&c, object, TILE_SCRIPT)?;
    if key.is_empty() || sub < 0 {
        return Err(format!("the tile opens {key:?} #{sub}"));
    }
    Ok(TileFields {
        key,
        sub,
        unlocked,
        label,
    })
}

/// A tile component's picture (its unlocked sprite's texture, alpha kept) and label, read from `assets`
pub fn read_tile_at(assets: &Assets, tile: unity::Object) -> Result<(Tile, Texture), String> {
    let fields = tile_fields(tile.bytes)?;
    let label = match assets.get(tile.file, fields.label, unity::MONO_BEHAVIOUR) {
        Ok(text) => unity::text(text.bytes)?.trim().to_string(),
        Err(_) => String::new(),
    };
    let sprite = assets.get(tile.file, fields.unlocked, unity::SPRITE)?;
    let tex = assets.get(
        sprite.file,
        unity::sprite_texture(sprite.bytes)?,
        unity::TEXTURE_2D,
    )?;
    let tex = texture::texture_object(tex.bytes, assets.nodes())?;
    Ok((
        Tile {
            id: fields.id(),
            width: tex.width,
            height: tex.height,
            label,
        },
        tex,
    ))
}

/// Tiles with their pictures, and what could not be read
pub type SceneTiles = (Vec<(Tile, Texture)>, Vec<String>);

/// Every map tile in `nodes` (a map scene with the files it references): one per collection id, in file order. A
/// tile that cannot be read is left out with a warning
pub fn scene_tiles(nodes: &[Node]) -> Result<SceneTiles, String> {
    let assets = Assets::new(nodes)?;
    let mut tiles: Vec<(Tile, Texture)> = Vec::new();
    let mut warnings = Vec::new();
    for object in assets.behaviours(TILE_SCRIPT) {
        match read_tile_at(&assets, object) {
            Ok((tile, tex)) if !tiles.iter().any(|(t, _)| t.id == tile.id) => {
                tiles.push((tile, tex))
            }
            Ok(_) => {}
            Err(err) => warnings.push(format!("map tile {}: {err}", object.path)),
        }
    }
    Ok((tiles, warnings))
}

/// A chapter's collection illustration, `Assets/Chapter<N>/Arts/<id>.jpg` (or `.png`): its collection id
pub fn addressable(key: &str) -> Option<&str> {
    let (chapter, file) = key.strip_prefix("Assets/Chapter")?.split_once("/Arts/")?;
    let id = file
        .strip_suffix(".jpg")
        .or_else(|| file.strip_suffix(".png"))?;
    let valid = !chapter.is_empty()
        && chapter.bytes().all(|b| b.is_ascii_digit())
        && !id.is_empty()
        && !id.contains('/');
    valid.then_some(id)
}

#[derive(Clone, Debug, PartialEq)]
pub struct Call {
    pub layout: Layout,
    pub id: String,
}

/// The persistent `OpenCollection*` calls in a scene file, in file order
pub fn scene_calls(scene: &[u8]) -> Vec<Call> {
    (4..scene.len())
        .filter(|&at| scene[at..].starts_with(b"OpenCollection"))
        .filter_map(|at| call_at(scene, at).ok())
        .collect()
}

/// A call whose `m_MethodName` text starts at `at`: the method, then `m_Mode` and the `ArgumentCache` (object
/// PPtr, object type name, int, float, string). Offsets align to 4 from the start of the file, as objects do
fn call_at(scene: &[u8], at: usize) -> Result<Call, String> {
    let mut c = Cursor::new(scene);
    c.seek(at - 4)?;
    let len = usize::try_from(c.i32()?).map_err(|_| "method name length")?;
    let name = c.bytes(len)?;
    let (_, layout) = METHODS
        .iter()
        .find(|(method, _)| method.as_bytes() == name)
        .ok_or("another method")?;
    c.align(4);
    if c.i32()? != STRING_MODE {
        return Err("not a string argument".into());
    }
    let _object = c.bytes(12)?;
    if c.string()?.len() > 512 {
        return Err("object type name".into());
    }
    let _int = c.i32()?;
    let _float = c.f32()?;
    let id = c.string()?;
    if id.is_empty() || id.len() > 128 || !id.bytes().all(|b| b.is_ascii_graphic()) {
        return Err("not a collection id".into());
    }
    Ok(Call {
        layout: *layout,
        id,
    })
}

/// Every collection id the entries build, `key + (sub - 1)` as `CollectionDatabase.BuildCollectionId` does (none
/// for `sub < 1`), with the entries that build it; more than one is a collision. Keys can end in digits (`heimu1#1`
/// is `heimu10`), so an id is never split back into key and sub
pub fn entry_ids(items: &[Item]) -> HashMap<String, Vec<(String, i32)>> {
    let mut ids: HashMap<String, Vec<(String, i32)>> = HashMap::new();
    for item in items.iter().filter(|i| i.sub >= 1) {
        let entries = ids
            .entry(format!("{}{}", item.key, item.sub - 1))
            .or_default();
        let entry = (item.key.clone(), item.sub);
        if !entries.contains(&entry) {
            entries.push(entry);
        }
    }
    ids
}

#[derive(Clone, Debug, PartialEq)]
pub struct Art {
    pub id: String,
    pub source: String,
    pub width: u32,
    pub height: u32,
}

#[derive(Clone, Debug, PartialEq)]
pub struct Placed {
    pub key: String,
    pub sub: i32,
    pub id: String,
    pub layout: Layout,
    pub scene: Option<String>,
    pub art: Option<Art>,
    pub tile: Option<Tile>,
}

/// Joins the map calls (with their scene, in build order) and the decoded art on the collection id, in the order of
/// the Collection list. Fails only when an id they use belongs to two entries
pub fn place(
    items: &[Item],
    calls: Vec<(String, Call)>,
    arts: Vec<Art>,
) -> Result<(Vec<Placed>, Vec<String>), String> {
    let ids = entry_ids(items);
    let entry = |id: &str| -> Result<Option<(String, i32)>, String> {
        match ids.get(id).map(Vec::as_slice) {
            None | Some([]) => Ok(None),
            Some([one]) => Ok(Some(one.clone())),
            Some(many) => Err(format!(
                "{id} is the collection id of {}",
                many.iter()
                    .map(|(key, sub)| format!("{key}#{sub}"))
                    .collect::<Vec<_>>()
                    .join(" and ")
            )),
        }
    };
    let mut warnings = Vec::new();
    let mut placed: Vec<Placed> = Vec::new();
    let mut at: HashMap<String, usize> = HashMap::new();
    for (scene, call) in calls {
        if let Some(&i) = at.get(&call.id) {
            if placed[i].layout != call.layout {
                warnings.push(format!(
                    "{} opens as {} and as {} in {scene}; keeping the first",
                    call.id,
                    placed[i].layout.as_str(),
                    call.layout.as_str()
                ));
            }
            continue;
        }
        let Some((key, sub)) = entry(&call.id)? else {
            warnings.push(format!(
                "a button in {scene} opens {}, which is no collection entry",
                call.id
            ));
            continue;
        };
        at.insert(call.id.clone(), placed.len());
        placed.push(Placed {
            key,
            sub,
            id: call.id,
            layout: call.layout,
            scene: Some(scene),
            art: None,
            tile: None,
        });
    }
    for art in arts {
        if let Some(&i) = at.get(&art.id) {
            if placed[i].layout == Layout::Text {
                warnings.push(format!("{} opens without its art", art.id));
            } else {
                placed[i].art = Some(art);
            }
            continue;
        }
        let Some((key, sub)) = entry(&art.id)? else {
            warnings.push(format!("{} is no collection entry's art", art.source));
            continue;
        };
        // No button opens it, so its shape decides: a 32:9 banner goes above the text, as the game's banners do
        let layout = if art.width >= 3 * art.height {
            Layout::Banner
        } else {
            Layout::Side
        };
        at.insert(art.id.clone(), placed.len());
        placed.push(Placed {
            key,
            sub,
            id: art.id.clone(),
            layout,
            scene: None,
            art: Some(art),
            tile: None,
        });
    }
    for entry in placed
        .iter()
        .filter(|p| p.layout != Layout::Text && p.art.is_none())
    {
        warnings.push(format!(
            "{} opens as {} but has no art",
            entry.id,
            entry.layout.as_str()
        ));
    }
    let mut order = HashMap::new();
    for (n, item) in items.iter().enumerate() {
        order.entry((item.key.as_str(), item.sub)).or_insert(n);
    }
    placed.sort_by_key(|p| {
        order
            .get(&(p.key.as_str(), p.sub))
            .copied()
            .unwrap_or(usize::MAX)
    });
    Ok((placed, warnings))
}

/// Puts each tile on the entry it opens. A tile for no placed entry, or a second one for an entry, is left out with
/// a warning
pub fn attach_tiles(placed: &mut [Placed], tiles: Vec<Tile>) -> Vec<String> {
    let mut warnings = Vec::new();
    for tile in tiles {
        match placed.iter_mut().find(|p| p.id == tile.id) {
            Some(entry) if entry.tile.is_none() => entry.tile = Some(tile),
            Some(_) => warnings.push(format!("{} has two map tiles; keeping the first", tile.id)),
            None => warnings.push(format!("a map tile opens {}, which is on no map", tile.id)),
        }
    }
    warnings
}

pub fn manifest(entries: &[Placed]) -> Result<Vec<u8>, String> {
    let entries: Vec<Value> = entries
        .iter()
        .map(|e| {
            let mut entry = json!({
                "id": e.id,
                "key": e.key,
                "sub": e.sub,
                "layout": e.layout.as_str(),
                "scene": e.scene,
                "art": e.art.as_ref().map(|a| json!({
                    "file": jpg_file(&a.id),
                    "original": png_file(&a.id),
                    "source": a.source,
                    "width": a.width,
                    "height": a.height,
                })),
            });
            if let Some(t) = &e.tile {
                entry["tile"] = json!({
                    "file": tile_file(&t.id),
                    "width": t.width,
                    "height": t.height,
                    "label": t.label,
                });
            }
            entry
        })
        .collect();
    serde_json::to_vec(&json!({ "v": 1, "entries": entries })).map_err(|e| e.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::texture;
    use crate::unityfs;
    use sha2::{Digest, Sha256};

    const P1: &str = "Assets/Phigros2/Chapter9/Chapter9SelectMusic.unity";
    const P2: &str = "Assets/Phigros2/Chapter9/Chapter9Phase2.unity";

    fn s(out: &mut Vec<u8>, text: &str) {
        out.extend((text.len() as i32).to_le_bytes());
        out.extend(text.as_bytes());
        while !out.len().is_multiple_of(4) {
            out.push(0);
        }
    }

    /// A persistent call as a scene stores it, from `m_MethodName` to `m_CallState`
    fn call(out: &mut Vec<u8>, method: &str, mode: i32, argument: &str) {
        s(out, method);
        out.extend(mode.to_le_bytes());
        out.extend(1i32.to_le_bytes());
        out.extend(0i64.to_le_bytes());
        s(out, "UnityEngine.Object, UnityEngine");
        out.extend(0i32.to_le_bytes());
        out.extend(0f32.to_le_bytes());
        s(out, argument);
        out.extend([0u8; 8]);
    }

    fn found(calls: Vec<Call>) -> Vec<(Layout, String)> {
        calls.into_iter().map(|c| (c.layout, c.id)).collect()
    }

    fn listed(calls: &[(Layout, &str)]) -> Vec<(Layout, String)> {
        calls.iter().map(|(l, id)| (*l, id.to_string())).collect()
    }

    #[test]
    fn reads_the_buttons_that_open_a_collection_entry() {
        let mut scene = vec![7u8; 12];
        call(
            &mut scene,
            "OpenCollectionHorizontalWithImage",
            5,
            "nizhidaoma0",
        );
        call(&mut scene, "OpenCollectionSEPlayer", 5, "nizhidaoma0");
        call(&mut scene, "OpenCollectionHorizontal", 5, "heimu10");
        call(&mut scene, "OpenCollectionHorizontal", 6, "heimu11");
        call(&mut scene, "OpenCollectionVertical", 5, "");
        call(&mut scene, "OpenCollectionVertical", 5, "two words");
        scene.extend(9i32.to_le_bytes());
        scene.extend(b"OpenCollectionVertical\0\0");
        call(&mut scene, "OpenInputPasswordSEPlayer", 1, "");
        call(&mut scene, "OpenInputPassword", 1, "");
        call(&mut scene, "OpenCollectionVertical", 5, "liangrenhuihe0");
        call(&mut scene, "OpenCollectionVertical", 5, "thelier0");
        scene.truncate(scene.len() - 14);
        assert_eq!(
            found(scene_calls(&scene)),
            listed(&[
                (Layout::Banner, "nizhidaoma0"),
                (Layout::Text, "heimu10"),
                (Layout::Side, "liangrenhuihe0"),
            ])
        );
        assert!(scene_calls(b"OpenCollectionVertical").is_empty());
    }

    #[test]
    fn reads_chapter_art_keys() {
        assert_eq!(
            addressable("Assets/Chapter9/Arts/liangrenhuihe0.jpg"),
            Some("liangrenhuihe0")
        );
        assert_eq!(addressable("Assets/Chapter12/Arts/x0.png"), Some("x0"));
        for key in [
            "Assets/Chapter9/Arts/",
            "Assets/Chapter9/Arts/.jpg",
            "Assets/Chapter9/Arts/a/b.jpg",
            "Assets/ChapterX/Arts/a.jpg",
            "Assets/Chapter/Arts/a.jpg",
            "Assets/Chapter9/Art/a.jpg",
            "Assets/Chapter9/Arts/a.webp",
            "c9s.test",
            "c9s.vK7mQp2H",
        ] {
            assert_eq!(addressable(key), None, "{key}");
        }
        assert_eq!(png_file("thewall0"), "collection-art/thewall0.png");
        assert_eq!(jpg_file("thewall0"), "collection-art/thewall0.jpg");
    }

    fn item(key: &str, sub: i32) -> Item {
        Item {
            song: 0,
            kind: 0,
            power: 0,
            info: Default::default(),
            key: key.into(),
            sub,
            title: Default::default(),
        }
    }

    fn entry(key: &str, sub: i32) -> Vec<(String, i32)> {
        vec![(key.to_string(), sub)]
    }

    #[test]
    fn builds_collection_ids_from_key_and_sub() {
        let ids = entry_ids(&[
            item("heimu1", 1),
            item("heimu1", 2),
            item("domeabout1", 1),
            item("zero", 0),
            item("a1", 1),
            item("a", 11),
            item("a1", 1),
        ]);
        assert_eq!(ids["heimu10"], entry("heimu1", 1));
        assert_eq!(ids["heimu11"], entry("heimu1", 2));
        assert_eq!(ids["domeabout10"], entry("domeabout1", 1));
        assert!(!ids.keys().any(|id| id.starts_with("zero")));
        assert_eq!(ids["a10"], [("a1".to_string(), 1), ("a".to_string(), 11)]);
    }

    fn art(id: &str, width: u32, height: u32) -> Art {
        Art {
            id: id.into(),
            source: format!("Assets/Chapter9/Arts/{id}.jpg"),
            width,
            height,
        }
    }

    fn on(scene: &str, layout: Layout, id: &str) -> (String, Call) {
        (
            scene.into(),
            Call {
                layout,
                id: id.into(),
            },
        )
    }

    #[test]
    fn places_every_entry_a_map_opens_or_that_has_art() {
        let items = [
            "first", "banner", "side", "text", "wide", "frame", "twice", "hidden",
        ]
        .map(|key| item(key, 1));
        let calls = vec![
            on(P1, Layout::Side, "side0"),
            on(P1, Layout::Banner, "banner0"),
            on(P1, Layout::Text, "text0"),
            on(P1, Layout::Banner, "nobody0"),
            on(P1, Layout::Text, "twice0"),
            on(P2, Layout::Side, "twice0"),
            on(P2, Layout::Text, "twice0"),
            on(P2, Layout::Text, "hidden0"),
        ];
        let arts = vec![
            art("side0", 2048, 1152),
            art("wide0", 2048, 576),
            art("frame0", 2048, 1152),
            art("stray0", 2048, 1152),
            art("hidden0", 2048, 1152),
        ];
        let (placed, warnings) = place(&items, calls, arts).unwrap();
        let rows: Vec<(&str, Layout, Option<&str>, Option<u32>)> = placed
            .iter()
            .map(|p| {
                (
                    p.id.as_str(),
                    p.layout,
                    p.scene.as_deref(),
                    p.art.as_ref().map(|a| a.height),
                )
            })
            .collect();
        assert_eq!(
            rows,
            [
                ("banner0", Layout::Banner, Some(P1), None),
                ("side0", Layout::Side, Some(P1), Some(1152)),
                ("text0", Layout::Text, Some(P1), None),
                ("wide0", Layout::Banner, None, Some(576)),
                ("frame0", Layout::Side, None, Some(1152)),
                ("twice0", Layout::Text, Some(P1), None),
                ("hidden0", Layout::Text, Some(P2), None),
            ]
        );
        assert_eq!((placed[0].key.as_str(), placed[0].sub), ("banner", 1));
        assert_eq!(
            warnings,
            [
                format!("a button in {P1} opens nobody0, which is no collection entry"),
                format!("twice0 opens as text and as side in {P2}; keeping the first"),
                "Assets/Chapter9/Arts/stray0.jpg is no collection entry's art".into(),
                "hidden0 opens without its art".into(),
                "banner0 opens as banner but has no art".into(),
            ]
        );
    }

    #[test]
    fn two_entries_with_one_id_fail_only_when_that_id_is_used() {
        let items = [item("a1", 1), item("a", 11), item("b", 1)];
        assert!(place(&items, vec![on(P1, Layout::Text, "b0")], vec![]).is_ok());
        assert_eq!(
            place(&items, vec![on(P1, Layout::Text, "a10")], vec![]).unwrap_err(),
            "a10 is the collection id of a1#1 and a#11"
        );
        assert!(place(&items, vec![], vec![art("a10", 2048, 1152)]).is_err());
    }

    #[test]
    fn writes_the_manifest_with_sorted_keys() {
        let placed = |sub: i32, layout, art| Placed {
            key: "nizhidaoma".into(),
            sub,
            id: format!("nizhidaoma{}", sub - 1),
            layout,
            scene: Some(P1.into()),
            art,
            tile: None,
        };
        let entries = [
            placed(1, Layout::Banner, Some(art("nizhidaoma0", 2048, 576))),
            Placed {
                key: "heimu1".into(),
                id: "heimu10".into(),
                tile: Some(Tile {
                    id: "heimu10".into(),
                    width: 395,
                    height: 256,
                    label: "".into(),
                }),
                ..placed(1, Layout::Text, None)
            },
        ];
        let body = manifest(&entries).unwrap();
        assert_eq!(body, manifest(&entries).unwrap());
        assert_eq!(
            String::from_utf8(body).unwrap(),
            concat!(
                r#"{"entries":["#,
                r#"{"art":{"file":"collection-art/nizhidaoma0.jpg","height":576,"original":"collection-art/nizhidaoma0.png","source":"Assets/Chapter9/Arts/nizhidaoma0.jpg","width":2048},"#,
                r#""id":"nizhidaoma0","key":"nizhidaoma","layout":"banner","scene":"Assets/Phigros2/Chapter9/Chapter9SelectMusic.unity","sub":1},"#,
                r#"{"art":null,"id":"heimu10","key":"heimu1","layout":"text","scene":"Assets/Phigros2/Chapter9/Chapter9SelectMusic.unity","sub":1,"#,
                r#""tile":{"file":"collection-art/heimu10-tile.png","height":256,"label":"","width":395}}"#,
                r#"],"v":1}"#
            )
        );
    }

    #[test]
    fn reads_a_tiles_entry_its_sprite_and_its_label() {
        use crate::unity::tests::{header, p, s};
        let mut b = Vec::new();
        header(&mut b, 1, (0, 9));
        b.extend(1i32.to_le_bytes());
        p(&mut b, 0, 4);
        p(&mut b, 0, 0);
        p(&mut b, 0, 7);
        s(&mut b, "thelier");
        b.extend(0i32.to_le_bytes());
        b.extend([0, 0, 0, 0]);
        for path in [5, 6, 8, 0, 0] {
            p(&mut b, 0, path);
        }
        b.extend([0, 0, 0, 0]);
        let fields = tile_fields(&b).unwrap();
        assert_eq!(fields.id(), "thelier0");
        assert_eq!((fields.unlocked.path, fields.label.path), (5, 8));
        assert!(tile_fields(&b[..b.len() - 4]).is_err());
    }

    #[test]
    fn puts_each_tile_on_the_entry_it_opens() {
        let tile = |id: &str| Tile {
            id: id.into(),
            width: 508,
            height: 456,
            label: "现实".into(),
        };
        let items = [item("thelier", 1), item("other", 1)];
        let calls = vec![
            on(P2, Layout::Side, "thelier0"),
            on(P2, Layout::Text, "other0"),
        ];
        let (mut placed, _) = place(&items, calls, vec![art("thelier0", 2048, 1152)]).unwrap();
        let warnings = attach_tiles(
            &mut placed,
            vec![tile("thelier0"), tile("nowhere0"), tile("thelier0")],
        );
        assert_eq!(placed[0].tile, Some(tile("thelier0")));
        assert_eq!(placed[1].tile, None);
        assert_eq!(
            warnings,
            [
                "a map tile opens nowhere0, which is on no map",
                "thelier0 has two map tiles; keeping the first"
            ]
        );
        assert_eq!(tile_file("thelier0"), "collection-art/thelier0-tile.png");
    }

    // The 4.0.1 APK's own data (see /tmp/phi-fixtures, written by the fixture script)
    fn fixture(name: &str) -> Option<Vec<u8>> {
        std::fs::read(format!("/tmp/phi-fixtures/{name}")).ok()
    }

    const LEVEL28: [(Layout, &str); 27] = [
        (Layout::Banner, "nizhidaoma0"),
        (Layout::Text, "guguthinking20"),
        (Layout::Text, "poyidomegino1"),
        (Layout::Text, "poyidomejiu1"),
        (Layout::Banner, "themirage0"),
        (Layout::Text, "domeabout60"),
        (Layout::Banner, "nizhidaoma1"),
        (Layout::Text, "guguthinking10"),
        (Layout::Text, "sundemimi2"),
        (Layout::Banner, "nizhidaoma2"),
        (Layout::Text, "domeabout20"),
        (Layout::Text, "sundemimi1"),
        (Layout::Text, "guguthinking30"),
        (Layout::Text, "domeabout30"),
        (Layout::Banner, "poyidomejiu0"),
        (Layout::Banner, "nizhidaoma3"),
        (Layout::Text, "domeabout50"),
        (Layout::Text, "sundemimi0"),
        (Layout::Side, "qiongdingguzhou0"),
        (Layout::Side, "m9beginning0"),
        (Layout::Text, "domeabout40"),
        (Layout::Text, "domeabout10"),
        (Layout::Banner, "poyidomegino0"),
        (Layout::Side, "liangrenhuihe0"),
        (Layout::Text, "sundemimi3"),
        (Layout::Text, "heimu10"),
        (Layout::Text, "heimu11"),
    ];

    const LEVEL30: [(Layout, &str); 6] = [
        (Layout::Side, "thechaos0"),
        (Layout::Banner, "thenoise0"),
        (Layout::Banner, "thewall0"),
        (Layout::Side, "thelier0"),
        (Layout::Text, "thewinner0"),
        (Layout::Side, "themessage0"),
    ];

    #[test]
    fn reads_the_os_maps_of_the_401_apk() {
        let (Some(level0), Some(level28), Some(level30), Some(meta)) = (
            fixture("level0"),
            fixture("level28"),
            fixture("level30"),
            fixture("meta.json"),
        ) else {
            return;
        };
        assert_eq!(found(scene_calls(&level28)), listed(&LEVEL28));
        assert_eq!(found(scene_calls(&level30)), listed(&LEVEL30));
        let meta: Value = serde_json::from_slice(&meta).unwrap();
        assert_eq!(
            (meta["scenes"][28].as_str(), meta["scenes"][30].as_str()),
            (Some(P1), Some(P2))
        );
        let calls = scene_calls(&level28)
            .into_iter()
            .map(|c| (P1.to_string(), c))
            .chain(
                scene_calls(&level30)
                    .into_iter()
                    .map(|c| (P2.to_string(), c)),
            )
            .collect();
        // Every image call's art at its real size; decoding it is decodes_chapter9_art's part
        let arts = LEVEL28
            .iter()
            .chain(&LEVEL30)
            .filter(|(layout, _)| *layout != Layout::Text)
            .map(|(layout, id)| art(id, 2048, if *layout == Layout::Banner { 576 } else { 1152 }))
            .collect();
        let items = crate::collection::parse_items(&level0).unwrap();
        let (placed, warnings) = place(&items, calls, arts).unwrap();
        assert!(warnings.is_empty(), "{warnings:?}");
        // blackhole0 opens from the passcode tile, which phi-specials adds
        assert_eq!(placed.len(), 33);
        assert_eq!(placed.iter().filter(|p| p.art.is_some()).count(), 15);
        let entry = |id: &str| {
            let p = placed.iter().find(|p| p.id == id).unwrap();
            (p.key.as_str(), p.sub, p.layout, p.scene.as_deref())
        };
        assert_eq!(entry("heimu10"), ("heimu1", 1, Layout::Text, Some(P1)));
        assert_eq!(entry("heimu11"), ("heimu1", 2, Layout::Text, Some(P1)));
        assert_eq!(
            entry("domeabout10"),
            ("domeabout1", 1, Layout::Text, Some(P1))
        );
        assert_eq!(
            entry("nizhidaoma1"),
            ("nizhidaoma", 2, Layout::Banner, Some(P1))
        );
        assert_eq!(
            entry("liangrenhuihe0"),
            ("liangrenhuihe", 1, Layout::Side, Some(P1))
        );
        assert!(!placed.iter().any(|p| p.id == "blackhole0"));
        assert_eq!(entry("thechaos0"), ("thechaos", 1, Layout::Side, Some(P2)));
        assert_eq!(
            entry("thewinner0"),
            ("thewinner", 1, Layout::Text, Some(P2))
        );
        let at: Vec<usize> = placed
            .iter()
            .map(|p| {
                items
                    .iter()
                    .position(|i| i.key == p.key && i.sub == p.sub)
                    .unwrap()
            })
            .collect();
        assert!(at.windows(2).all(|w| w[0] < w[1]));
        assert!(!placed.iter().any(|p| p.key.starts_with("unknownsignal")));
    }

    #[test]
    fn decodes_chapter9_art() {
        for (id, size, sha) in [
            (
                "thechaos0",
                (2048, 1152),
                "9806cca2e8a8ed8181885451e11bc956c36b12c8fa93a5e86469a1ad2114e617",
            ),
            (
                "nizhidaoma0",
                (2048, 576),
                "c4b73bb5d0404765dd3a45045f242724a18505942708e61fa9d1dc4ace47cb57",
            ),
        ] {
            let Some(bundle) = fixture(&format!("art/{id}.bundle")) else {
                continue;
            };
            let nodes = unityfs::decode(&bundle).unwrap();
            let rgb = texture::texture_from_bundle(&nodes)
                .unwrap()
                .into_rgb()
                .unwrap();
            assert_eq!(rgb.dimensions(), size, "{id}");
            assert_eq!(format!("{:x}", Sha256::digest(rgb.as_raw())), sha, "{id}");
        }
    }
}
