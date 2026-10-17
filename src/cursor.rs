pub struct Cursor<'a> {
    data: &'a [u8],
    pos: usize,
}

impl<'a> Cursor<'a> {
    pub fn new(data: &'a [u8]) -> Self {
        Self { data, pos: 0 }
    }

    pub fn pos(&self) -> usize {
        self.pos
    }

    pub fn remaining(&self) -> usize {
        self.data.len().saturating_sub(self.pos)
    }

    pub fn seek(&mut self, pos: usize) -> Result<(), String> {
        if pos > self.data.len() {
            return Err("seek past end".into());
        }
        self.pos = pos;
        Ok(())
    }

    pub fn u8(&mut self) -> Result<u8, String> {
        let b = *self.data.get(self.pos).ok_or("truncated")?;
        self.pos += 1;
        Ok(b)
    }

    pub fn bool(&mut self) -> Result<bool, String> {
        Ok(self.u8()? != 0)
    }

    pub fn i32(&mut self) -> Result<i32, String> {
        let b = self.bytes(4)?;
        Ok(i32::from_le_bytes(b.try_into().unwrap()))
    }

    pub fn u32(&mut self) -> Result<u32, String> {
        Ok(self.i32()? as u32)
    }

    pub fn u64(&mut self) -> Result<u64, String> {
        let b = self.bytes(8)?;
        Ok(u64::from_le_bytes(b.try_into().unwrap()))
    }

    pub fn i64(&mut self) -> Result<i64, String> {
        Ok(self.u64()? as i64)
    }

    pub fn f32(&mut self) -> Result<f32, String> {
        Ok(f32::from_le_bytes(self.u32()?.to_le_bytes()))
    }

    pub fn bytes(&mut self, n: usize) -> Result<&'a [u8], String> {
        let end = self.pos.checked_add(n).ok_or("truncated")?;
        let slice = self.data.get(self.pos..end).ok_or("truncated")?;
        self.pos = end;
        Ok(slice)
    }

    pub fn align(&mut self, n: usize) {
        let mask = n - 1;
        self.pos = (self.pos + mask) & !mask;
    }

    pub fn string(&mut self) -> Result<String, String> {
        self.align(4);
        let len = self.i32()?;
        if !(0..=1_000_000).contains(&len) {
            return Err(format!("string length {len}"));
        }
        let bytes = self.bytes(len as usize)?;
        self.align(4);
        Ok(String::from_utf8_lossy(bytes).into_owned())
    }
}
