//! A bundle's objects and the references between them. A PPtr names an object by file (0 is the file it is read
//! from, `n` that file's `n`th external) and path ID. The readers take an object's bytes in the layout Unity 2022.3
//! writes, which the game ships no type tree for here, and fail on anything they do not consume exactly.

use crate::cursor::Cursor;
use crate::serialized::{self, AssetObject};
use crate::unityfs::{self, Node};
use std::collections::HashMap;

pub const GAME_OBJECT: i32 = 1;
pub const TEXTURE_2D: i32 = 28;
pub const AUDIO_SOURCE: i32 = 82;
pub const AUDIO_CLIP: i32 = 83;
pub const MONO_BEHAVIOUR: i32 = 114;
pub const MONO_SCRIPT: i32 = 115;
pub const ASSET_BUNDLE: i32 = 142;
pub const SPRITE: i32 = 213;
pub const VIDEO_PLAYER: i32 = 328;
pub const VIDEO_CLIP: i32 = 329;

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct PPtr {
    pub file: i32,
    pub path: i64,
}

impl PPtr {
    pub fn read(c: &mut Cursor) -> Result<Self, String> {
        Ok(Self {
            file: c.i32()?,
            path: c.i64()?,
        })
    }

    pub fn is_null(self) -> bool {
        self.path == 0
    }
}

struct File<'a> {
    name: &'a str,
    bytes: &'a [u8],
    objects: HashMap<i64, AssetObject>,
    externals: Vec<String>,
}

/// The serialized files of one bundle and its streamed data
pub struct Assets<'a> {
    files: Vec<File<'a>>,
    nodes: &'a [Node],
}

#[derive(Clone, Copy, Debug)]
pub struct Object<'a> {
    pub file: usize,
    pub path: i64,
    pub class_id: i32,
    pub bytes: &'a [u8],
}

impl<'a> Assets<'a> {
    pub fn new(nodes: &'a [Node]) -> Result<Self, String> {
        let mut files = Vec::new();
        for node in nodes.iter().filter(|n| !unityfs::is_resource(&n.name)) {
            let parsed =
                serialized::parse(&node.bytes).map_err(|e| format!("{}: {e}", node.name))?;
            files.push(File {
                name: &node.name,
                bytes: &node.bytes,
                objects: parsed.objects.into_iter().map(|o| (o.path_id, o)).collect(),
                externals: parsed.externals,
            });
        }
        if files.is_empty() {
            return Err("bundle has no serialized file".into());
        }
        Ok(Self { files, nodes })
    }

    pub fn nodes(&self) -> &'a [Node] {
        self.nodes
    }

    /// Every object, file by file in path ID order
    pub fn objects(&self) -> Vec<Object<'a>> {
        let mut out = Vec::new();
        for (n, file) in self.files.iter().enumerate() {
            let mut paths: Vec<&i64> = file.objects.keys().collect();
            paths.sort();
            out.extend(paths.into_iter().filter_map(|path| self.object(n, *path)));
        }
        out
    }

    fn object(&self, file: usize, path: i64) -> Option<Object<'a>> {
        let f = self.files.get(file)?;
        let object = f.objects.get(&path)?;
        Some(Object {
            file,
            path,
            class_id: object.class_id,
            bytes: object.bytes(f.bytes)?,
        })
    }

    /// The name of the file `ptr` points into, read from `from`: that file's own, or the last part of the external's
    /// path, which is a serialized file's name when it is in some bundle
    pub fn file_name(&self, from: usize, ptr: PPtr) -> Result<&str, String> {
        let file = self.files.get(from).ok_or("no such file")?;
        if ptr.file == 0 {
            return Ok(file.name);
        }
        let external = usize::try_from(ptr.file - 1)
            .ok()
            .and_then(|n| file.externals.get(n))
            .ok_or_else(|| format!("{} has no external file {}", file.name, ptr.file))?;
        Ok(external.rsplit('/').next().unwrap_or(external))
    }

    /// The object `ptr` names, read from the file `from`; it must be in this bundle and of class `class_id`
    pub fn get(&self, from: usize, ptr: PPtr, class_id: i32) -> Result<Object<'a>, String> {
        if ptr.is_null() {
            return Err("null reference".into());
        }
        let name = self.file_name(from, ptr)?;
        let file = self
            .files
            .iter()
            .position(|f| f.name == name)
            .ok_or_else(|| format!("{name} is not in this bundle"))?;
        let object = self
            .object(file, ptr.path)
            .ok_or_else(|| format!("{name} has no object {}", ptr.path))?;
        if object.class_id != class_id {
            return Err(format!(
                "object {} of {name} is class {}, not {class_id}",
                ptr.path, object.class_id
            ));
        }
        Ok(object)
    }

    /// Streamed data by the path an object gives for it (`archive:/CAB-<hash>/CAB-<hash>.resource`)
    pub fn resource(&self, source: &str) -> Result<&'a [u8], String> {
        let name = source.rsplit('/').next().unwrap_or(source);
        self.nodes
            .iter()
            .find(|n| n.name == name)
            .map(|n| n.bytes.as_slice())
            .ok_or_else(|| format!("bundle has no {name}"))
    }

    /// The class name of a MonoBehaviour's script
    pub fn script_class(&self, behaviour: Object) -> Result<String, String> {
        let (_, script, _) = behaviour_header(behaviour.bytes)?;
        let script = self.get(behaviour.file, script, MONO_SCRIPT)?;
        script_class_name(script.bytes)
    }

    /// The MonoBehaviours whose script is the class `name`
    pub fn behaviours(&self, name: &str) -> Vec<Object<'a>> {
        self.objects()
            .into_iter()
            .filter(|o| o.class_id == MONO_BEHAVIOUR)
            .filter(|o| self.script_class(*o).is_ok_and(|class| class == name))
            .collect()
    }
}

