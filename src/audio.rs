const SETUPS: &[u8] = include_bytes!("fsb_setups.bin");

struct Bits<'a> {
    data: &'a [u8],
    bit: usize,
}

impl<'a> Bits<'a> {
    fn new(data: &'a [u8]) -> Self {
        Self { data, bit: 0 }
    }

    fn read(&mut self, n: u32) -> Result<u32, String> {
        let mut v = 0u32;
        for i in 0..n {
            let byte = *self
                .data
                .get(self.bit / 8)
                .ok_or("vorbis bitstream truncated")?;
            let bit = (byte >> (self.bit % 8)) & 1;
            v |= (bit as u32) << i;
            self.bit += 1;
        }
        Ok(v)
    }
}

fn ilog(mut x: usize) -> u32 {
    let mut n = 0;
    while x > 0 {
        x >>= 1;
        n += 1;
    }
    n
}

fn lookup1_values(entries: usize, dim: usize) -> Result<usize, String> {
    if dim == 0 {
        return Ok(0);
    }
    let mut vals = (entries as f64).powf(1.0 / dim as f64).floor() as isize;
    for _ in 0..64 {
        if vals < 0 {
            return Err("codebook quantvals".into());
        }
        let mut acc: i128 = 1;
        let mut acc1: i128 = 1;
        for _ in 0..dim {
            acc = acc.saturating_mul(vals as i128);
            acc1 = acc1.saturating_mul((vals as i128) + 1);
        }
        if acc <= entries as i128 && acc1 > entries as i128 {
            return Ok(vals as usize);
        }
        if acc > entries as i128 {
            vals -= 1;
        } else {
            vals += 1;
        }
    }
    Err("codebook quantvals".into())
}

fn skip_book(b: &mut Bits) -> Result<(), String> {
    if b.read(24)? != 0x564342 {
        return Err("codebook sync".into());
    }
    let dim = b.read(16)? as usize;
    let entries = b.read(24)? as usize;
    if entries > 16_000_000 {
        return Err("codebook entries".into());
    }
    if b.read(1)? == 0 {
        let unused = b.read(1)?;
        for _ in 0..entries {
            if unused == 0 || b.read(1)? == 1 {
                let _ = b.read(5)?;
            }
        }
    } else {
        let mut _length = b.read(5)? + 1;
        let mut i = 0;
        while i < entries {
            let num = b.read(ilog(entries - i))? as usize;
            if num > entries - i {
                return Err("ordered codebook".into());
            }
            i += num;
            _length += 1;
        }
    }
    let maptype = b.read(4)?;
    if maptype == 1 || maptype == 2 {
        let _ = b.read(32)?;
        let _ = b.read(32)?;
        let quant = b.read(4)? + 1;
        let _ = b.read(1)?;
        let vals = if maptype == 1 {
            lookup1_values(entries, dim)?
        } else {
            entries.saturating_mul(dim)
        };
        for _ in 0..vals {
            let _ = b.read(quant)?;
        }
    } else if maptype != 0 {
        return Err(format!("codebook map {maptype}"));
    }
    Ok(())
}

fn skip_floor1(b: &mut Bits) -> Result<(), String> {
    let partitions = b.read(5)? as usize;
    let mut classes = Vec::with_capacity(partitions);
    let mut max_class = -1i32;
    for _ in 0..partitions {
        let class = b.read(4)? as i32;
        max_class = max_class.max(class);
        classes.push(class as usize);
    }
    let mut dims = Vec::new();
    for _ in 0..(max_class + 1) as usize {
        dims.push(b.read(3)? as usize + 1);
        let subs = b.read(2)?;
        if subs != 0 {
            let _ = b.read(8)?;
        }
        for _ in 0..(1usize << subs) {
            let _ = b.read(8)?;
        }
    }
    let _mult = b.read(2)?;
    let rangebits = b.read(4)?;
    for class in classes {
        let dim = *dims.get(class).ok_or("floor class")?;
        for _ in 0..dim {
            let _ = b.read(rangebits)?;
        }
    }
    Ok(())
}

