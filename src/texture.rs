use crate::cursor::Cursor;
use crate::serialized::{self, AssetObject};
use crate::unityfs::Node;
use image::codecs::png::PngEncoder;
use image::{ColorType, ImageEncoder, RgbImage};
use std::io::Cursor as IoCursor;

pub fn png_from_bundle(nodes: &[Node]) -> Result<Vec<u8>, String> {
    encode_png(&texture_from_bundle(nodes)?)
}

/// The largest readable Texture2D in the bundle, decoded to top-down RGB8 or RGBA8
pub fn texture_from_bundle(nodes: &[Node]) -> Result<Texture, String> {
    let cab = nodes
        .iter()
        .find(|n| !n.name.ends_with(".resS") && !n.name.ends_with(".resource"))
        .ok_or("bundle has no serialized file")?;
    let objects = serialized::objects(&cab.bytes)?;
    let mut best: Option<Texture> = None;
    for object in objects {
        if object.class_id != 28 {
            continue;
        }
        let Some(tex) = read_texture(&cab.bytes, &object, nodes) else {
            continue;
        };
        let pixels = tex.width.saturating_mul(tex.height);
        if best
            .as_ref()
            .map(|b| b.width.saturating_mul(b.height) < pixels)
            .unwrap_or(true)
        {
            best = Some(tex);
        }
    }
    best.ok_or_else(|| "bundle has no readable texture".to_string())
}

pub fn encode_png(tex: &Texture) -> Result<Vec<u8>, String> {
    let mut png = Vec::new();
    PngEncoder::new(IoCursor::new(&mut png))
        .write_image(&tex.pixels, tex.width, tex.height, tex.color)
        .map_err(|e| e.to_string())?;
    Ok(png)
}

pub struct Texture {
    pub width: u32,
    pub height: u32,
    pub pixels: Vec<u8>,
    pub color: ColorType,
}

impl Texture {
    /// RGB pixels with alpha dropped, the same pixels `image::open(png).to_rgb8()` returns
    pub fn into_rgb(self) -> Option<RgbImage> {
        let rgb = match self.color {
            ColorType::Rgb8 => self.pixels,
            ColorType::Rgba8 => self
                .pixels
                .chunks_exact(4)
                .flat_map(|p| [p[0], p[1], p[2]])
                .collect(),
            _ => return None,
        };
        RgbImage::from_raw(self.width, self.height, rgb)
    }
}