/// A MonoBehaviour's own fields: its GameObject and script, and a cursor at the first field of the script
pub fn behaviour_header(object: &[u8]) -> Result<(PPtr, PPtr, Cursor<'_>), String> {
    let mut c = Cursor::new(object);
    let game_object = PPtr::read(&mut c)?;
    c.u8()?;
    c.align(4);
    let script = PPtr::read(&mut c)?;
    let _name = c.string()?;
    Ok((game_object, script, c))
}

pub fn end(c: &Cursor, object: &[u8], what: &str) -> Result<(), String> {
    if c.pos() == object.len() {
        Ok(())
    } else {
        Err(format!(
            "{what} object has {} bytes, parsed {}",
            object.len(),
            c.pos()
        ))
    }
}

fn script_class_name(object: &[u8]) -> Result<String, String> {
    let mut c = Cursor::new(object);
    let _name = c.string()?;
    let _order = c.i32()?;
    let _hash = c.bytes(16)?;
    c.string()
}

fn pptrs(c: &mut Cursor) -> Result<Vec<PPtr>, String> {
    let count = c.i32()?;
    if !(0..=100_000).contains(&count) {
        return Err(format!("reference count {count}"));
    }
    (0..count).map(|_| PPtr::read(c)).collect()
}

/// A GameObject's components
pub fn components(object: &[u8]) -> Result<Vec<PPtr>, String> {
    let mut c = Cursor::new(object);
    let components = pptrs(&mut c)?;
    let _layer = c.u32()?;
    let _name = c.string()?;
    let _tag_and_active = c.bytes(3)?;
    end(&c, object, "game object")?;
    Ok(components)
}

/// The assets an AssetBundle lists by name (its container), in its order
pub fn bundle_assets(object: &[u8]) -> Result<Vec<PPtr>, String> {
    let mut c = Cursor::new(object);
    let _name = c.string()?;
    let _preload = pptrs(&mut c)?;
    let count = c.i32()?;
    if !(0..=100_000).contains(&count) {
        return Err(format!("asset bundle container count {count}"));
    }
    let mut assets = Vec::with_capacity(count as usize);
    for _ in 0..count {
        let _key = c.string()?;
        let _preload_index = c.i32()?;
        let _preload_size = c.i32()?;
        assets.push(PPtr::read(&mut c)?);
    }
    Ok(assets)
}

/// The clip an AudioSource plays
pub fn audio_source_clip(object: &[u8]) -> Result<PPtr, String> {
    let mut c = Cursor::new(object);
    let _game_object = PPtr::read(&mut c)?;
    c.u8()?;
    c.align(4);
    let _mixer = PPtr::read(&mut c)?;
    PPtr::read(&mut c)
}