fn skip_floor0(b: &mut Bits) -> Result<(), String> {
    let _ = b.read(8)?;
    let _ = b.read(16)?;
    let _ = b.read(16)?;
    let _ = b.read(6)?;
    let _ = b.read(8)?;
    let n = b.read(4)? + 1;
    for _ in 0..n {
        let _ = b.read(8)?;
    }
    Ok(())
}

fn skip_residue(b: &mut Bits) -> Result<(), String> {
    let _ = b.read(24)?;
    let _ = b.read(24)?;
    let _ = b.read(24)?;
    let partitions = b.read(6)? + 1;
    let _ = b.read(8)?;
    let mut books = 0usize;
    for _ in 0..partitions {
        let mut cascade = b.read(3)?;
        if b.read(1)? == 1 {
            cascade |= b.read(5)? << 3;
        }
        books += cascade.count_ones() as usize;
    }
    for _ in 0..books {
        let _ = b.read(8)?;
    }
    Ok(())
}

fn skip_mapping(b: &mut Bits, channels: usize) -> Result<(), String> {
    let submaps = if b.read(1)? == 1 { b.read(4)? + 1 } else { 1 };
    if b.read(1)? == 1 {
        let steps = b.read(8)? + 1;
        let bits = ilog(channels.saturating_sub(1));
        for _ in 0..steps {
            let _ = b.read(bits)?;
            let _ = b.read(bits)?;
        }
    }
    if b.read(2)? != 0 {
        return Err("reserved mapping bits".into());
    }
    if submaps > 1 {
        for _ in 0..channels {
            let _ = b.read(4)?;
        }
    }
    for _ in 0..submaps {
        let _ = b.read(8)?;
        let _ = b.read(8)?;
        let _ = b.read(8)?;
    }
    Ok(())
}

/// Short block is 256 samples and long is 2048, which is what FMOD writes into the id header
pub fn mode_blockflags(setup: &[u8]) -> Result<Vec<bool>, String> {
    let mut b = Bits::new(setup);
    if b.read(8)? != 5 {
        return Err("setup packet type".into());
    }
    let mut name = [0u8; 6];
    for slot in &mut name {
        *slot = b.read(8)? as u8;
    }
    if &name != b"vorbis" {
        return Err("setup packet name".into());
    }
    let books = b.read(8)? + 1;
    for _ in 0..books {
        skip_book(&mut b)?;
    }
    let times = b.read(6)? + 1;
    for _ in 0..times {
        if b.read(16)? != 0 {
            return Err("unsupported vorbis time backend".into());
        }
    }
    let floors = b.read(6)? + 1;
    for _ in 0..floors {
        match b.read(16)? {
            0 => skip_floor0(&mut b)?,
            1 => skip_floor1(&mut b)?,
            other => return Err(format!("floor type {other}")),
        }
    }
    let residues = b.read(6)? + 1;
    for _ in 0..residues {
        let _kind = b.read(16)?;
        skip_residue(&mut b)?;
    }
    let maps = b.read(6)? + 1;
    for _ in 0..maps {
        if b.read(16)? != 0 {
            return Err("mapping type".into());
        }
        skip_mapping(&mut b, 2)?;
    }
    let modes = b.read(6)? + 1;
    let mut flags = Vec::with_capacity(modes as usize);
    for _ in 0..modes {
        flags.push(b.read(1)? == 1);
        let _window = b.read(16)?;
        let _transform = b.read(16)?;
        let _mapping = b.read(8)?;
    }
    if b.read(1)? != 1 {
        return Err("setup framing bit".into());
    }
    Ok(flags)
}