fn read_texture(file: &[u8], object: &AssetObject, nodes: &[Node]) -> Option<Texture> {
    let end = object.start.checked_add(object.size)?;
    let body = file.get(object.start..end)?;
    let mut r = Cursor::new(body);
    let _name = r.string().ok()?;
    let _fallback = r.i32().ok()?;
    let _down = r.u8().ok()?;
    let _alpha = r.u8().ok()?;
    r.align(4);
    let width = r.i32().ok()?;
    let height = r.i32().ok()?;
    let _complete = r.i32().ok()?;
    let _mips_stripped = r.i32().ok()?;
    let format = r.i32().ok()?;
    let _mips = r.i32().ok()?;
    let _ = r.u8().ok()?;
    let _ = r.u8().ok()?;
    let _ = r.u8().ok()?;
    let _ = r.u8().ok()?;
    r.align(4);
    let _prio = r.i32().ok()?;
    let _images = r.i32().ok()?;
    let _dim = r.i32().ok()?;
    let _filter = r.i32().ok()?;
    let _aniso = r.i32().ok()?;
    let _bias = r.f32().ok()?;
    let _wrap_u = r.i32().ok()?;
    let _wrap_v = r.i32().ok()?;
    let _wrap_w = r.i32().ok()?;
    let _light = r.i32().ok()?;
    let _color = r.i32().ok()?;
    let blob = r.i32().ok()?;
    if !(0..=1_000_000).contains(&blob) {
        return None;
    }
    let _ = r.bytes(blob as usize).ok()?;
    r.align(4);
    let inline_len = r.i32().ok()?;
    if !(0..=64_000_000).contains(&inline_len) {
        return None;
    }
    let inline = r.bytes(inline_len as usize).ok()?;
    let stream_off = r.u64().ok()? as usize;
    let stream_size = r.u32().ok()? as usize;
    let path = r.string().ok()?;
    if !(1..=8192).contains(&width) || !(1..=8192).contains(&height) {
        return None;
    }
    let raw = if stream_size > 0 {
        let node_name = path.rsplit('/').next().unwrap_or("");
        let node = nodes
            .iter()
            .find(|n| n.name == node_name || n.name.ends_with(node_name))?;
        let end = stream_off.checked_add(stream_size)?;
        node.bytes.get(stream_off..end)?
    } else if !inline.is_empty() {
        inline
    } else {
        return None;
    };
    let (color, pixels) = match format {
        3 => {
            let need = width as usize * height as usize * 3;
            if raw.len() < need {
                return None;
            }
            let mut pixels = raw[..need].to_vec();
            flip_rows(&mut pixels, width as u32, height as u32, 3);
            (ColorType::Rgb8, pixels)
        }
        4 => {
            let need = width as usize * height as usize * 4;
            if raw.len() < need {
                return None;
            }
            let mut pixels = raw[..need].to_vec();
            flip_rows(&mut pixels, width as u32, height as u32, 4);
            (ColorType::Rgba8, pixels)
        }
        34 => {
            let rgb = decode_etc1(raw, width as u32, height as u32).ok()?;
            (ColorType::Rgb8, rgb)
        }
        45 => {
            let rgb = decode_etc2_color(raw, width as u32, height as u32, false).ok()?;
            (ColorType::Rgb8, rgb)
        }
        47 => {
            let rgba = decode_etc2_color(raw, width as u32, height as u32, true).ok()?;
            (ColorType::Rgba8, rgba)
        }
        _ => return None,
    };
    Some(Texture {
        width: width as u32,
        height: height as u32,
        pixels,
        color,
    })
}

fn flip_rows(pixels: &mut [u8], width: u32, height: u32, bpp: usize) {
    let stride = width as usize * bpp;
    let h = height as usize;
    for y in 0..h / 2 {
        let (top, bottom) = pixels.split_at_mut((h - 1 - y) * stride);
        top[y * stride..(y + 1) * stride].swap_with_slice(&mut bottom[..stride]);
    }
}

const ETC2_DIST: [i32; 8] = [3, 6, 11, 16, 23, 32, 41, 64];

const EAC_MOD: [[i32; 8]; 16] = [
    [-3, -6, -9, -15, 2, 5, 8, 14],
    [-3, -7, -10, -13, 2, 6, 9, 12],
    [-2, -5, -8, -13, 1, 4, 7, 12],
    [-2, -4, -6, -13, 1, 3, 5, 12],
    [-3, -6, -8, -12, 2, 5, 7, 11],
    [-3, -7, -9, -11, 2, 6, 8, 10],
    [-4, -7, -8, -11, 3, 6, 7, 10],
    [-3, -5, -8, -11, 2, 4, 7, 10],
    [-2, -6, -8, -10, 1, 5, 7, 9],
    [-2, -5, -8, -10, 1, 4, 7, 9],
    [-2, -4, -8, -10, 1, 3, 7, 9],
    [-2, -5, -7, -10, 1, 4, 6, 9],
    [-3, -4, -7, -10, 2, 3, 6, 9],
    [-1, -2, -3, -10, 0, 1, 2, 9],
    [-4, -6, -8, -9, 3, 5, 7, 8],
    [-3, -5, -7, -9, 2, 4, 6, 8],
];

fn ext4(v: u32) -> u8 {
    ((v << 4) | v) as u8
}

fn ext6(v: u32) -> u8 {
    ((v << 2) | (v >> 4)) as u8
}

fn ext7(v: u32) -> u8 {
    ((v << 1) | (v >> 6)) as u8
}

fn selector(low: u32, px: usize, py: usize) -> i32 {
    let bit = px * 4 + py;
    let msb = ((low >> (bit + 16)) & 1) as i32;
    let lsb = ((low >> bit) & 1) as i32;
    (msb << 1) | lsb
}

