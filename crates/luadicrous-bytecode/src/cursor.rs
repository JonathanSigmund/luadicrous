use crate::ParseError;

#[derive(Debug, Clone)]
pub(crate) struct Cursor<'a> {
    bytes: &'a [u8],
    offset: usize,
}

impl<'a> Cursor<'a> {
    pub(crate) const fn new(bytes: &'a [u8]) -> Self {
        Self { bytes, offset: 0 }
    }

    pub(crate) const fn position(&self) -> usize {
        self.offset
    }

    pub(crate) fn remaining(&self) -> &'a [u8] {
        &self.bytes[self.offset..]
    }

    pub(crate) fn read_u8(&mut self) -> Result<u8, ParseError> {
        let offset = self.offset;
        let value = *self.bytes.get(offset).ok_or(ParseError::UnexpectedEof {
            offset,
            needed: 1,
            remaining: 0,
        })?;
        self.offset += 1;
        Ok(value)
    }

    pub(crate) fn read_exact(&mut self, length: usize) -> Result<&'a [u8], ParseError> {
        let offset = self.offset;
        let end = offset
            .checked_add(length)
            .ok_or(ParseError::LengthOverflow { offset, length })?;
        let bytes = self
            .bytes
            .get(offset..end)
            .ok_or(ParseError::UnexpectedEof {
                offset,
                needed: length,
                remaining: self.bytes.len().saturating_sub(offset),
            })?;
        self.offset = end;
        Ok(bytes)
    }

    pub(crate) fn read_u32(&mut self) -> Result<u32, ParseError> {
        let bytes: [u8; 4] = self
            .read_exact(4)?
            .try_into()
            .expect("read_exact returned four bytes");
        Ok(u32::from_le_bytes(bytes))
    }

    pub(crate) fn read_i32(&mut self) -> Result<i32, ParseError> {
        let bytes: [u8; 4] = self
            .read_exact(4)?
            .try_into()
            .expect("read_exact returned four bytes");
        Ok(i32::from_le_bytes(bytes))
    }

    pub(crate) fn read_u64(&mut self) -> Result<u64, ParseError> {
        let bytes: [u8; 8] = self
            .read_exact(8)?
            .try_into()
            .expect("read_exact returned eight bytes");
        Ok(u64::from_le_bytes(bytes))
    }

    pub(crate) fn read_var_u32(&mut self) -> Result<u32, ParseError> {
        let start = self.offset;
        let mut value = 0_u32;
        for shift in (0..35).step_by(7) {
            let byte = self.read_u8()?;
            if shift == 28 && byte & 0xf0 != 0 {
                return Err(ParseError::VarintOverflow { offset: start });
            }
            value |= u32::from(byte & 0x7f) << shift;
            if byte & 0x80 == 0 {
                return Ok(value);
            }
        }
        Err(ParseError::VarintOverflow { offset: start })
    }

    pub(crate) fn read_var_u64(&mut self) -> Result<u64, ParseError> {
        let start = self.offset;
        let mut value = 0_u64;
        for shift in (0..70).step_by(7) {
            let byte = self.read_u8()?;
            if shift == 63 && byte & 0xfe != 0 {
                return Err(ParseError::Varint64Overflow { offset: start });
            }
            value |= u64::from(byte & 0x7f) << shift;
            if byte & 0x80 == 0 {
                return Ok(value);
            }
        }
        Err(ParseError::Varint64Overflow { offset: start })
    }
}