/// The clip a VideoPlayer plays
pub fn video_player_clip(object: &[u8]) -> Result<PPtr, String> {
    let mut c = Cursor::new(object);
    let _game_object = PPtr::read(&mut c)?;
    c.u8()?;
    c.align(4);
    PPtr::read(&mut c)
}

#[derive(Clone, Debug, PartialEq)]
pub struct AudioClip {
    pub name: String,
    pub source: String,
    pub offset: u64,
    pub size: u64,
}

pub fn audio_clip(object: &[u8]) -> Result<AudioClip, String> {
    let mut c = Cursor::new(object);
    let name = c.string()?;
    let _load_type = c.i32()?;
    let _channels = c.i32()?;
    let _frequency = c.i32()?;
    let _bits = c.i32()?;
    let _length = c.f32()?;
    c.bytes(2)?;
    c.align(4);
    let _subsound = c.i32()?;
    c.bytes(3)?;
    c.align(4);
    let source = c.string()?;
    let offset = c.u64()?;
    let size = c.u64()?;
    let _compression = c.i32()?;
    end(&c, object, "audio clip")?;
    Ok(AudioClip {
        name,
        source,
        offset,
        size,
    })
}

impl AudioClip {
    /// The clip's bytes in its streamed data (an FSB5 file)
    pub fn data<'a>(&self, resource: &'a [u8]) -> Result<&'a [u8], String> {
        let start = usize::try_from(self.offset).map_err(|_| "clip offset")?;
        let end = usize::try_from(self.size)
            .ok()
            .and_then(|size| start.checked_add(size))
            .ok_or("clip size")?;
        resource
            .get(start..end)
            .ok_or_else(|| format!("{} is past the end of {}", self.name, self.source))
    }
}

/// `UnityEvent` persistent calls, as `Graphic.m_OnCullStateChanged` and buttons store them
fn skip_calls(c: &mut Cursor) -> Result<(), String> {
    let count = c.i32()?;
    if !(0..=10_000).contains(&count) {
        return Err(format!("persistent call count {count}"));
    }
    for _ in 0..count {
        let _target = PPtr::read(c)?;
        let _target_type = c.string()?;
        let _method = c.string()?;
        let _mode = c.i32()?;
        let _object = PPtr::read(c)?;
        let _object_type = c.string()?;
        let _int = c.i32()?;
        let _float = c.f32()?;
        let _string = c.string()?;
        c.u8()?;
        c.align(4);
        let _state = c.i32()?;
    }
    Ok(())
}

/// A UI `Graphic`'s fields, which `Image` and `Text` start with after the MonoBehaviour's own
fn skip_graphic(c: &mut Cursor) -> Result<(), String> {
    let _material = PPtr::read(c)?;
    let _color = c.bytes(16)?;
    c.u8()?;
    c.align(4);
    let _padding = c.bytes(16)?;
    c.u8()?;
    c.align(4);
    skip_calls(c)
}

/// The sprite a UI `Image` shows
pub fn image_sprite(object: &[u8]) -> Result<PPtr, String> {
    let (_, _, mut c) = behaviour_header(object)?;
    skip_graphic(&mut c)?;
    let sprite = PPtr::read(&mut c)?;
    let _type = c.i32()?;
    for _preserve_aspect_and_fill_center in 0..2 {
        c.u8()?;
        c.align(4);
    }
    let _fill_method = c.i32()?;
    let _fill_amount = c.f32()?;
    c.u8()?;
    c.align(4);
    let _fill_origin = c.i32()?;
    c.u8()?;
    c.align(4);
    let _pixels_per_unit = c.f32()?;
    end(&c, object, "image")?;
    Ok(sprite)
}

/// The text a UI `Text` shows
pub fn text(object: &[u8]) -> Result<String, String> {
    let (_, _, mut c) = behaviour_header(object)?;
    skip_graphic(&mut c)?;
    let _font = PPtr::read(&mut c)?;
    let _size = c.i32()?;
    let _style = c.i32()?;
    c.u8()?;
    c.align(4);
    let _min = c.i32()?;
    let _max = c.i32()?;
    let _alignment = c.i32()?;
    for _align_by_geometry_and_rich_text in 0..2 {
        c.u8()?;
        c.align(4);
    }
    let _horizontal = c.i32()?;
    let _vertical = c.i32()?;
    let _spacing = c.f32()?;
    let text = c.string()?;
    end(&c, object, "text")?;
    Ok(text)
}

