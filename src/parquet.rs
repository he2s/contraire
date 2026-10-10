//! A minimal Parquet writer for exports: one row group, one PLAIN-encoded, uncompressed data
//! page per column, all columns REQUIRED. Enough for DuckDB, pandas/pyarrow and polars to
//! read, in about 200 lines instead of the `parquet` crate (hundreds of KB and a long build).
//!
//! Columns are inferred from JSON rows: strings → BYTE_ARRAY (UTF8), integers → INT64,
//! other numbers → DOUBLE. Missing values become "" or NaN, so no definition levels are needed.

use serde_json::Value;

#[derive(Clone, Copy, PartialEq)]
enum Kind {
    Int,
    Double,
    Text,
}

/// Thrift compact protocol, just the parts the Parquet footer and page headers use.
struct Thrift {
    out: Vec<u8>,
    last: Vec<i16>,
}

const I32: u8 = 5;
const I64: u8 = 6;
const BINARY: u8 = 8;
const LIST: u8 = 9;
const STRUCT: u8 = 12;

impl Thrift {
    fn new() -> Thrift {
        Thrift { out: Vec::new(), last: vec![0] }
    }
    fn varint(&mut self, mut v: u64) {
        while v >= 0x80 {
            self.out.push(v as u8 | 0x80);
            v >>= 7;
        }
        self.out.push(v as u8);
    }
    fn zigzag(&mut self, v: i64) {
        self.varint(((v << 1) ^ (v >> 63)) as u64);
    }
    fn field(&mut self, id: i16, ty: u8) {
        let delta = id - self.last.last().copied().unwrap_or(0);
        if (1..=15).contains(&delta) {
            self.out.push((delta as u8) << 4 | ty);
        } else {
            self.out.push(ty);
            self.zigzag(id as i64);
        }
        if let Some(last) = self.last.last_mut() {
            *last = id;
        }
    }
    fn i32(&mut self, id: i16, v: i32) {
        self.field(id, I32);
        self.zigzag(v as i64);
    }
    fn i64(&mut self, id: i16, v: i64) {
        self.field(id, I64);
        self.zigzag(v);
    }
    fn bytes(&mut self, v: &[u8]) {
        self.varint(v.len() as u64);
        self.out.extend_from_slice(v);
    }
    fn string(&mut self, id: i16, v: &str) {
        self.field(id, BINARY);
        self.bytes(v.as_bytes());
    }
    fn list(&mut self, id: i16, elem: u8, n: usize) {
        self.field(id, LIST);
        if n < 15 {
            self.out.push((n as u8) << 4 | elem);
        } else {
            self.out.push(0xF0 | elem);
            self.varint(n as u64);
        }
    }
    /// Starts a struct, as a field (`Some(id)`) or as a list element (`None`).
    fn begin(&mut self, id: Option<i16>) {
        if let Some(id) = id {
            self.field(id, STRUCT);
        }
        self.last.push(0);
    }
    fn end(&mut self) {
        self.out.push(0);
        self.last.pop();
    }
}

