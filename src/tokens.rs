//! API tokens for scripts and agents: `Authorization: Bearer ctr_<id>_<secret>`.
//!
//! Only a SHA-256 of each token is stored (file mode 0600, written atomically via rename);
//! the secret is shown once at creation. Lookup is by the public `id`, then a
//! constant-time hash comparison. The table is read on every request but written only
//! when a token is created or revoked, so it sits behind an uncontended `RwLock` holding
//! an `Arc` snapshot (the one deliberate lock in the server). `last_used` is an atomic
//! per token and is persisted with the next table write.

use std::fs;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, RwLock};

use serde_json::{Value, json};

#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Debug)]
pub enum Scope {
    Read,
    Operate,
    Admin,
}

impl Scope {
    pub fn parse(s: &str) -> Option<Scope> {
        match s {
            "read" => Some(Scope::Read),
            "operate" => Some(Scope::Operate),
            "admin" => Some(Scope::Admin),
            _ => None,
        }
    }

    pub fn as_str(self) -> &'static str {
        match self {
            Scope::Read => "read",
            Scope::Operate => "operate",
            Scope::Admin => "admin",
        }
    }
}

pub struct Token {
    pub id: String,
    pub name: String,
    pub scope: Scope,
    hash: String,
    pub created: u64,
    pub expires: Option<u64>,
    pub last_used: AtomicU64,
}

impl Token {
    fn to_json(&self, with_hash: bool) -> Value {
        let mut v = json!({
            "id": self.id,
            "name": self.name,
            "scope": self.scope.as_str(),
            "created": self.created,
            "expires": self.expires,
            "last_used": match self.last_used.load(Ordering::Relaxed) { 0 => None, t => Some(t) },
        });
        if with_hash {
            v["sha256"] = json!(self.hash);
        }
        v
    }

    fn from_json(v: &Value) -> Option<Token> {
        Some(Token {
            id: v.get("id")?.as_str()?.to_string(),
            name: v.get("name")?.as_str()?.to_string(),
            scope: Scope::parse(v.get("scope")?.as_str()?)?,
            hash: v.get("sha256")?.as_str()?.to_string(),
            created: v.get("created")?.as_u64()?,
            expires: v.get("expires").and_then(Value::as_u64),
            last_used: AtomicU64::new(v.get("last_used").and_then(Value::as_u64).unwrap_or(0)),
        })
    }
}

pub struct Store {
    path: Option<PathBuf>,
    table: RwLock<Arc<Vec<Token>>>,
}

pub enum Check {
    Valid { name: String, scope: Scope },
    Invalid,
}

impl Store {
    /// Tokens live next to the audit log; `None` disables tokens (no state directory).
    pub fn open(path: Option<PathBuf>) -> Store {
        let tokens = path
            .as_ref()
            .and_then(|p| fs::read_to_string(p).ok())
            .and_then(|s| serde_json::from_str::<Value>(&s).ok())
            .and_then(|v| v.get("tokens")?.as_array().map(|a| a.iter().filter_map(Token::from_json).collect()))
            .unwrap_or_default();
        Store { path, table: RwLock::new(Arc::new(tokens)) }
    }

    pub fn path(&self) -> Option<&Path> {
        self.path.as_deref()
    }

    fn snapshot(&self) -> Arc<Vec<Token>> {
        self.table.read().map(|t| t.clone()).unwrap_or_default()
    }

    pub fn len(&self) -> usize {
        self.snapshot().len()
    }

    pub fn check(&self, presented: &str, now: u64) -> Check {
        let Some((id, _)) = presented.strip_prefix("ctr_").and_then(|r| r.split_once('_')) else {
            return Check::Invalid;
        };
        let table = self.snapshot();
        let Some(t) = table.iter().find(|t| t.id == id) else { return Check::Invalid };
        if !crate::http::ct_eq(hex(&sha256(presented.as_bytes())).as_bytes(), t.hash.as_bytes()) {
            return Check::Invalid;
        }
        if t.expires.is_some_and(|e| now >= e) {
            return Check::Invalid;
        }
        t.last_used.store(now, Ordering::Relaxed);
        Check::Valid { name: t.name.clone(), scope: t.scope }
    }

    pub fn list(&self) -> Vec<Value> {
        self.snapshot().iter().map(|t| t.to_json(false)).collect()
    }

    /// Returns the public record and the secret token (shown once).
    pub fn create(&self, name: &str, scope: Scope, expires: Option<u64>, now: u64) -> Result<(Value, String), String> {
        if self.path.is_none() {
            return Err("tokens need a state directory: set --audit-file to a path".into());
        }
        let id = hex(&random_bytes(4));
        let secret = format!("ctr_{id}_{}", base64url(&random_bytes(32)));
        let token = Token {
            id,
            name: name.to_string(),
            scope,
            hash: hex(&sha256(secret.as_bytes())),
            created: now,
            expires,
            last_used: AtomicU64::new(0),
        };
        let record = token.to_json(false);
        self.update(|ts| ts.push(token))?;
        Ok((record, secret))
    }

