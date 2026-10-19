fn cstr(buf: &[u8], mut i: usize) -> Result<(String, usize), String> {
    let start = i;
    while i < buf.len() && buf[i] != 0 {
        i += 1;
    }
    if i >= buf.len() {
        return Err("unterminated unityfs string".into());
    }
    let s = String::from_utf8_lossy(&buf[start..i]).into_owned();
    Ok((s, i + 1))
}

fn u32be(buf: &[u8], i: usize) -> Result<(u32, usize), String> {
    let b = buf.get(i..i + 4).ok_or("truncated unityfs")?;
    Ok((u32::from_be_bytes(b.try_into().unwrap()), i + 4))
}

fn u64be(buf: &[u8], i: usize) -> Result<(u64, usize), String> {
    let b = buf.get(i..i + 8).ok_or("truncated unityfs")?;
    Ok((u64::from_be_bytes(b.try_into().unwrap()), i + 8))
}

/// LZ4 block stream, the same framing UnityFS uses for both lz4 and lz4hc
pub fn lz4_block(src: &[u8], usize_out: usize) -> Result<Vec<u8>, String> {
    let mut out = Vec::with_capacity(usize_out);
    lz4_block_into(src, usize_out, &mut out)?;
    Ok(out)
}

/// Decode one LZ4 block and append exactly `size` bytes to `out`. Matches may only reach back to the
/// start of this block, which is how UnityFS writes them (blocks are independent)
pub fn lz4_block_into(src: &[u8], size: usize, out: &mut Vec<u8>) -> Result<(), String> {
    let base = out.len();
    let end = base + size;
    // LZ4 expands at most ~255x, so a corrupt size field cannot make this reserve gigabytes
    out.reserve(size.min(src.len().saturating_mul(255)));
    let mut p = 0;
    while out.len() < end {
        let token = *src.get(p).ok_or("lz4 truncated")?;
        p += 1;
        let mut lit = (token >> 4) as usize;
        if lit == 15 {
            loop {
                let b = *src.get(p).ok_or("lz4 truncated")? as usize;
                p += 1;
                lit += b;
                if b != 255 {
                    break;
                }
            }
        }
        let lit_end = p + lit;
        if lit_end > src.len() {
            return Err("lz4 literal overrun".into());
        }
        out.extend_from_slice(&src[p..lit_end]);
        p = lit_end;
        if out.len() >= end {
            break;
        }
        let offset = *src.get(p).ok_or("lz4 truncated")? as usize
            | ((*src.get(p + 1).ok_or("lz4 truncated")? as usize) << 8);
        p += 2;
        if offset == 0 || offset > out.len() - base {
            return Err("lz4 bad offset".into());
        }
        let mut match_len = (token & 0x0f) as usize + 4;
        if token & 0x0f == 15 {
            loop {
                let b = *src.get(p).ok_or("lz4 truncated")? as usize;
                p += 1;
                match_len += b;
                if b != 255 {
                    break;
                }
            }
        }
        // Nothing past `end` is kept, so do not copy it
        let match_len = match_len.min(end - out.len());
        let start = out.len() - offset;
        if offset >= match_len {
            out.extend_from_within(start..start + match_len);
        } else if offset == 1 {
            let b = out[start];
            out.resize(out.len() + match_len, b);
        } else {
            // Overlapping match: each byte may repeat one written earlier in this same match
            for k in 0..match_len {
                let b = out[start + k];
                out.push(b);
            }
        }
    }
    out.truncate(end);
    Ok(())
}

/// One LZMA block as UnityFS stores it: the 5-byte properties header, then the stream with no size field
fn lzma_into(src: &[u8], size: usize, out: &mut Vec<u8>) -> Result<(), String> {
    let base = out.len();
    let options = lzma_rs::decompress::Options {
        unpacked_size: lzma_rs::decompress::UnpackedSize::UseProvided(Some(size as u64)),
        ..Default::default()
    };
    lzma_rs::lzma_decompress_with_options(&mut &src[..], out, &options)
        .map_err(|e| format!("lzma: {e}"))?;
    if out.len() - base != size {
        return Err(format!(
            "lzma: {} bytes, block says {size}",
            out.len() - base
        ));
    }
    Ok(())
}