/// Encodes `rows` (JSON objects with the same keys) as a Parquet file. `None` without rows.
pub fn write(rows: &[Value]) -> Option<Vec<u8>> {
    let first = rows.first()?.as_object()?;
    let cols: Vec<(&str, Kind)> = first
        .keys()
        .map(|k| {
            let mut kind = None;
            for r in rows {
                match (&r[k], kind) {
                    (Value::String(_), _) => kind = Some(Kind::Text),
                    (Value::Number(n), None | Some(Kind::Int)) if n.is_i64() || n.is_u64() => kind = Some(Kind::Int),
                    (Value::Number(_), Some(Kind::Text)) => {}
                    (Value::Number(_), _) => kind = Some(Kind::Double),
                    _ => {}
                }
                if kind == Some(Kind::Text) {
                    break;
                }
            }
            (k.as_str(), kind.unwrap_or(Kind::Double))
        })
        .collect();

    let mut file = b"PAR1".to_vec();
    let mut chunks = Vec::new(); // (offset, size incl. header)
    for (name, kind) in &cols {
        let mut data = Vec::with_capacity(rows.len() * 8);
        for r in rows {
            let v = &r[*name];
            match kind {
                Kind::Int => data.extend_from_slice(&v.as_i64().or_else(|| v.as_u64().map(|u| u as i64)).unwrap_or(0).to_le_bytes()),
                Kind::Double => data.extend_from_slice(&v.as_f64().unwrap_or(f64::NAN).to_le_bytes()),
                Kind::Text => {
                    let s = match v {
                        Value::String(s) => s.clone(),
                        Value::Null => String::new(),
                        other => other.to_string(),
                    };
                    data.extend_from_slice(&(s.len() as u32).to_le_bytes());
                    data.extend_from_slice(s.as_bytes());
                }
            }
        }
        let mut h = Thrift::new();
        h.i32(1, 0); // DATA_PAGE
        h.i32(2, data.len() as i32);
        h.i32(3, data.len() as i32);
        h.begin(Some(5));
        h.i32(1, rows.len() as i32);
        h.i32(2, 0); // PLAIN
        h.i32(3, 3); // RLE (no levels: all columns required)
        h.i32(4, 3);
        h.end();
        h.out.push(0);
        let offset = file.len();
        file.extend_from_slice(&h.out);
        file.extend_from_slice(&data);
        chunks.push((offset, h.out.len() + data.len()));
    }

    let physical = |k: Kind| match k {
        Kind::Int => 2,    // INT64
        Kind::Double => 5, // DOUBLE
        Kind::Text => 6,   // BYTE_ARRAY
    };
    let mut m = Thrift::new();
    m.i32(1, 1); // version
    m.list(2, STRUCT, cols.len() + 1);
    m.begin(None);
    m.string(4, "schema");
    m.i32(5, cols.len() as i32);
    m.end();
    for (name, kind) in &cols {
        m.begin(None);
        m.i32(1, physical(*kind));
        m.i32(3, 0); // REQUIRED
        m.string(4, name);
        if *kind == Kind::Text {
            m.i32(6, 0); // converted type UTF8
        }
        m.end();
    }
    m.i64(3, rows.len() as i64);
    m.list(4, STRUCT, 1);
    m.begin(None);
    m.list(1, STRUCT, cols.len());
    for ((name, kind), (offset, size)) in cols.iter().zip(&chunks) {
        m.begin(None);
        m.i64(2, *offset as i64);
        m.begin(Some(3));
        m.i32(1, physical(*kind));
        m.list(2, I32, 1);
        m.zigzag(0); // PLAIN
        m.list(3, BINARY, 1);
        m.bytes(name.as_bytes());
        m.i32(4, 0); // UNCOMPRESSED
        m.i64(5, rows.len() as i64);
        m.i64(6, *size as i64);
        m.i64(7, *size as i64);
        m.i64(9, *offset as i64);
        m.end();
        m.end();
    }
    m.i64(2, chunks.iter().map(|c| c.1 as i64).sum());
    m.i64(3, rows.len() as i64);
    m.end();
    m.string(6, concat!("contraire ", env!("CARGO_PKG_VERSION")));
    m.out.push(0);
    file.extend_from_slice(&m.out);
    file.extend_from_slice(&(m.out.len() as u32).to_le_bytes());
    file.extend_from_slice(b"PAR1");
    Some(file)
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn writes_a_framed_file_with_inferred_columns() {
        let rows = vec![json!({"minute": 60, "p95_ms": 1.5, "service": "cart"}), json!({"minute": 120, "p95_ms": null, "service": "ad"})];
        let f = write(&rows).unwrap();
        assert!(f.starts_with(b"PAR1") && f.ends_with(b"PAR1"));
        let footer = u32::from_le_bytes(f[f.len() - 8..f.len() - 4].try_into().unwrap()) as usize;
        assert!(footer > 0 && footer < f.len());
        // Data pages: 2 × i64, 2 × f64, then "cart" and "ad" length-prefixed.
        let text = String::from_utf8_lossy(&f);
        assert!(text.contains("cart") && text.contains("service") && text.contains("p95_ms"));
        assert!(write(&[]).is_none());
    }

    #[test]
    fn thrift_compact_basics() {
        let mut t = Thrift::new();
        t.i32(1, -1);
        t.i64(17, 300); // long delta: type byte, then zigzag id
        assert_eq!(t.out, [0x15, 0x01, 0x06, 0x22, 0xd8, 0x04]);
    }
}