fn setup_packet(crc: u32) -> Result<&'static [u8], String> {
    if SETUPS.len() < 4 {
        return Err("empty vorbis setup table".into());
    }
    let count = u32::from_le_bytes(SETUPS[0..4].try_into().unwrap()) as usize;
    let mut i = 4;
    for _ in 0..count {
        if i + 8 > SETUPS.len() {
            break;
        }
        let key = u32::from_le_bytes(SETUPS[i..i + 4].try_into().unwrap());
        let len = u32::from_le_bytes(SETUPS[i + 4..i + 8].try_into().unwrap()) as usize;
        i += 8;
        let blob = SETUPS.get(i..i + len).ok_or("truncated setup table")?;
        if key == crc {
            return Ok(blob);
        }
        i += len;
    }
    Err(format!("no vorbis setup for crc {crc:#x}"))
}

/// Tables for the Ogg page CRC (polynomial 0x04c11db7, MSB first) read eight bytes at a time:
/// `CRC_TABLES[k][b]` is the CRC of byte `b` followed by `k` zero bytes. A one-byte table is no faster
/// than the bitwise loop, because each step waits on the previous one
const fn crc_tables() -> [[u32; 256]; 8] {
    let mut tables = [[0u32; 256]; 8];
    let mut i = 0;
    while i < 256 {
        let mut crc = (i as u32) << 24;
        let mut k = 0;
        while k < 8 {
            crc = if crc & 0x8000_0000 != 0 {
                (crc << 1) ^ 0x04c1_1db7
            } else {
                crc << 1
            };
            k += 1;
        }
        tables[0][i] = crc;
        i += 1;
    }
    let mut k = 1;
    while k < 8 {
        let mut i = 0;
        while i < 256 {
            let prev = tables[k - 1][i];
            tables[k][i] = (prev << 8) ^ tables[0][(prev >> 24) as usize];
            i += 1;
        }
        k += 1;
    }
    tables
}

static CRC_TABLES: [[u32; 256]; 8] = crc_tables();

fn ogg_crc(data: &[u8]) -> u32 {
    let t = &CRC_TABLES;
    let mut crc = 0u32;
    let mut words = data.chunks_exact(8);
    for w in &mut words {
        let hi = crc ^ u32::from_be_bytes([w[0], w[1], w[2], w[3]]);
        crc = t[7][(hi >> 24) as usize]
            ^ t[6][(hi >> 16) as usize & 0xff]
            ^ t[5][(hi >> 8) as usize & 0xff]
            ^ t[4][hi as usize & 0xff]
            ^ t[3][w[4] as usize]
            ^ t[2][w[5] as usize]
            ^ t[1][w[6] as usize]
            ^ t[0][w[7] as usize];
    }
    for &byte in words.remainder() {
        crc = (crc << 8) ^ t[0][((crc >> 24) as u8 ^ byte) as usize];
    }
    crc
}

/// Ogg pages for one logical stream. A page carries at most 255 lacing values, so a page is flushed
/// early when the next packet would not fit, and a packet longer than one page continues on the next
struct OggStream {
    out: Vec<u8>,
    serial: u32,
    seq: u32,
    lacing: Vec<u8>,
    body: Vec<u8>,
    packets: usize,
    granule: i64,
    continued: bool,
    last_page: Option<usize>,
}

impl OggStream {
    fn new(serial: u32, capacity: usize) -> Self {
        Self {
            out: Vec::with_capacity(capacity),
            serial,
            seq: 0,
            lacing: Vec::with_capacity(255),
            body: Vec::new(),
            packets: 0,
            granule: 0,
            continued: false,
            last_page: None,
        }
    }

    fn push(&mut self, packet: &[u8], granule: i64) {
        if self.packets > 0 && self.lacing.len() + packet.len() / 255 + 1 > 255 {
            self.flush(0);
        }
        let mut rest = packet;
        loop {
            let n = rest.len().min(255);
            self.lacing.push(n as u8);
            self.body.extend_from_slice(&rest[..n]);
            rest = &rest[n..];
            if n < 255 {
                break;
            }
            if self.lacing.len() == 255 {
                self.flush(0);
                self.continued = true;
            }
        }
        self.packets += 1;
        self.granule = granule;
    }

