//! AMF0 encoding/decoding.
//!
//! Port of `ngx_rtmp_amf.c`. The C version reads into caller-provided
//! descriptor tables; here we decode into a dynamic [`Amf0`] tree and let
//! callers pick out the fields they need, which is simpler and just as fast
//! for the small command messages RTMP uses.
//!
//! Compatibility quirks preserved from the C implementation:
//! * objects / ECMA arrays that hit end-of-input before the end marker are
//!   accepted (Envivio encoders send unterminated arrays);
//! * OBJECT and ECMA_ARRAY are treated as interchangeable by the accessors.

use bytes::{BufMut, BytesMut};

pub const NUMBER: u8 = 0x00;
pub const BOOLEAN: u8 = 0x01;
pub const STRING: u8 = 0x02;
pub const OBJECT: u8 = 0x03;
pub const NULL: u8 = 0x05;
pub const UNDEFINED: u8 = 0x06;
pub const ECMA_ARRAY: u8 = 0x08;
pub const OBJECT_END: u8 = 0x09;
pub const STRICT_ARRAY: u8 = 0x0a;
pub const DATE: u8 = 0x0b;
pub const LONG_STRING: u8 = 0x0c;

#[derive(Debug, Clone, PartialEq)]
pub enum Amf0 {
    Number(f64),
    Boolean(bool),
    String(String),
    Object(Vec<(String, Amf0)>),
    Null,
    Undefined,
    EcmaArray(Vec<(String, Amf0)>),
    StrictArray(Vec<Amf0>),
    Date { millis: f64, tz: i16 },
}

#[derive(Debug, thiserror::Error, PartialEq)]
pub enum AmfError {
    #[error("unexpected end of AMF data")]
    Eof,
    #[error("unsupported AMF0 type marker 0x{0:02x}")]
    UnknownType(u8),
    #[error("AMF object not terminated by end marker")]
    BadObjectEnd,
}

impl Amf0 {
    pub fn str(s: impl Into<String>) -> Self {
        Amf0::String(s.into())
    }

    pub fn object<K: Into<String>>(props: impl IntoIterator<Item = (K, Amf0)>) -> Self {
        Amf0::Object(props.into_iter().map(|(k, v)| (k.into(), v)).collect())
    }

    /// Property lookup on OBJECT or ECMA_ARRAY values.
    pub fn get(&self, key: &str) -> Option<&Amf0> {
        match self {
            Amf0::Object(p) | Amf0::EcmaArray(p) => {
                p.iter().find(|(k, _)| k == key).map(|(_, v)| v)
            }
            _ => None,
        }
    }

    pub fn as_str(&self) -> Option<&str> {
        match self {
            Amf0::String(s) => Some(s),
            _ => None,
        }
    }

    pub fn as_f64(&self) -> Option<f64> {
        match self {
            Amf0::Number(n) => Some(*n),
            _ => None,
        }
    }

    pub fn as_bool(&self) -> Option<bool> {
        match self {
            Amf0::Boolean(b) => Some(*b),
            Amf0::Number(n) => Some(*n != 0.0),
            _ => None,
        }
    }

    pub fn encode(&self, out: &mut BytesMut) {
        match self {
            Amf0::Number(n) => {
                out.put_u8(NUMBER);
                out.put_f64(*n);
            }
            Amf0::Boolean(b) => {
                out.put_u8(BOOLEAN);
                out.put_u8(*b as u8);
            }
            Amf0::String(s) => {
                if s.len() > u16::MAX as usize {
                    out.put_u8(LONG_STRING);
                    out.put_u32(s.len() as u32);
                } else {
                    out.put_u8(STRING);
                    out.put_u16(s.len() as u16);
                }
                out.put_slice(s.as_bytes());
            }
            Amf0::Object(props) => {
                out.put_u8(OBJECT);
                encode_props(props, out);
            }
            Amf0::Null => out.put_u8(NULL),
            Amf0::Undefined => out.put_u8(UNDEFINED),
            Amf0::EcmaArray(props) => {
                out.put_u8(ECMA_ARRAY);
                out.put_u32(props.len() as u32);
                encode_props(props, out);
            }
            Amf0::StrictArray(items) => {
                out.put_u8(STRICT_ARRAY);
                out.put_u32(items.len() as u32);
                for v in items {
                    v.encode(out);
                }
            }
            Amf0::Date { millis, tz } => {
                out.put_u8(DATE);
                out.put_f64(*millis);
                out.put_i16(*tz);
            }
        }
    }
}

fn encode_props(props: &[(String, Amf0)], out: &mut BytesMut) {
    for (k, v) in props {
        let k = &k.as_bytes()[..k.len().min(u16::MAX as usize)];
        out.put_u16(k.len() as u16);
        out.put_slice(k);
        v.encode(out);
    }
    out.put_slice(&[0, 0, OBJECT_END]);
}