fn paint(base: [u8; 3], delta: i32) -> [u8; 3] {
    [
        (base[0] as i32 + delta).clamp(0, 255) as u8,
        (base[1] as i32 + delta).clamp(0, 255) as u8,
        (base[2] as i32 + delta).clamp(0, 255) as u8,
    ]
}

fn decode_etc2_rgb_block(block: &[u8]) -> [[u8; 3]; 16] {
    let high_be = u32::from_be_bytes(block[0..4].try_into().unwrap());
    let low = u32::from_be_bytes(block[4..8].try_into().unwrap());
    let word = u32::from_le_bytes(block[0..4].try_into().unwrap());
    let diff = high_be & 2 != 0;
    let mut mode = if diff { 1 } else { 0 };
    if diff {
        let r = ((high_be >> 27) & 31) as i32;
        let mut dr = ((high_be >> 24) & 7) as i32;
        if dr >= 4 {
            dr -= 8;
        }
        let g = ((high_be >> 19) & 31) as i32;
        let mut dg = ((high_be >> 16) & 7) as i32;
        if dg >= 4 {
            dg -= 8;
        }
        let b = ((high_be >> 11) & 31) as i32;
        let mut db = ((high_be >> 8) & 7) as i32;
        if db >= 4 {
            db -= 8;
        }
        if !(0..=31).contains(&(r + dr)) {
            mode = 2;
        } else if !(0..=31).contains(&(g + dg)) {
            mode = 3;
        } else if !(0..=31).contains(&(b + db)) {
            mode = 4;
        }
    }
    let mut out = [[0u8; 3]; 16];
    if mode == 4 {
        let bits = u64::from_le_bytes(block[0..8].try_into().unwrap());
        let origin_r = ext6(((bits >> 1) & 63) as u32);
        let origin_g = ext7((((bits & 1) << 6) | ((bits >> 9) & 63)) as u32);
        let origin_b = ext6(
            ((((bits >> 8) & 1) << 5)
                | (((bits >> 19) & 3) << 3)
                | (((bits >> 16) & 3) << 1)
                | ((bits >> 31) & 1)) as u32,
        );
        let horiz_r = ext6(((((bits >> 26) & 31) << 1) | ((bits >> 24) & 1)) as u32);
        let horiz_g = ext7(((bits >> 33) & 127) as u32);
        let horiz_b = ext6(((((bits >> 32) & 1) << 5) | ((bits >> 43) & 31)) as u32);
        let vert_r = ext6(((((bits >> 40) & 7) << 3) | ((bits >> 53) & 7)) as u32);
        let vert_g = ext7(((((bits >> 48) & 31) << 2) | ((bits >> 62) & 3)) as u32);
        let vert_b = ext6(((bits >> 56) & 63) as u32);
        let o = [origin_r as i32, origin_g as i32, origin_b as i32];
        let h = [horiz_r as i32, horiz_g as i32, horiz_b as i32];
        let v = [vert_r as i32, vert_g as i32, vert_b as i32];
        for py in 0..4 {
            for px in 0..4 {
                let mut pix = [0u8; 3];
                for c in 0..3 {
                    let val =
                        (4 * o[c] + (px as i32) * (h[c] - o[c]) + (py as i32) * (v[c] - o[c]) + 2)
                            >> 2;
                    pix[c] = val.clamp(0, 255) as u8;
                }
                out[py * 4 + px] = pix;
            }
        }
        return out;
    }
    if mode == 2 || mode == 3 {
        let (c1, c2, dist) = if mode == 2 {
            let r1 = (((word >> 3) & 3) << 2) | (word & 3);
            let g1 = (word >> 12) & 15;
            let b1 = (word >> 8) & 15;
            let r2 = (word >> 20) & 15;
            let g2 = (word >> 16) & 15;
            let b2 = (word >> 28) & 15;
            let dist = ETC2_DIST[(((word >> 26) & 3) << 1 | ((word >> 24) & 1)) as usize];
            (
                [ext4(r1), ext4(g1), ext4(b1)],
                [ext4(r2), ext4(g2), ext4(b2)],
                dist,
            )
        } else {
            let r1 = (word >> 3) & 15;
            let g1 = ((word & 7) << 1) | ((word >> 12) & 1);
            let b1 = (((word >> 11) & 1) << 3) | (((word >> 8) & 3) << 1) | ((word >> 23) & 1);
            let r2 = (word >> 19) & 15;
            let g2 = (((word >> 16) & 7) << 1) | ((word >> 31) & 1);
            let b2 = (word >> 27) & 15;
            let rgb1 = (r1 << 16) | (g1 << 8) | b1;
            let rgb2 = (r2 << 16) | (g2 << 8) | b2;
            let mut dist_i = (((word >> 26) & 1) << 2) | (((word >> 24) & 1) << 1);
            if rgb1 >= rgb2 {
                dist_i += 1;
            }
            let dist = ETC2_DIST[dist_i as usize];
            (
                [ext4(r1), ext4(g1), ext4(b1)],
                [ext4(r2), ext4(g2), ext4(b2)],
                dist,
            )
        };
        let paints = if mode == 2 {
            [c1, paint(c2, dist), c2, paint(c2, -dist)]
        } else {
            [
                paint(c1, dist),
                paint(c1, -dist),
                paint(c2, dist),
                paint(c2, -dist),
            ]
        };
        for py in 0..4 {
            for px in 0..4 {
                let code = selector(low, px, py) as usize;
                out[py * 4 + px] = paints[code];
            }
        }
        return out;
    }
    // ETC1 individual or differential. Reuse the block decoder's colors by calling the same path on 8 bytes
    let colors = decode_etc1_block(block);
    out = colors;
    out
}

