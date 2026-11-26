use sha2::{Digest, Sha256};
use std::time::{Duration, Instant};

pub const PAIR_TIMEOUT: Duration = Duration::from_secs(60);
pub fn comparison_code(a: &str, b: &str) -> String {
    let (a, b) = if a <= b { (a, b) } else { (b, a) };
    let mut hash = Sha256::new();
    hash.update(a.as_bytes());
    hash.update(b.as_bytes());
    let digest = hash.finalize();
    format!(
        "{:06}",
        u64::from_be_bytes(digest[..8].try_into().unwrap()) % 1_000_000
    )
}
#[derive(Debug)]
pub struct Pairing {
    pub peer: String,
    pub code: String,
    pub local_accepted: bool,
    pub peer_accepted: bool,
    pub started: Instant,
}
impl Pairing {
    pub fn new(peer: String, a: &str, b: &str) -> Self {
        Self {
            peer,
            code: comparison_code(a, b),
            local_accepted: false,
            peer_accepted: false,
            started: Instant::now(),
        }
    }
    pub fn expired(&self) -> bool {
        self.started.elapsed() >= PAIR_TIMEOUT
    }
    pub fn complete(&self) -> bool {
        self.local_accepted && self.peer_accepted && !self.expired()
    }
}
#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn both_accepts_and_timeout() {
        let mut p = Pairing::new("peer".into(), "a", "b");
        p.local_accepted = true;
        assert!(!p.complete());
        p.peer_accepted = true;
        assert!(p.complete());
        p.started = Instant::now() - PAIR_TIMEOUT;
        assert!(p.expired());
        assert!(!p.complete());
    }
    #[test]
    fn symmetric_six_digits() {
        for a in ["a", "abc", "00000000", "ffffffff"] {
            let code = comparison_code(a, "other");
            assert_eq!(code, comparison_code("other", a));
            assert_eq!(code.len(), 6);
            assert!(code.bytes().all(|b| b.is_ascii_digit()));
        }
    }
}