/// Append one block of `size` bytes compressed with `method` (the low 6 bits of its flags)
fn block_into(src: &[u8], method: u32, size: usize, out: &mut Vec<u8>) -> Result<(), String> {
    match method {
        0 => out.extend_from_slice(src.get(..size).ok_or("unityfs block truncated")?),
        1 => lzma_into(src, size, out)?,
        2 | 3 => lz4_block_into(src, size, out)?,
        other => return Err(format!("unityfs block compression {other}")),
    }
    Ok(())
}

#[derive(Clone)]
pub struct Node {
    pub name: String,
    pub bytes: Vec<u8>,
}

struct Archive {
    blocks: Vec<(u32, u32, u16)>,
    nodes: Vec<(String, usize, usize)>,
    data_off: usize,
}

fn parse_archive(data: &[u8]) -> Result<Archive, String> {
    if data.len() < 32 || &data[..7] != b"UnityFS" {
        return Err("not a UnityFS archive".into());
    }
    let mut i = 8;
    let (_version, n) = u32be(data, i)?;
    i = n;
    let (_unity, n) = cstr(data, i)?;
    i = n;
    let (_rev, n) = cstr(data, i)?;
    i = n;
    let (_size, n) = u64be(data, i)?;
    i = n;
    let (csize, n) = u32be(data, i)?;
    i = n;
    let (usize_info, n) = u32be(data, i)?;
    i = n;
    let (flags, n) = u32be(data, i)?;
    i = n;
    if flags & 0x200 != 0 {
        i = (i + 15) & !15;
    }
    let info_at = if flags & 0x80 != 0 {
        data.len()
            .checked_sub(csize as usize)
            .ok_or("unityfs block info past start")?
    } else {
        i
    };
    let info_comp = data
        .get(info_at..info_at + csize as usize)
        .ok_or("unityfs block info truncated")?;
    let mut info = Vec::with_capacity(usize_info as usize);
    block_into(info_comp, flags & 0x3f, usize_info as usize, &mut info)?;
    let mut o = 16;
    let (nb, n) = u32be(&info, o)?;
    o = n;
    let mut blocks = Vec::with_capacity(nb as usize);
    for _ in 0..nb {
        let (uncomp, n) = u32be(&info, o)?;
        o = n;
        let (comp, n) = u32be(&info, o)?;
        o = n;
        let flags_b = u16::from_be_bytes(
            info.get(o..o + 2)
                .ok_or("truncated block flag")?
                .try_into()
                .unwrap(),
        );
        o += 2;
        blocks.push((uncomp, comp, flags_b));
    }
    let (nn, n) = u32be(&info, o)?;
    o = n;
    let mut nodes = Vec::with_capacity(nn as usize);
    for _ in 0..nn {
        let (off, n) = u64be(&info, o)?;
        o = n;
        let (sz, n) = u64be(&info, o)?;
        o = n;
        let (_nfl, n) = u32be(&info, o)?;
        o = n;
        let (name, n) = cstr(&info, o)?;
        o = n;
        nodes.push((name, off as usize, sz as usize));
    }
    let mut data_off = if flags & 0x80 != 0 {
        i
    } else {
        info_at + csize as usize
    };
    data_off = (data_off + 15) & !15;
    Ok(Archive {
        blocks,
        nodes,
        data_off,
    })
}

fn inflate_until(data: &[u8], archive: &Archive, need: usize) -> Result<Vec<u8>, String> {
    // Size the buffer once from the block table (capped so a corrupt header cannot reserve gigabytes)
    let mut total = 0usize;
    for (uncomp, _, _) in &archive.blocks {
        total = total.saturating_add(*uncomp as usize);
        if need > 0 && total >= need {
            break;
        }
    }
    let mut raw = Vec::with_capacity(total.min(1 << 30));
    let mut p = archive.data_off;
    for (uncomp, comp, fl) in &archive.blocks {
        let chunk = data
            .get(p..p + *comp as usize)
            .ok_or("unityfs data truncated")?;
        p += *comp as usize;
        block_into(chunk, u32::from(fl & 0x3f), *uncomp as usize, &mut raw)?;
        if need > 0 && raw.len() >= need {
            break;
        }
    }
    Ok(raw)
}

pub fn read_named(data: &[u8], name: &str) -> Result<Vec<u8>, String> {
    let archive = parse_archive(data)?;
    let (_, off, sz) = archive
        .nodes
        .iter()
        .find(|(n, _, _)| n == name)
        .ok_or_else(|| format!("unityfs has no {name}"))?;
    let need = off + sz;
    let raw = inflate_until(data, &archive, need)?;
    raw.get(*off..need)
        .map(|b| b.to_vec())
        .ok_or_else(|| "unityfs node past data".to_string())
}

