//! Shared utility functions.

use tokio::io::AsyncWriteExt;

/// Write a single line of data (with trailing newline) and flush.
///
/// Used by stdio transport, bridge, and backend RPC to avoid
/// repeating the write_all + newline + flush pattern.
pub async fn write_line(w: &mut (impl AsyncWriteExt + Unpin), data: &[u8]) -> anyhow::Result<()> {
    w.write_all(data).await?;
    w.write_all(b"\n").await?;
    w.flush().await?;
    Ok(())
}

/// FNV-1a 64-bit hash — deterministic across Rust versions (unlike `DefaultHasher`).
///
/// Used for config-change detection, credential scoping, and random seeding.
pub fn fnv1a(s: &str) -> u64 {
    let mut h: u64 = 0xcbf29ce484222325;
    for b in s.bytes() {
        h ^= b as u64;
        h = h.wrapping_mul(0x100000001b3);
    }
    h
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn deterministic() {
        assert_eq!(fnv1a("hello"), fnv1a("hello"));
        assert_ne!(fnv1a("hello"), fnv1a("world"));
    }

    #[test]
    fn empty_string() {
        // Should not panic and should produce the FNV offset basis
        let _ = fnv1a("");
    }
}