fn decode_etc1_block(block: &[u8]) -> [[u8; 3]; 16] {
    let high = u32::from_be_bytes(block[0..4].try_into().unwrap());
    let low = u32::from_be_bytes(block[4..8].try_into().unwrap());
    let flip = high & 1 != 0;
    let diff = high & 2 != 0;
    let (c0, c1) = if diff {
        let base = [
            ((high >> 27) & 31) as i32,
            ((high >> 19) & 31) as i32,
            ((high >> 11) & 31) as i32,
        ];
        let mut delta = [
            ((high >> 24) & 7) as i32,
            ((high >> 16) & 7) as i32,
            ((high >> 8) & 7) as i32,
        ];
        for d in &mut delta {
            if *d >= 4 {
                *d -= 8;
            }
        }
        let ext = |v: i32| {
            let v = v.clamp(0, 31) as u32;
            ((v << 3) | (v >> 2)) as u8
        };
        (
            [ext(base[0]), ext(base[1]), ext(base[2])],
            [
                ext(base[0] + delta[0]),
                ext(base[1] + delta[1]),
                ext(base[2] + delta[2]),
            ],
        )
    } else {
        (
            [
                ext4((high >> 28) & 15),
                ext4((high >> 20) & 15),
                ext4((high >> 12) & 15),
            ],
            [
                ext4((high >> 24) & 15),
                ext4((high >> 16) & 15),
                ext4((high >> 8) & 15),
            ],
        )
    };
    let table0 = ((high >> 5) & 7) as usize;
    let table1 = ((high >> 2) & 7) as usize;
    let mut out = [[0u8; 3]; 16];
    for py in 0..4usize {
        for px in 0..4usize {
            let code = selector(low, px, py);
            let first = if flip { py < 2 } else { px < 2 };
            let base = if first { c0 } else { c1 };
            let table = if first { table0 } else { table1 };
            let mag = ETC1_MOD[table][(code & 1) as usize];
            let delta = if code < 2 { mag } else { -mag };
            out[py * 4 + px] = paint(base, delta);
        }
    }
    out
}

fn decode_eac_alpha(block: &[u8]) -> [u8; 16] {
    let base = block[0] as i32;
    let multiplier = (block[1] >> 4) as i32;
    let table = (block[1] & 0x0f) as usize;
    let mut bits = 0u64;
    for (i, byte) in block[2..8].iter().enumerate() {
        bits |= (*byte as u64) << (8 * i);
    }
    let mut out = [0u8; 16];
    for (i, slot) in out.iter_mut().enumerate() {
        let index = ((bits >> (3 * i)) & 7) as usize;
        let value = base + EAC_MOD[table][index] * multiplier;
        *slot = value.clamp(0, 255) as u8;
    }
    out
}