/// Streamed data (`.resS`, `.resource`) beside a bundle's serialized files
pub fn is_resource(name: &str) -> bool {
    name.ends_with(".resS") || name.ends_with(".resource")
}

pub fn decode(data: &[u8]) -> Result<Vec<Node>, String> {
    decode_where(data, |_| true)
}

pub fn decode_where(data: &[u8], keep: impl Fn(&str) -> bool) -> Result<Vec<Node>, String> {
    let archive = parse_archive(data)?;
    let raw = inflate_until(data, &archive, 0)?;
    let mut out = Vec::new();
    for (name, off, sz) in archive.nodes {
        if !keep(&name) {
            continue;
        }
        let end = off.checked_add(sz).ok_or("unityfs node size")?;
        let bytes = raw.get(off..end).ok_or("unityfs node past data")?.to_vec();
        out.push(Node { name, bytes });
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn decodes_overlapping_and_plain_lz4_matches_relative_to_the_block() {
        // "abc" + 10 bytes at offset 3, "Z" + 5 at offset 1, "xy" + 4 at offset 8, then "!"
        let block = [
            0x36, b'a', b'b', b'c', 3, 0, 0x11, b'Z', 1, 0, 0x20, b'x', b'y', 8, 0, 0x10, b'!',
        ];
        let mut out = b"PRE".to_vec();
        lz4_block_into(&block, 26, &mut out).unwrap();
        assert_eq!(&out[..], b"PREabcabcabcabcaZZZZZZxyZZZZ!");
        // An offset reaching before this block's start is corrupt, even though bytes exist there
        let mut out = b"PRE".to_vec();
        assert!(lz4_block_into(&[0x10, b'a', 2, 0], 6, &mut out).is_err());
    }

    #[test]
    fn decodes_unity_lzma_blocks_after_other_bytes() {
        let payload = b"level0 level0 level0 sharedassets0.assets".repeat(40);
        let mut block = Vec::new();
        let options = lzma_rs::compress::Options {
            unpacked_size: lzma_rs::compress::UnpackedSize::SkipWritingToHeader,
        };
        lzma_rs::lzma_compress_with_options(&mut &payload[..], &mut block, &options).unwrap();
        let mut out = b"PRE".to_vec();
        block_into(&block, 1, payload.len(), &mut out).unwrap();
        assert_eq!(&out[..3], b"PRE");
        assert_eq!(&out[3..], &payload[..]);
        assert!(block_into(&block, 1, payload.len() + 1, &mut Vec::new()).is_err());
        assert!(block_into(&block, 4, payload.len(), &mut Vec::new()).is_err());
    }

    #[test]
    fn round_trips_an_uncompressed_archive() {
        let payload = b"hello-level0";
        let mut info = vec![0u8; 16];
        info.extend_from_slice(&1u32.to_be_bytes());
        info.extend_from_slice(&(payload.len() as u32).to_be_bytes());
        info.extend_from_slice(&(payload.len() as u32).to_be_bytes());
        info.extend_from_slice(&0u16.to_be_bytes());
        info.extend_from_slice(&1u32.to_be_bytes());
        info.extend_from_slice(&0u64.to_be_bytes());
        info.extend_from_slice(&(payload.len() as u64).to_be_bytes());
        info.extend_from_slice(&0u32.to_be_bytes());
        info.extend_from_slice(b"level0\0");
        let mut file = b"UnityFS\0".to_vec();
        file.extend_from_slice(&8u32.to_be_bytes());
        file.extend_from_slice(b"5.x.x\0");
        file.extend_from_slice(b"2022.3.62f2\0");
        let total = 0u64;
        file.extend_from_slice(&total.to_be_bytes());
        file.extend_from_slice(&(info.len() as u32).to_be_bytes());
        file.extend_from_slice(&(info.len() as u32).to_be_bytes());
        file.extend_from_slice(&0u32.to_be_bytes());
        file.extend_from_slice(&info);
        let padded = (file.len() + 15) & !15;
        file.resize(padded, 0);
        file.extend_from_slice(payload);
        let nodes = decode(&file).unwrap();
        assert_eq!(nodes.len(), 1);
        assert_eq!(nodes[0].name, "level0");
        assert_eq!(nodes[0].bytes, payload);
    }
}