    pub fn revoke(&self, id: &str) -> Result<bool, String> {
        let mut found = false;
        self.update(|ts| {
            let before = ts.len();
            ts.retain(|t| t.id != id);
            found = ts.len() != before;
        })?;
        Ok(found)
    }

    fn update(&self, f: impl FnOnce(&mut Vec<Token>)) -> Result<(), String> {
        let mut guard = self.table.write().map_err(|_| "token table poisoned".to_string())?;
        let mut next: Vec<Token> = guard
            .iter()
            .map(|t| Token {
                id: t.id.clone(),
                name: t.name.clone(),
                scope: t.scope,
                hash: t.hash.clone(),
                created: t.created,
                expires: t.expires,
                last_used: AtomicU64::new(t.last_used.load(Ordering::Relaxed)),
            })
            .collect();
        f(&mut next);
        if let Some(path) = &self.path {
            let body = json!({ "tokens": next.iter().map(|t| t.to_json(true)).collect::<Vec<_>>() });
            write_private(path, body.to_string().as_bytes()).map_err(|e| format!("cannot save {}: {e}", path.display()))?;
        }
        *guard = Arc::new(next);
        Ok(())
    }
}

/// Write via a temp file + rename so readers never see a half-written file; 0600 on unix.
fn write_private(path: &Path, data: &[u8]) -> std::io::Result<()> {
    let tmp = path.with_extension("tmp");
    let mut opts = fs::OpenOptions::new();
    opts.write(true).create(true).truncate(true);
    #[cfg(unix)]
    std::os::unix::fs::OpenOptionsExt::mode(&mut opts, 0o600);
    std::io::Write::write_all(&mut opts.open(&tmp)?, data)?;
    fs::rename(tmp, path)
}

pub fn random_bytes(n: usize) -> Vec<u8> {
    #[cfg(unix)]
    {
        use std::io::Read;
        let mut buf = vec![0u8; n];
        if fs::File::open("/dev/urandom").and_then(|mut f| f.read_exact(&mut buf)).is_ok() {
            return buf;
        }
    }
    // Fallback (Windows): std seeds SipHash keys from the OS RNG; hash distinct inputs.
    use std::hash::{BuildHasher, Hasher};
    let mut out = Vec::with_capacity(n + 8);
    let mut i = 0u64;
    while out.len() < n {
        let mut h = std::collections::hash_map::RandomState::new().build_hasher();
        h.write_u64(i);
        h.write_u128(std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map_or(0, |d| d.as_nanos()));
        out.extend_from_slice(&h.finish().to_le_bytes());
        i += 1;
    }
    out.truncate(n);
    out
}

pub fn hex(b: &[u8]) -> String {
    b.iter().map(|x| format!("{x:02x}")).collect()
}

fn base64url(b: &[u8]) -> String {
    crate::http::base64(b).trim_end_matches('=').replace('+', "-").replace('/', "_")
}

