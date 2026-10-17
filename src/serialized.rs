use crate::cursor::Cursor;

pub struct AssetObject {
    pub path_id: i64,
    pub class_id: i32,
    pub start: usize,
    pub size: usize,
}

impl AssetObject {
    pub fn bytes<'a>(&self, file: &'a [u8]) -> Option<&'a [u8]> {
        file.get(self.start..self.start.checked_add(self.size)?)
    }
}

/// A serialized file's objects and the files its PPtrs reach: FileID `n` is `externals[n - 1]`, a path such as
/// `archive:/CAB-<hash>/CAB-<hash>` (its last part names a file in the same bundle) or `Library/unity default resources`
pub struct Serialized {
    pub objects: Vec<AssetObject>,
    pub externals: Vec<String>,
}

/// Objects in a Unity serialized file. `start` is an index into `file`: the file stores it relative to
/// the data section
pub fn objects(file: &[u8]) -> Result<Vec<AssetObject>, String> {
    read_objects(file).map(|(objects, _, _)| objects)
}

/// The objects and, after them, the external files
pub fn parse(file: &[u8]) -> Result<Serialized, String> {
    let (objects, mut r, version) = read_objects(file)?;
    if version >= 11 {
        let scripts = r.i32()?;
        if !(0..=100_000).contains(&scripts) {
            return Err(format!("script type count {scripts}"));
        }
        for _ in 0..scripts {
            let _file = r.i32()?;
            r.align(4);
            let _path = r.i64()?;
        }
    }
    let count = r.i32()?;
    if !(0..=10_000).contains(&count) {
        return Err(format!("external file count {count}"));
    }
    let mut externals = Vec::with_capacity(count as usize);
    for _ in 0..count {
        let _empty = cstr(file, &mut r)?;
        let _guid = r.bytes(16)?;
        let _kind = r.i32()?;
        externals.push(cstr(file, &mut r)?);
    }
    Ok(Serialized { objects, externals })
}

fn read_objects(file: &[u8]) -> Result<(Vec<AssetObject>, Cursor<'_>, u32), String> {
    let mut r = Cursor::new(file);
    // The leading header fields are big-endian until the endian byte
    let _meta = be_u32(file, &mut r)?;
    let _file_size = be_u32(file, &mut r)?;
    let version = be_u32(file, &mut r)?;
    let mut data_offset = be_u32(file, &mut r)? as usize;
    if version >= 9 {
        let big = r.u8()? != 0;
        let _ = r.bytes(3)?;
        if version >= 22 {
            let _metadata = be_u32(file, &mut r)?;
            let _file_size = be_u64(file, &mut r)?;
            data_offset = be_u64(file, &mut r)? as usize;
            let _unknown = be_u64(file, &mut r)?;
            if big {
                return Err("big-endian serialized file".into());
            }
        }
    }
    let _unity = cstr(file, &mut r)?;
    let _platform = r.i32()?;
    let enable_tree = r.bool()?;
    let type_count = r.i32()?;
    if !(0..=10_000).contains(&type_count) {
        return Err(format!("type count {type_count}"));
    }
    let mut class_ids = Vec::with_capacity(type_count as usize);
    for _ in 0..type_count {
        let class_id = r.i32()?;
        class_ids.push(class_id);
        if version >= 16 {
            let _stripped = r.u8()?;
        }
        if version >= 17 {
            let _script = r.bytes(2)?;
        }
        if version >= 13 && ((version >= 16 && class_id == 114) || (version < 16 && class_id < 0)) {
            let _ = r.bytes(16)?;
        }
        if version >= 13 {
            let _ = r.bytes(16)?;
        }
        // the dependency list belongs to the type tree
        if enable_tree {
            skip_type_tree(&mut r, version)?;
            if version >= 21 {
                let n = r.i32()?;
                if !(0..=10_000).contains(&n) {
                    return Err(format!("type dependency count {n}"));
                }
                let _ = r.bytes(n as usize * 4)?;
            }
        }
    }
    let object_count = r.i32()?;
    if !(0..=1_000_000).contains(&object_count) {
        return Err(format!("object count {object_count}"));
    }
    let mut out = Vec::with_capacity(object_count as usize);
    for _ in 0..object_count {
        r.align(4);
        let path_id = r.i64()?;
        let relative = if version >= 22 {
            r.u64()? as usize
        } else {
            r.u32()? as usize
        };
        let start = relative.checked_add(data_offset).ok_or("object offset")?;
        let size = r.u32()? as usize;
        let type_id = r.i32()?;
        let class_id = *class_ids
            .get(type_id as usize)
            .ok_or("type id out of range")?;
        out.push(AssetObject {
            path_id,
            class_id,
            start,
            size,
        });
    }
    Ok((out, r, version))
}