/// Encode a sequence of values back to back (an RTMP command body).
pub fn encode_all(values: &[Amf0]) -> BytesMut {
    let mut out = BytesMut::with_capacity(128);
    for v in values {
        v.encode(&mut out);
    }
    out
}

/// Decode every value in `data`.
pub fn decode_all(data: &[u8]) -> Result<Vec<Amf0>, AmfError> {
    let mut r = Reader { data, pos: 0 };
    let mut out = Vec::new();
    while r.pos < data.len() {
        out.push(r.value()?);
    }
    Ok(out)
}

struct Reader<'a> {
    data: &'a [u8],
    pos: usize,
}

impl<'a> Reader<'a> {
    fn take(&mut self, n: usize) -> Result<&'a [u8], AmfError> {
        if self.data.len() - self.pos < n {
            return Err(AmfError::Eof);
        }
        let s = &self.data[self.pos..self.pos + n];
        self.pos += n;
        Ok(s)
    }

    fn u8(&mut self) -> Result<u8, AmfError> {
        Ok(self.take(1)?[0])
    }
    fn u16(&mut self) -> Result<u16, AmfError> {
        Ok(u16::from_be_bytes(self.take(2)?.try_into().unwrap()))
    }
    fn u32(&mut self) -> Result<u32, AmfError> {
        Ok(u32::from_be_bytes(self.take(4)?.try_into().unwrap()))
    }
    fn f64(&mut self) -> Result<f64, AmfError> {
        Ok(f64::from_be_bytes(self.take(8)?.try_into().unwrap()))
    }
    fn string(&mut self, len: usize) -> Result<String, AmfError> {
        Ok(String::from_utf8_lossy(self.take(len)?).into_owned())
    }

    fn value(&mut self) -> Result<Amf0, AmfError> {
        let marker = self.u8()?;
        Ok(match marker {
            NUMBER => Amf0::Number(self.f64()?),
            BOOLEAN => Amf0::Boolean(self.u8()? != 0),
            STRING => {
                let n = self.u16()? as usize;
                Amf0::String(self.string(n)?)
            }
            LONG_STRING => {
                let n = self.u32()? as usize;
                Amf0::String(self.string(n)?)
            }
            OBJECT => Amf0::Object(self.props()?),
            NULL => Amf0::Null,
            UNDEFINED => Amf0::Undefined,
            ECMA_ARRAY => {
                let _count = self.u32()?; // advisory only, like the C code
                Amf0::EcmaArray(self.props()?)
            }
            STRICT_ARRAY => {
                let n = self.u32()? as usize;
                let mut items = Vec::with_capacity(n.min(1024));
                for _ in 0..n {
                    items.push(self.value()?);
                }
                Amf0::StrictArray(items)
            }
            DATE => Amf0::Date { millis: self.f64()?, tz: self.u16()? as i16 },
            other => return Err(AmfError::UnknownType(other)),
        })
    }

    fn props(&mut self) -> Result<Vec<(String, Amf0)>, AmfError> {
        let mut props = Vec::new();
        loop {
            // Envivio sends unfinalized arrays: EOF where a key should be is OK.
            if self.data.len() - self.pos < 2 {
                self.pos = self.data.len();
                return Ok(props);
            }
            let klen = self.u16()? as usize;
            if klen == 0 {
                break;
            }
            let key = self.string(klen)?;
            let v = self.value()?;
            props.push((key, v));
        }
        match self.u8() {
            Ok(OBJECT_END) => Ok(props),
            _ => Err(AmfError::BadObjectEnd),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn roundtrip() {
        let vals = vec![
            Amf0::str("connect"),
            Amf0::Number(1.0),
            Amf0::object([
                ("app", Amf0::str("live")),
                ("objectEncoding", Amf0::Number(0.0)),
                ("fpad", Amf0::Boolean(false)),
            ]),
            Amf0::Null,
            Amf0::EcmaArray(vec![("width".into(), Amf0::Number(1920.0))]),
            Amf0::StrictArray(vec![Amf0::Number(1.0), Amf0::Undefined]),
        ];
        let enc = encode_all(&vals);
        assert_eq!(decode_all(&enc).unwrap(), vals);
    }

    #[test]
    fn known_bytes() {
        let enc = encode_all(&[Amf0::str("_result"), Amf0::Number(1.0)]);
        assert_eq!(
            &enc[..],
            b"\x02\x00\x07_result\x00\x3f\xf0\x00\x00\x00\x00\x00\x00"
        );
    }

    #[test]
    fn unterminated_object_is_accepted() {
        let mut enc = encode_all(&[Amf0::object([("a", Amf0::Number(2.0))])]);
        enc.truncate(enc.len() - 3); // drop 00 00 09
        let v = decode_all(&enc).unwrap();
        assert_eq!(v[0].get("a"), Some(&Amf0::Number(2.0)));
    }

    #[test]
    fn truncated_value_is_error() {
        assert_eq!(decode_all(b"\x00\x3f\xf0"), Err(AmfError::Eof));
    }
}
