use crate::cursor::Cursor;

pub struct AssetObject {
    pub class_id: i32,
    pub start: usize,
    pub size: usize,
}

/// Objects in a Unity serialized file. `start` is an index into `file`
pub fn objects(file: &[u8]) -> Result<Vec<AssetObject>, String> {
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
        if enable_tree {
            skip_type_tree(&mut r, version)?;
        }
        if version >= 21 {
            let n = r.i32()?;
            if !(0..=10_000).contains(&n) {
                return Err(format!("type dependency count {n}"));
            }
            let _ = r.bytes(n as usize * 4)?;
        }
    }
    let object_count = r.i32()?;
    if !(0..=1_000_000).contains(&object_count) {
        return Err(format!("object count {object_count}"));
    }
    let mut out = Vec::with_capacity(object_count as usize);
    for _ in 0..object_count {
        r.align(4);
        let _path = r.u64()?;
        let mut start = if version >= 22 {
            r.u64()? as usize
        } else {
            r.u32()? as usize
        };
        let size = r.u32()? as usize;
        let type_id = r.i32()?;
        if start < data_offset {
            start += data_offset;
        }
        let class_id = *class_ids
            .get(type_id as usize)
            .ok_or("type id out of range")?;
        out.push(AssetObject {
            class_id,
            start,
            size,
        });
    }
    Ok(out)
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