    fn flush(&mut self, extra_flags: u8) {
        if self.lacing.is_empty() {
            return;
        }
        let mut flags = extra_flags;
        if self.continued {
            flags |= 0x01;
        }
        if self.last_page.is_none() {
            flags |= 0x02;
        }
        // A page on which no packet ends carries granule -1
        let granule = if self.packets > 0 { self.granule } else { -1 };
        let start = self.out.len();
        self.out.extend_from_slice(b"OggS");
        self.out.push(0);
        self.out.push(flags);
        self.out.extend_from_slice(&granule.to_le_bytes());
        self.out.extend_from_slice(&self.serial.to_le_bytes());
        self.out.extend_from_slice(&self.seq.to_le_bytes());
        self.out.extend_from_slice(&0u32.to_le_bytes());
        self.out.push(self.lacing.len() as u8);
        self.out.extend_from_slice(&self.lacing);
        self.out.extend_from_slice(&self.body);
        let crc = ogg_crc(&self.out[start..]);
        self.out[start + 22..start + 26].copy_from_slice(&crc.to_le_bytes());
        self.seq += 1;
        self.last_page = Some(start);
        self.lacing.clear();
        self.body.clear();
        self.packets = 0;
        self.continued = false;
    }

    /// Flush the open page with end-of-stream, or mark the last written page when nothing is open
    fn finish(mut self) -> Vec<u8> {
        if !self.lacing.is_empty() {
            self.flush(0x04);
        } else if let Some(start) = self.last_page {
            self.out[start + 5] |= 0x04;
            self.out[start + 22..start + 26].copy_from_slice(&[0; 4]);
            let len = 27 + self.out[start + 26] as usize;
            let body: usize = self.out[start + 27..start + len]
                .iter()
                .map(|n| *n as usize)
                .sum();
            let crc = ogg_crc(&self.out[start..start + len + body]);
            self.out[start + 22..start + 26].copy_from_slice(&crc.to_le_bytes());
        }
        self.out
    }
}

fn ident_packet(channels: u8, rate: u32) -> Vec<u8> {
    let mut p = vec![1];
    p.extend_from_slice(b"vorbis");
    p.extend_from_slice(&0u32.to_le_bytes());
    p.push(channels);
    p.extend_from_slice(&rate.to_le_bytes());
    p.extend_from_slice(&0u32.to_le_bytes());
    p.extend_from_slice(&0u32.to_le_bytes());
    p.extend_from_slice(&0u32.to_le_bytes());
    // blocksize short 2^8, long 2^11, framing bit. Packed LSB first into the last byte
    p.push((8 & 0x0f) | ((11 & 0x0f) << 4));
    p.push(1);
    p
}

fn comment_packet() -> Vec<u8> {
    let mut p = vec![3];
    p.extend_from_slice(b"vorbis");
    p.extend_from_slice(&0u32.to_le_bytes());
    p.extend_from_slice(&0u32.to_le_bytes());
    p.push(1);
    p
}

fn packet_blocksize(packet: &[u8], flags: &[bool]) -> Result<i64, String> {
    let mut b = Bits::new(packet);
    if b.read(1)? != 0 {
        return Err("non-audio vorbis packet".into());
    }
    let width = ilog(flags.len().saturating_sub(1));
    let mode = if width == 0 {
        0
    } else {
        b.read(width)? as usize
    };
    let long = *flags.get(mode).ok_or("vorbis mode")?;
    Ok(if long { 2048 } else { 256 })
}

struct Sample<'a> {
    channels: u8,
    rate: u32,
    crc: u32,
    data: &'a [u8],
}