fn skip_type_tree(r: &mut Cursor, version: u32) -> Result<(), String> {
    let nodes = r.i32()?;
    let string_size = r.i32()?;
    if nodes < 0 || string_size < 0 {
        return Err("bad type tree".into());
    }
    let node_bytes = 24 + if version >= 19 { 8 } else { 0 };
    let _ = r.bytes(nodes as usize * node_bytes)?;
    let _ = r.bytes(string_size as usize)?;
    Ok(())
}

fn be_u32(file: &[u8], r: &mut Cursor) -> Result<u32, String> {
    let b = r.bytes(4)?;
    let _ = file;
    Ok(u32::from_be_bytes(b.try_into().unwrap()))
}

fn be_u64(file: &[u8], r: &mut Cursor) -> Result<u64, String> {
    let b = r.bytes(8)?;
    let _ = file;
    Ok(u64::from_be_bytes(b.try_into().unwrap()))
}

fn cstr(file: &[u8], r: &mut Cursor) -> Result<String, String> {
    let start = r.pos();
    while r.pos() < file.len() && file[r.pos()] != 0 {
        r.u8()?;
    }
    let end = r.pos();
    let _ = r.u8()?;
    Ok(String::from_utf8_lossy(&file[start..end]).into_owned())
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A version 22 serialized file with no type tree: one MonoBehaviour type, `objects` as (path ID, size), one
    /// script reference and `externals`
    fn file(objects: &[(i64, u32)], externals: &[&str]) -> Vec<u8> {
        let mut b = Vec::new();
        for v in [0u32, 0, 22, 0] {
            b.extend(v.to_be_bytes());
        }
        b.extend([0, 0, 0, 0]);
        b.extend(0u32.to_be_bytes());
        for v in [0u64, 4096, 0] {
            b.extend(v.to_be_bytes());
        }
        b.extend(b"2022.3.62f2\0");
        b.extend(13i32.to_le_bytes());
        b.push(0);
        b.extend(1i32.to_le_bytes());
        b.extend(114i32.to_le_bytes());
        b.push(0);
        b.extend(0i16.to_le_bytes());
        b.extend([0u8; 32]);
        b.extend((objects.len() as i32).to_le_bytes());
        let mut offset = 0u64;
        for (path, size) in objects {
            while b.len() % 4 != 0 {
                b.push(0);
            }
            b.extend(path.to_le_bytes());
            b.extend(offset.to_le_bytes());
            b.extend(size.to_le_bytes());
            b.extend(0i32.to_le_bytes());
            offset += *size as u64;
        }
        b.extend(1i32.to_le_bytes());
        b.extend(1i32.to_le_bytes());
        while b.len() % 4 != 0 {
            b.push(0);
        }
        b.extend(4i64.to_le_bytes());
        b.extend((externals.len() as i32).to_le_bytes());
        for path in externals {
            b.push(0);
            b.extend([9u8; 16]);
            b.extend(0i32.to_le_bytes());
            b.extend(path.as_bytes());
            b.push(0);
        }
        b
    }

    #[test]
    fn reads_path_ids_and_external_files() {
        let bytes = file(
            &[(1, 12), (-8255244746183859794, 95)],
            &[
                "archive:/CAB-0e26/CAB-0e26.sharedAssets",
                "Library/unity default resources",
            ],
        );
        let parsed = parse(&bytes).unwrap();
        let objects: Vec<(i64, i32, usize, usize)> = parsed
            .objects
            .iter()
            .map(|o| (o.path_id, o.class_id, o.start, o.size))
            .collect();
        assert_eq!(
            objects,
            [(1, 114, 4096, 12), (-8255244746183859794, 114, 4108, 95)]
        );
        assert_eq!(
            parsed.externals,
            [
                "archive:/CAB-0e26/CAB-0e26.sharedAssets",
                "Library/unity default resources"
            ]
        );
        assert_eq!(objects.len(), super::objects(&bytes).unwrap().len());
        // A file cut inside its externals still lists its objects
        let cut = &bytes[..bytes.len() - 10];
        assert!(parse(cut).is_err());
        assert_eq!(super::objects(cut).unwrap().len(), 2);
    }
}