fn decode_etc2_color(data: &[u8], width: u32, height: u32, rgba: bool) -> Result<Vec<u8>, String> {
    if !width.is_multiple_of(4) || !height.is_multiple_of(4) {
        return Err("etc2 size".into());
    }
    let bw = width as usize / 4;
    let bh = height as usize / 4;
    let block_bytes = if rgba { 16 } else { 8 };
    if data.len() < bw * bh * block_bytes {
        return Err("etc2 truncated".into());
    }
    let bpp = if rgba { 4 } else { 3 };
    let mut out = vec![0u8; width as usize * height as usize * bpp];
    for by in 0..bh {
        for bx in 0..bw {
            let o = (by * bw + bx) * block_bytes;
            // ETC2 RGBA8 blocks are the EAC alpha half first, then the colour half
            let (color, alpha) = if rgba {
                (
                    decode_etc2_rgb_block(&data[o + 8..o + 16]),
                    Some(decode_eac_alpha(&data[o..o + 8])),
                )
            } else {
                (decode_etc2_rgb_block(&data[o..o + 8]), None)
            };
            for py in 0..4 {
                for px in 0..4 {
                    let x = bx * 4 + px;
                    let y = height as usize - 1 - (by * 4 + py);
                    let i = (y * width as usize + x) * bpp;
                    let pix = color[py * 4 + px];
                    out[i] = pix[0];
                    out[i + 1] = pix[1];
                    out[i + 2] = pix[2];
                    if let Some(alpha) = &alpha {
                        out[i + 3] = alpha[py * 4 + px];
                    }
                }
            }
        }
    }
    Ok(out)
}

const ETC1_MOD: [[i32; 2]; 8] = [
    [2, 8],
    [5, 17],
    [9, 29],
    [13, 42],
    [18, 60],
    [24, 80],
    [33, 106],
    [47, 183],
];

