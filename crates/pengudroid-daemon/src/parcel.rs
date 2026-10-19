//! A minimal Android `Parcel` codec.
//!
//! A binder transaction carries a `Parcel`: a little-endian byte buffer of
//! length-prefixed primitives. A reply to `ITelephony` (or any other AIDL
//! interface) must be marshalled in exactly this format or the caller will read
//! garbage, so this module implements the subset the platform services need.
//!
//! The formats implemented here are the ones `Parcel.cpp` writes:
//!
//! * `int32`/`int64` — fixed-width little-endian;
//! * `boolean` — a 32-bit `0`/`1`;
//! * `String` — a 32-bit character count (`-1` for `null`), the UTF-16LE code
//!   units, then a `0x0000` terminator;
//! * `byte[]` — a 32-bit length then the raw bytes;
//! * a nullable `Parcelable` — a 32-bit presence flag (`0` = `null`);
//! * a `List` — a 32-bit element count then the elements.
//!
//! Only what the stub services marshal is implemented; there is no support for
//! `Bundle`, `IBinder` or file descriptors, which those services never return.

/// Writer for an Android `Parcel`.
#[derive(Debug, Default, Clone)]
pub struct ParcelWriter {
    buf: Vec<u8>,
}

impl ParcelWriter {
    pub fn new() -> ParcelWriter {
        ParcelWriter::default()
    }

    /// Finish the parcel and return its bytes.
    pub fn into_bytes(self) -> Vec<u8> {
        self.buf
    }

    /// `writeInt32`.
    pub fn write_i32(&mut self, v: i32) -> &mut Self {
        self.buf.extend_from_slice(&v.to_le_bytes());
        self
    }

    /// `writeInt64`.
    pub fn write_i64(&mut self, v: i64) -> &mut Self {
        self.buf.extend_from_slice(&v.to_le_bytes());
        self
    }

    /// `writeInt` for a `boolean` — a 32-bit `0`/`1`.
    pub fn write_bool(&mut self, v: bool) -> &mut Self {
        self.write_i32(i32::from(v))
    }

    /// `writeString16`. `None` marshals as a `null` string (length `-1`).
    pub fn write_string16(&mut self, s: Option<&str>) -> &mut Self {
        match s {
            None => {
                self.write_i32(-1);
            }
            Some(s) => {
                let utf16: Vec<u16> = s.encode_utf16().collect();
                self.write_i32(utf16.len() as i32);
                for unit in &utf16 {
                    self.buf.extend_from_slice(&unit.to_le_bytes());
                }
                // The trailing NUL is part of the on-wire format.
                self.buf.extend_from_slice(&0u16.to_le_bytes());
            }
        }
        self
    }

    /// `writeByteArray`.
    pub fn write_byte_array(&mut self, bytes: &[u8]) -> &mut Self {
        self.write_i32(bytes.len() as i32);
        self.buf.extend_from_slice(bytes);
        self
    }

    /// A `null` `Parcelable` — a 32-bit `0` presence flag.
    pub fn write_null_parcelable(&mut self) -> &mut Self {
        self.write_i32(0)
    }

    /// An empty `List` — a 32-bit `0` element count.
    pub fn write_empty_list(&mut self) -> &mut Self {
        self.write_i32(0)
    }
}

/// Reader for an Android `Parcel` (the request side of a transaction).
#[derive(Debug, Clone)]
pub struct ParcelReader<'a> {
    data: &'a [u8],
    pos: usize,
}

impl<'a> ParcelReader<'a> {
    pub fn new(data: &'a [u8]) -> ParcelReader<'a> {
        ParcelReader { data, pos: 0 }
    }

    /// Bytes consumed so far.
    pub fn position(&self) -> usize {
        self.pos
    }

    fn take(&mut self, n: usize) -> Option<&'a [u8]> {
        let end = self.pos.checked_add(n)?;
        let slice = self.data.get(self.pos..end)?;
        self.pos = end;
        Some(slice)
    }

    /// `readInt32`.
    pub fn read_i32(&mut self) -> Option<i32> {
        let b = self.take(4)?;
        Some(i32::from_le_bytes([b[0], b[1], b[2], b[3]]))
    }

    /// `readInt64`.
    pub fn read_i64(&mut self) -> Option<i64> {
        let b = self.take(8)?;
        Some(i64::from_le_bytes([b[0], b[1], b[2], b[3], b[4], b[5], b[6], b[7]]))
    }

    /// `readInt` as a `boolean`.
    pub fn read_bool(&mut self) -> Option<bool> {
        self.read_i32().map(|v| v != 0)
    }

    /// `readString16`. Returns `Some(None)` for a `null` string.
    pub fn read_string16(&mut self) -> Option<Option<String>> {
        let len = self.read_i32()?;
        if len < 0 {
            return Some(None);
        }
        let units = self.take(len as usize * 2)?;
        let mut chars: Vec<u16> = Vec::with_capacity(len as usize);
        for chunk in units.chunks_exact(2) {
            chars.push(u16::from_le_bytes([chunk[0], chunk[1]]));
        }
        // Consume the trailing NUL.
        self.take(2)?;
        Some(Some(String::from_utf16_lossy(&chars)))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn writes_little_endian_ints() {
        let mut w = ParcelWriter::new();
        w.write_i32(0x0102_0304);
        assert_eq!(w.into_bytes(), vec![0x04, 0x03, 0x02, 0x01]);
    }

    #[test]
    fn writes_a_string16_with_terminator() {
        let mut w = ParcelWriter::new();
        w.write_string16(Some("AB"));
        // length=2, 'A', 'B', NUL
        assert_eq!(
            w.into_bytes(),
            vec![2, 0, 0, 0, b'A', 0, b'B', 0, 0, 0]
        );
    }

    #[test]
    fn writes_a_null_string_as_minus_one() {
        let mut w = ParcelWriter::new();
        w.write_string16(None);
        assert_eq!(w.into_bytes(), (-1i32).to_le_bytes().to_vec());
    }

    #[test]
    fn writes_a_boolean_as_a_32_bit_int() {
        let mut w = ParcelWriter::new();
        w.write_bool(true).write_bool(false);
        assert_eq!(w.into_bytes(), vec![1, 0, 0, 0, 0, 0, 0, 0]);
    }

    #[test]
    fn round_trips_a_string() {
        let mut w = ParcelWriter::new();
        w.write_i32(7).write_string16(Some("héllo")).write_bool(true);
        let bytes = w.into_bytes();
        let mut r = ParcelReader::new(&bytes);
        assert_eq!(r.read_i32(), Some(7));
        assert_eq!(r.read_string16(), Some(Some("héllo".to_string())));
        assert_eq!(r.read_bool(), Some(true));
    }

    #[test]
    fn round_trips_a_null_string() {
        let mut w = ParcelWriter::new();
        w.write_string16(None);
        let bytes = w.into_bytes();
        let mut r = ParcelReader::new(&bytes);
        assert_eq!(r.read_string16(), Some(None));
    }

    #[test]
    fn reader_reports_truncation() {
        let mut r = ParcelReader::new(&[1, 2]);
        assert_eq!(r.read_i32(), None);
    }

    #[test]
    fn writes_a_byte_array() {
        let mut w = ParcelWriter::new();
        w.write_byte_array(&[9, 8, 7]);
        assert_eq!(w.into_bytes(), vec![3, 0, 0, 0, 9, 8, 7]);
    }
}