/// SHA-256 (FIPS 180-4). Small and dependency-free; only used for token hashes.
pub fn sha256(data: &[u8]) -> [u8; 32] {
    const K: [u32; 64] = [
        0x428a2f98, 0x71374491, 0xb5c0fbcf, 0xe9b5dba5, 0x3956c25b, 0x59f111f1, 0x923f82a4, 0xab1c5ed5,
        0xd807aa98, 0x12835b01, 0x243185be, 0x550c7dc3, 0x72be5d74, 0x80deb1fe, 0x9bdc06a7, 0xc19bf174,
        0xe49b69c1, 0xefbe4786, 0x0fc19dc6, 0x240ca1cc, 0x2de92c6f, 0x4a7484aa, 0x5cb0a9dc, 0x76f988da,
        0x983e5152, 0xa831c66d, 0xb00327c8, 0xbf597fc7, 0xc6e00bf3, 0xd5a79147, 0x06ca6351, 0x14292967,
        0x27b70a85, 0x2e1b2138, 0x4d2c6dfc, 0x53380d13, 0x650a7354, 0x766a0abb, 0x81c2c92e, 0x92722c85,
        0xa2bfe8a1, 0xa81a664b, 0xc24b8b70, 0xc76c51a3, 0xd192e819, 0xd6990624, 0xf40e3585, 0x106aa070,
        0x19a4c116, 0x1e376c08, 0x2748774c, 0x34b0bcb5, 0x391c0cb3, 0x4ed8aa4a, 0x5b9cca4f, 0x682e6ff3,
        0x748f82ee, 0x78a5636f, 0x84c87814, 0x8cc70208, 0x90befffa, 0xa4506ceb, 0xbef9a3f7, 0xc67178f2,
    ];
    let mut h: [u32; 8] =
        [0x6a09e667, 0xbb67ae85, 0x3c6ef372, 0xa54ff53a, 0x510e527f, 0x9b05688c, 0x1f83d9ab, 0x5be0cd19];
    let mut msg = data.to_vec();
    msg.push(0x80);
    while msg.len() % 64 != 56 {
        msg.push(0);
    }
    msg.extend_from_slice(&((data.len() as u64) * 8).to_be_bytes());
    for block in msg.chunks(64) {
        let mut w = [0u32; 64];
        for (i, c) in block.chunks(4).enumerate() {
            w[i] = u32::from_be_bytes([c[0], c[1], c[2], c[3]]);
        }
        for i in 16..64 {
            let s0 = w[i - 15].rotate_right(7) ^ w[i - 15].rotate_right(18) ^ (w[i - 15] >> 3);
            let s1 = w[i - 2].rotate_right(17) ^ w[i - 2].rotate_right(19) ^ (w[i - 2] >> 10);
            w[i] = w[i - 16].wrapping_add(s0).wrapping_add(w[i - 7]).wrapping_add(s1);
        }
        let [mut a, mut b, mut c, mut d, mut e, mut f, mut g, mut hh] = h;
        for i in 0..64 {
            let s1 = e.rotate_right(6) ^ e.rotate_right(11) ^ e.rotate_right(25);
            let ch = (e & f) ^ (!e & g);
            let t1 = hh.wrapping_add(s1).wrapping_add(ch).wrapping_add(K[i]).wrapping_add(w[i]);
            let s0 = a.rotate_right(2) ^ a.rotate_right(13) ^ a.rotate_right(22);
            let maj = (a & b) ^ (a & c) ^ (b & c);
            let t2 = s0.wrapping_add(maj);
            hh = g;
            g = f;
            f = e;
            e = d.wrapping_add(t1);
            d = c;
            c = b;
            b = a;
            a = t1.wrapping_add(t2);
        }
        for (x, y) in h.iter_mut().zip([a, b, c, d, e, f, g, hh]) {
            *x = x.wrapping_add(y);
        }
    }
    let mut out = [0u8; 32];
    for (i, x) in h.iter().enumerate() {
        out[i * 4..i * 4 + 4].copy_from_slice(&x.to_be_bytes());
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sha256_known_vectors() {
        assert_eq!(hex(&sha256(b"")), "e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855");
        assert_eq!(hex(&sha256(b"abc")), "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad");
        let long = b"abcdbcdecdefdefgefghfghighijhijkijkljklmklmnlmnomnopnopq";
        assert_eq!(hex(&sha256(long)), "248d6a61d20638b8e5c026930c3e6039a33ce45964ff2167f6ecedd419db06c1");
    }

    #[test]
    fn create_check_revoke_persist() {
        let dir = std::env::temp_dir().join(format!("contraire-tokens-{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();
        let path = dir.join("tokens.json");
        let store = Store::open(Some(path.clone()));
        let (rec, secret) = store.create("agent", Scope::Operate, None, 100).unwrap();
        assert!(secret.starts_with("ctr_"));
        assert!(matches!(store.check(&secret, 200), Check::Valid { scope: Scope::Operate, .. }));
        assert!(matches!(store.check(&format!("{secret}x"), 200), Check::Invalid));
        assert!(matches!(store.check("ctr_nope_nope", 200), Check::Invalid));
        // Survives a restart, and the secret itself is never written to disk.
        assert!(!fs::read_to_string(&path).unwrap().contains(&secret));
        let reopened = Store::open(Some(path.clone()));
        assert!(matches!(reopened.check(&secret, 300), Check::Valid { .. }));
        assert!(reopened.revoke(rec["id"].as_str().unwrap()).unwrap());
        assert!(matches!(reopened.check(&secret, 400), Check::Invalid));
        let _ = fs::remove_dir_all(dir);
    }

    #[test]
    fn expired_tokens_are_rejected() {
        let dir = std::env::temp_dir().join(format!("contraire-tokens-exp-{}", std::process::id()));
        fs::create_dir_all(&dir).unwrap();
        let store = Store::open(Some(dir.join("t.json")));
        let (_, secret) = store.create("short", Scope::Read, Some(1000), 0).unwrap();
        assert!(matches!(store.check(&secret, 999), Check::Valid { .. }));
        assert!(matches!(store.check(&secret, 1000), Check::Invalid));
        let _ = fs::remove_dir_all(dir);
    }
}