fn decode_etc1(data: &[u8], width: u32, height: u32) -> Result<Vec<u8>, String> {
    if !width.is_multiple_of(4) || !height.is_multiple_of(4) {
        return Err("etc1 size".into());
    }
    let bw = width as usize / 4;
    let bh = height as usize / 4;
    if data.len() < bw * bh * 8 {
        return Err("etc1 truncated".into());
    }
    let mut out = vec![0u8; width as usize * height as usize * 3];
    for by in 0..bh {
        for bx in 0..bw {
            let o = (by * bw + bx) * 8;
            let high = u32::from_be_bytes(data[o..o + 4].try_into().unwrap());
            let low = u32::from_be_bytes(data[o + 4..o + 8].try_into().unwrap());
            let flip = high & 1 != 0;
            let diff = high & 2 != 0;
            let (c0, c1) = if diff {
                let base = [
                    ((high >> 27) & 31) as i32,
                    ((high >> 19) & 31) as i32,
                    ((high >> 11) & 31) as i32,
                ];
                let mut delta = [
                    ((high >> 24) & 7) as i32,
                    ((high >> 16) & 7) as i32,
                    ((high >> 8) & 7) as i32,
                ];
                for d in &mut delta {
                    if *d >= 4 {
                        *d -= 8;
                    }
                }
                let ext = |v: i32| {
                    let v = v.clamp(0, 31) as u32;
                    ((v << 3) | (v >> 2)) as u8
                };
                (
                    [ext(base[0]), ext(base[1]), ext(base[2])],
                    [
                        ext(base[0] + delta[0]),
                        ext(base[1] + delta[1]),
                        ext(base[2] + delta[2]),
                    ],
                )
            } else {
                let ext = |v: u32| ((v << 4) | v) as u8;
                (
                    [
                        ext((high >> 28) & 15),
                        ext((high >> 20) & 15),
                        ext((high >> 12) & 15),
                    ],
                    [
                        ext((high >> 24) & 15),
                        ext((high >> 16) & 15),
                        ext((high >> 8) & 15),
                    ],
                )
            };
            let table0 = ((high >> 5) & 7) as usize;
            let table1 = ((high >> 2) & 7) as usize;
            for py in 0..4usize {
                for px in 0..4usize {
                    let bit = px * 4 + py;
                    let msb = ((low >> (bit + 16)) & 1) as i32;
                    let lsb = ((low >> bit) & 1) as i32;
                    let code = (msb << 1) | lsb;
                    let first = if flip { py < 2 } else { px < 2 };
                    let base = if first { c0 } else { c1 };
                    let table = if first { table0 } else { table1 };
                    let mag = ETC1_MOD[table][(code & 1) as usize];
                    let delta = if code < 2 { mag } else { -mag };
                    let x = bx * 4 + px;
                    let y = height as usize - 1 - (by * 4 + py);
                    let i = (y * width as usize + x) * 3;
                    for c in 0..3 {
                        out[i + c] = (base[c] as i32 + delta).clamp(0, 255) as u8;
                    }
                }
            }
        }
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::unityfs;

    #[test]
    fn decodes_the_sample_illustration() {
        let path = "/tmp/ill.bundle";
        if !std::path::Path::new(path).exists() {
            return;
        }
        let nodes = unityfs::decode(&std::fs::read(path).unwrap()).unwrap();
        let png = png_from_bundle(&nodes).unwrap();
        let img = image::load_from_memory(&png).unwrap();
        assert_eq!(img.width(), 2048);
        assert_eq!(img.height(), 1080);
        if std::path::Path::new("/tmp/DataErr0r.png").exists() {
            let want = image::open("/tmp/DataErr0r.png").unwrap().to_rgb8();
            let got = img.to_rgb8();
            let mut diff = 0u64;
            let n = (got.width().min(want.width()) * got.height().min(want.height())) as u64;
            for y in 0..got.height().min(want.height()) {
                for x in 0..got.width().min(want.width()) {
                    let a = got.get_pixel(x, y).0;
                    let b = want.get_pixel(x, y).0;
                    for c in 0..3 {
                        diff += a[c].abs_diff(b[c]) as u64;
                    }
                }
            }
            let mean = diff as f64 / (n as f64 * 3.0);
            assert!(mean < 2.0, "illustration mean abs {mean}");
        }
        let avatar = "/tmp/avatar.bundle";
        if std::path::Path::new(avatar).exists() {
            let nodes = unityfs::decode(&std::fs::read(avatar).unwrap()).unwrap();
            let png = png_from_bundle(&nodes).unwrap();
            let img = image::load_from_memory(&png).unwrap();
            assert!(img.width() >= 16 && img.height() >= 16);
            let reference = "/tmp/Burn.png";
            if std::path::Path::new(reference).exists() {
                let want = image::open(reference).unwrap().to_rgb8();
                let got = img.to_rgb8();
                assert_eq!((got.width(), got.height()), (want.width(), want.height()));
                let mut diff = 0u64;
                for (a, b) in got.pixels().zip(want.pixels()) {
                    for c in 0..3 {
                        diff += a.0[c].abs_diff(b.0[c]) as u64;
                    }
                }
                let mean = diff as f64 / (got.width() * got.height() * 3) as f64;
                assert!(mean < 2.0, "etc1 mean abs {mean}");
            }
        }
        if std::path::Path::new("/tmp/bad.bundle").exists()
            && std::path::Path::new("/tmp/Aphasia.png").exists()
        {
            let nodes = unityfs::decode(&std::fs::read("/tmp/bad.bundle").unwrap()).unwrap();
            let png = png_from_bundle(&nodes).unwrap();
            let got = image::load_from_memory(&png).unwrap().to_rgb8();
            let want = image::open("/tmp/Aphasia.png").unwrap().to_rgb8();
            assert_eq!((got.width(), got.height()), (want.width(), want.height()));
            let mut diff = 0u64;
            for (a, b) in got.pixels().zip(want.pixels()) {
                for c in 0..3 {
                    diff += a.0[c].abs_diff(b.0[c]) as u64;
                }
            }
            let mean = diff as f64 / (got.width() * got.height() * 3) as f64;
            assert!(mean < 2.0, "etc2 mean abs {mean}");
        }
    }
}