fn read_fsb(bytes: &[u8]) -> Result<Sample<'_>, String> {
    if bytes.len() < 60 || &bytes[..4] != b"FSB5" {
        return Err("not an FSB5 file".into());
    }
    let version = u32::from_le_bytes(bytes[4..8].try_into().unwrap());
    let samples = u32::from_le_bytes(bytes[8..12].try_into().unwrap());
    let header_size = u32::from_le_bytes(bytes[12..16].try_into().unwrap()) as usize;
    let name_size = u32::from_le_bytes(bytes[16..20].try_into().unwrap()) as usize;
    let mode = u32::from_le_bytes(bytes[24..28].try_into().unwrap());
    if samples == 0 {
        return Err("fsb has no samples".into());
    }
    if mode != 15 {
        return Err(format!("fsb mode {mode} is not vorbis"));
    }
    let mut header_at = 60;
    if version == 0 {
        header_at += 4;
    }
    let base = u64::from_le_bytes(
        bytes
            .get(header_at..header_at + 8)
            .ok_or("fsb sample")?
            .try_into()
            .unwrap(),
    );
    let mut next = base & 1;
    let freq_id = ((base >> 1) & 0xf) as u32;
    let channels = (((base >> 5) & 1) as u8) + 1;
    let mut at = header_at + 8;
    let mut rate = match freq_id {
        1 => 8000,
        2 => 11000,
        3 => 11025,
        4 => 16000,
        5 => 22050,
        6 => 24000,
        7 => 32000,
        8 => 44100,
        9 => 48000,
        _ => 0,
    };
    let mut crc = None;
    while next == 1 {
        let raw = u32::from_le_bytes(
            bytes
                .get(at..at + 4)
                .ok_or("fsb chunk")?
                .try_into()
                .unwrap(),
        );
        at += 4;
        next = (raw & 1) as u64;
        let size = ((raw >> 1) & 0x00ff_ffff) as usize;
        let kind = (raw >> 25) & 0x7f;
        let chunk = bytes.get(at..at + size).ok_or("fsb chunk body")?;
        at += size;
        if kind == 2 && chunk.len() >= 4 {
            rate = u32::from_le_bytes(chunk[0..4].try_into().unwrap());
        }
        if kind == 1 && !chunk.is_empty() {
            // channel override is a single byte; the header bit already covers stereo
        }
        if kind == 11 {
            if chunk.len() < 4 {
                return Err("short vorbisdata".into());
            }
            crc = Some(u32::from_le_bytes(chunk[0..4].try_into().unwrap()));
        }
    }
    let crc = crc.ok_or("fsb sample has no vorbis setup crc")?;
    if rate == 0 {
        return Err(format!("unknown fsb frequency id {freq_id}"));
    }
    let fixed = if version == 0 { 64 } else { 60 };
    let start = fixed + header_size + name_size;
    Ok(Sample {
        channels,
        rate,
        crc,
        data: bytes.get(start..).ok_or("fsb audio")?,
    })
}

pub fn fsb_to_ogg(bytes: &[u8]) -> Result<Vec<u8>, String> {
    rebuild_ogg(bytes, 32)
}

/// Rebuild the Ogg stream, flushing a page after `per_page` audio packets (or sooner, when the page runs
/// out of lacing values)
fn rebuild_ogg(bytes: &[u8], per_page: usize) -> Result<Vec<u8>, String> {
    let sample = read_fsb(bytes)?;
    let setup = setup_packet(sample.crc)?;
    let flags = mode_blockflags(setup)?;
    let mut ogg = OggStream::new(
        1,
        sample.data.len() + sample.data.len() / 32 + setup.len() + 1024,
    );
    // Each header packet gets its own page; the first audio packet must start a fresh page
    ogg.push(&ident_packet(sample.channels, sample.rate), 0);
    ogg.flush(0);
    ogg.push(&comment_packet(), 0);
    ogg.flush(0);
    ogg.push(setup, 0);
    ogg.flush(0);
    let mut audio = sample.data;
    let mut prev = 0i64;
    let mut granule = 0i64;
    while audio.len() >= 2 {
        let size = u16::from_le_bytes(audio[0..2].try_into().unwrap()) as usize;
        audio = &audio[2..];
        if size == 0 {
            break;
        }
        if audio.len() < size {
            return Err("truncated vorbis packet".into());
        }
        let (packet, rest) = audio.split_at(size);
        audio = rest;
        let block = packet_blocksize(packet, &flags)?;
        granule = if prev == 0 {
            0
        } else {
            granule + (block + prev) / 4
        };
        prev = block;
        ogg.push(packet, granule);
        if ogg.packets >= per_page {
            ogg.flush(0);
        }
    }
    Ok(ogg.finish())
}