/// The texture a Sprite draws from (`m_RD.texture`)
pub fn sprite_texture(object: &[u8]) -> Result<PPtr, String> {
    let mut c = Cursor::new(object);
    let _name = c.string()?;
    let _rect = c.bytes(16)?;
    let _offset = c.bytes(8)?;
    let _border = c.bytes(16)?;
    let _pixels_to_units = c.f32()?;
    let _pivot = c.bytes(8)?;
    let _extrude = c.u32()?;
    c.u8()?;
    c.align(4);
    let _render_key = c.bytes(24)?;
    let tags = c.i32()?;
    if !(0..=10_000).contains(&tags) {
        return Err(format!("sprite atlas tag count {tags}"));
    }
    for _ in 0..tags {
        c.string()?;
    }
    let _atlas = PPtr::read(&mut c)?;
    PPtr::read(&mut c)
}

/// An `AnimationCurve`, skipped
pub fn skip_curve(c: &mut Cursor) -> Result<(), String> {
    let keys = c.i32()?;
    if !(0..=100_000).contains(&keys) {
        return Err(format!("curve key count {keys}"));
    }
    c.bytes(keys as usize * 28)?;
    let _pre = c.i32()?;
    let _post = c.i32()?;
    let _rotation = c.i32()?;
    Ok(())
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;

    pub fn s(out: &mut Vec<u8>, text: &str) {
        out.extend((text.len() as i32).to_le_bytes());
        out.extend(text.as_bytes());
        while !out.len().is_multiple_of(4) {
            out.push(0);
        }
    }

    pub fn p(out: &mut Vec<u8>, file: i32, path: i64) {
        out.extend(file.to_le_bytes());
        out.extend(path.to_le_bytes());
    }

    pub fn header(out: &mut Vec<u8>, game_object: i64, script: (i32, i64)) {
        p(out, 0, game_object);
        out.extend([1, 0, 0, 0]);
        p(out, script.0, script.1);
        s(out, "");
    }

    /// `Graphic`'s fields with one persistent call
    fn graphic(out: &mut Vec<u8>) {
        p(out, 1, 18);
        out.extend([0u8; 16]);
        out.extend([1, 0, 0, 0]);
        out.extend([0u8; 16]);
        out.extend([1, 0, 0, 0]);
        out.extend(1i32.to_le_bytes());
        p(out, 0, 7);
        s(out, "UnityEngine.Object, UnityEngine");
        s(out, "SetActive");
        out.extend(6i32.to_le_bytes());
        p(out, 0, 0);
        s(out, "");
        out.extend(0i32.to_le_bytes());
        out.extend(0f32.to_le_bytes());
        s(out, "x");
        out.extend([1, 0, 0, 0]);
        out.extend(2i32.to_le_bytes());
    }

    #[test]
    fn reads_an_image_and_a_text_after_their_graphic() {
        let mut image = Vec::new();
        header(&mut image, 1, (1, 5));
        graphic(&mut image);
        p(&mut image, 3, -3217550930489318217);
        image.extend(0i32.to_le_bytes());
        image.extend([0, 0, 0, 0, 1, 0, 0, 0]);
        image.extend(4i32.to_le_bytes());
        image.extend(1f32.to_le_bytes());
        image.extend([1, 0, 0, 0]);
        image.extend(0i32.to_le_bytes());
        image.extend([0, 0, 0, 0]);
        image.extend(1f32.to_le_bytes());
        assert_eq!(
            image_sprite(&image).unwrap(),
            PPtr {
                file: 3,
                path: -3217550930489318217
            }
        );
        let mut longer = image.clone();
        longer.extend([0u8; 4]);
        assert!(image_sprite(&longer).is_err());

        let mut text_object = Vec::new();
        header(&mut text_object, 2, (0, 9));
        graphic(&mut text_object);
        p(&mut text_object, 0, 4);
        for v in [35i32, 0, 1, 3, 35, 4, 0, 1, 0, 0] {
            text_object.extend(v.to_le_bytes());
        }
        text_object.extend(1f32.to_le_bytes());
        s(&mut text_object, "答案");
        assert_eq!(text(&text_object).unwrap(), "答案");
        assert!(text(&text_object[..text_object.len() - 4]).is_err());
    }

    #[test]
    fn reads_what_sources_play_and_where_a_clip_streams_from() {
        let mut source = Vec::new();
        p(&mut source, 0, 11);
        source.extend([1, 0, 0, 0]);
        p(&mut source, 0, 0);
        p(&mut source, 1, 33);
        source.extend([0u8; 40]);
        assert_eq!(
            audio_source_clip(&source).unwrap(),
            PPtr { file: 1, path: 33 }
        );
        let mut player = Vec::new();
        p(&mut player, 0, 5);
        player.extend([1, 0, 0, 0]);
        p(&mut player, 1, 22);
        assert_eq!(
            video_player_clip(&player).unwrap(),
            PPtr { file: 1, path: 22 }
        );

        let mut clip = Vec::new();
        s(&mut clip, "文字浮现音效");
        for v in [0i32, 2, 44100, 16] {
            clip.extend(v.to_le_bytes());
        }
        clip.extend(0.14f32.to_le_bytes());
        clip.extend([0, 0, 0, 0]);
        clip.extend(0i32.to_le_bytes());
        clip.extend([0, 0, 1, 0]);
        s(&mut clip, "archive:/CAB-1/CAB-1.resource");
        clip.extend(4u64.to_le_bytes());
        clip.extend(3u64.to_le_bytes());
        clip.extend(1i32.to_le_bytes());
        let parsed = audio_clip(&clip).unwrap();
        assert_eq!(
            (parsed.name.as_str(), parsed.source.as_str()),
            ("文字浮现音效", "archive:/CAB-1/CAB-1.resource")
        );
        assert_eq!(parsed.data(b"0123FSB5").unwrap(), b"FSB");
        assert!(AudioClip { size: 5, ..parsed }.data(b"0123FSB5").is_err());
        assert!(audio_clip(&clip[..clip.len() - 4]).is_err());
    }

    #[test]
    fn reads_components_bundle_assets_and_sprite_textures() {
        let mut game_object = Vec::new();
        game_object.extend(2i32.to_le_bytes());
        p(&mut game_object, 0, 6168861130642117000);
        p(&mut game_object, 0, -4047794379647554281);
        game_object.extend(5u32.to_le_bytes());
        s(&mut game_object, "AnswerCollectionItem");
        game_object.extend([0, 0, 1]);
        assert_eq!(
            components(&game_object).unwrap(),
            [
                PPtr {
                    file: 0,
                    path: 6168861130642117000
                },
                PPtr {
                    file: 0,
                    path: -4047794379647554281
                }
            ]
        );

        let mut bundle = Vec::new();
        s(&mut bundle, "x.bundle");
        bundle.extend(1i32.to_le_bytes());
        p(&mut bundle, 0, 9);
        bundle.extend(1i32.to_le_bytes());
        s(&mut bundle, "b7645380c9e921944863e90d4e0028ab");
        bundle.extend(0i32.to_le_bytes());
        bundle.extend(22i32.to_le_bytes());
        p(&mut bundle, 0, -8255244746183859794);
        bundle.extend([0u8; 20]);
        assert_eq!(
            bundle_assets(&bundle).unwrap(),
            [PPtr {
                file: 0,
                path: -8255244746183859794
            }]
        );

        let mut sprite = Vec::new();
        s(&mut sprite, "CollectionHeader");
        sprite.extend([0u8; 16 + 8 + 16]);
        sprite.extend(100f32.to_le_bytes());
        sprite.extend([0u8; 8]);
        sprite.extend(1u32.to_le_bytes());
        sprite.extend([0, 0, 0, 0]);
        sprite.extend([7u8; 24]);
        sprite.extend(1i32.to_le_bytes());
        s(&mut sprite, "tag");
        p(&mut sprite, 0, 0);
        p(&mut sprite, 0, 8309331757537555771);
        assert_eq!(
            sprite_texture(&sprite).unwrap(),
            PPtr {
                file: 0,
                path: 8309331757537555771
            }
        );
    }
}