pub fn ogg_from_nodes(nodes: &[crate::unityfs::Node]) -> Result<Vec<u8>, String> {
    let fsb = nodes
        .iter()
        .find(|n| n.bytes.starts_with(b"FSB5"))
        .ok_or("bundle has no FSB5 audio")?;
    fsb_to_ogg(&fsb.bytes)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_bundled_setup_parses() {
        let count = u32::from_le_bytes(SETUPS[0..4].try_into().unwrap()) as usize;
        let mut i = 4;
        for _ in 0..count {
            let len = u32::from_le_bytes(SETUPS[i + 4..i + 8].try_into().unwrap()) as usize;
            i += 8;
            let blob = &SETUPS[i..i + len];
            let flags = mode_blockflags(blob).unwrap();
            assert!(!flags.is_empty());
            i += len;
        }
    }

    /// Parse pages back into packets, checking CRCs, sequence numbers, lacing and BOS/EOS placement
    fn read_pages(ogg: &[u8]) -> (Vec<Vec<u8>>, Vec<(u8, i64)>) {
        let mut packets = Vec::new();
        let mut pages = Vec::new();
        let mut partial: Vec<u8> = Vec::new();
        let mut at = 0;
        while at < ogg.len() {
            assert_eq!(&ogg[at..at + 4], b"OggS");
            let flags = ogg[at + 5];
            let granule = i64::from_le_bytes(ogg[at + 6..at + 14].try_into().unwrap());
            let seq = u32::from_le_bytes(ogg[at + 18..at + 22].try_into().unwrap());
            assert_eq!(seq as usize, pages.len());
            let nsegs = ogg[at + 26] as usize;
            let lacing = &ogg[at + 27..at + 27 + nsegs];
            let body_len: usize = lacing.iter().map(|n| *n as usize).sum();
            let end = at + 27 + nsegs + body_len;
            let mut page = ogg[at..end].to_vec();
            page[22..26].copy_from_slice(&[0; 4]);
            assert_eq!(ogg_crc(&page).to_le_bytes(), ogg[at + 22..at + 26]);
            assert_eq!(
                flags & 0x01 != 0,
                !partial.is_empty(),
                "continuation flag on page {seq}"
            );
            let mut body = &ogg[at + 27 + nsegs..end];
            let mut ended = false;
            for n in lacing {
                partial.extend_from_slice(&body[..*n as usize]);
                body = &body[*n as usize..];
                if *n < 255 {
                    packets.push(std::mem::take(&mut partial));
                    ended = true;
                }
            }
            assert_eq!(granule == -1, !ended, "granule on page {seq}");
            pages.push((flags, granule));
            at = end;
        }
        assert!(partial.is_empty());
        for (i, (flags, _)) in pages.iter().enumerate() {
            assert_eq!(flags & 0x02 != 0, i == 0, "BOS on page {i}");
            assert_eq!(flags & 0x04 != 0, i == pages.len() - 1, "EOS on page {i}");
        }
        (packets, pages)
    }

    #[test]
    fn pages_never_exceed_255_lacing_values_and_end_with_eos() {
        let sizes: Vec<usize> = [10, 2000, 255, 0, 510, 65535, 3, 70000, 254]
            .into_iter()
            .chain(std::iter::repeat_n(2000, 64))
            .collect();
        let input: Vec<Vec<u8>> = sizes
            .iter()
            .enumerate()
            .map(|(i, n)| vec![i as u8; *n])
            .collect();
        let mut ogg = OggStream::new(7, 0);
        for (i, packet) in input.iter().enumerate() {
            ogg.push(packet, i as i64);
            if ogg.packets >= 32 {
                ogg.flush(0);
            }
        }
        let (packets, pages) = read_pages(&ogg.finish());
        assert_eq!(packets, input);
        // The 65535- and 70000-byte packets span pages: a continued page and one where nothing ends
        assert!(pages.iter().any(|(flags, _)| flags & 0x01 != 0));
        assert!(pages.iter().any(|(_, granule)| *granule == -1));
        // 32 small packets fill one page exactly, so EOS lands on an already written page
        let mut ogg = OggStream::new(7, 0);
        for i in 0..64 {
            ogg.push(&[1, 2, 3], i);
            if ogg.packets >= 32 {
                ogg.flush(0);
            }
        }
        let (packets, pages) = read_pages(&ogg.finish());
        assert_eq!((packets.len(), pages.len()), (64, 2));
        assert_eq!(pages[1], (0x04, 63));
    }

    #[test]
    fn table_crc_matches_the_bitwise_definition() {
        let data: Vec<u8> = (0..4099u32)
            .map(|i| (i.wrapping_mul(2654435761) >> 13) as u8)
            .collect();
        let bitwise = |data: &[u8]| {
            let mut crc = 0u32;
            for &byte in data {
                crc ^= (byte as u32) << 24;
                for _ in 0..8 {
                    crc = if crc & 0x8000_0000 != 0 {
                        (crc << 1) ^ 0x04c1_1db7
                    } else {
                        crc << 1
                    };
                }
            }
            crc
        };
        for len in (0..=17).chain([4096, 4099]) {
            assert_eq!(ogg_crc(&data[..len]), bitwise(&data[..len]), "length {len}");
        }
    }

    #[test]
    fn real_packets_fill_pages_up_to_255_lacing_values() {
        let path = "/tmp/one.fsb";
        if !std::path::Path::new(path).exists() {
            return;
        }
        let fsb = std::fs::read(path).unwrap();
        let (paged, _) = read_pages(&fsb_to_ogg(&fsb).unwrap());
        let full = rebuild_ogg(&fsb, usize::MAX).unwrap();
        let (packets, pages) = read_pages(&full);
        assert_eq!(packets, paged);
        let mut at = 0;
        let mut full_pages = 0;
        while at < full.len() {
            let segments = full[at + 26] as usize;
            full_pages += usize::from(segments >= 250);
            let body: usize = full[at + 27..at + 27 + segments]
                .iter()
                .map(|n| *n as usize)
                .sum();
            at += 27 + segments + body;
        }
        assert!(
            full_pages > 10,
            "{full_pages} of {} pages near full",
            pages.len()
        );
        // Kept for decoding with an external tool
        std::fs::write(std::env::temp_dir().join("phi-fsb-full-pages.ogg"), &full).unwrap();
    }

    #[test]
    fn rebuilds_the_sample_fsb_when_present() {
        let path = "/tmp/one.fsb";
        if !std::path::Path::new(path).exists() {
            return;
        }
        let ogg = fsb_to_ogg(&std::fs::read(path).unwrap()).unwrap();
        assert!(ogg.starts_with(b"OggS"));
        let (packets, _) = read_pages(&ogg);
        assert!(packets.len() > 3);
        let out = std::env::temp_dir().join("phi-fsb-test.ogg");
        std::fs::write(&out, &ogg).unwrap();
        let status = std::process::Command::new("ffmpeg")
            .args(["-hide_banner", "-i"])
            .arg(&out)
            .args(["-f", "null", "-"])
            .output();
        if let Ok(status) = status {
            assert!(
                status.status.success(),
                "{}",
                String::from_utf8_lossy(&status.stderr)
            );
        }
    }
}
