//! Building a [`FileManifest`] from a real file (streaming BLAKE3 hash),
//! path helpers for where incoming bytes land, and the on-disk resume
//! sidecar format (Tier 7.5: "write a sidecar `.hoppr-part` JSON next to
//! the incoming file" — here named `.part.json`, next to the `.part` data
//! file it describes).

use std::path::{Path, PathBuf};
use std::time::UNIX_EPOCH;

use serde::{Deserialize, Serialize};
use tokio::io::AsyncReadExt;

use crate::protocol::{FileManifest, TransferId};
use crate::transfer::TransferError;

/// Buffer size for the hashing pass in [`hash_file`]. Independent of the
/// wire chunk size (Tier 7.5's `CHUNK_SIZE`) — this is a read-only local
/// pass, never sent anywhere.
const HASH_BUF_SIZE: usize = 512 * 1024;

/// Streams `path` once, computing its BLAKE3 hash.
///
/// # Errors
/// Returns an error if `path` can't be opened or read.
pub async fn hash_file(path: &Path) -> Result<[u8; 32], TransferError> {
    let mut file = tokio::fs::File::open(path).await?;
    let mut hasher = blake3::Hasher::new();
    let mut buf = vec![0u8; HASH_BUF_SIZE];
    loop {
        let n = file.read(&mut buf).await?;
        if n == 0 {
            break;
        }
        hasher.update(&buf[..n]);
    }
    Ok(*hasher.finalize().as_bytes())
}

/// Streams `path` once, computing the [`FileManifest`] to offer for it:
/// name, size, BLAKE3 hash, and (if available) original modification
/// time.
///
/// # Errors
/// Returns an error if `path` can't be opened, read, or `stat`-ed.
pub async fn build_manifest(path: &Path, chunk_size: u32) -> Result<FileManifest, TransferError> {
    let metadata = tokio::fs::metadata(path).await?;
    let modified = metadata
        .modified()
        .ok()
        .and_then(|t| t.duration_since(UNIX_EPOCH).ok())
        .map(|d| d.as_secs());
    let hash = hash_file(path).await?;
    let name = path
        .file_name()
        .map_or_else(|| "file".to_string(), |n| n.to_string_lossy().into_owned());

    Ok(FileManifest {
        name,
        size: metadata.len(),
        hash,
        chunk_size,
        modified,
    })
}

/// Appends `.part` to `dest`'s filename — where incoming bytes land until
/// the transfer is verified complete (Tier 7.5).
#[must_use]
pub fn part_path(dest: &Path) -> PathBuf {
    append_suffix(dest, ".part")
}

/// The resume-state sidecar for `dest` — recording enough to resume an
/// interrupted transfer without re-deriving it from the `.part` file alone
/// (Tier 7.5).
#[must_use]
pub fn sidecar_path(dest: &Path) -> PathBuf {
    append_suffix(dest, ".part.json")
}

fn append_suffix(path: &Path, suffix: &str) -> PathBuf {
    let mut name = path.as_os_str().to_os_string();
    name.push(suffix);
    PathBuf::from(name)
}

/// Takes only the final path component of a peer-supplied file name,
/// falling back to a generic name for anything empty or path-traversal-
/// shaped (`../../etc/passwd`). The peer is trusted (post-pairing) to
/// relay input faithfully, not to hand us a safe filesystem path — this is
/// what keeps an incoming transfer confined to the configured download
/// directory regardless.
#[must_use]
pub fn sanitize_file_name(name: &str) -> String {
    Path::new(name)
        .file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .filter(|n| !n.is_empty())
        .unwrap_or_else(|| "transferred_file".to_string())
}

/// What's persisted alongside a partial incoming file so a later run can
/// resume it (Tier 7.5) rather than starting over.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct ResumeState {
    /// Which transfer this partial file belongs to — a resume offer for a
    /// DIFFERENT transfer id (e.g. the sender restarted and reassigned
    /// ids) must not be trusted, since chunk offsets from one transfer are
    /// meaningless for another.
    pub transfer_id: TransferId,
    /// The complete file's expected BLAKE3 hash, from the offer's
    /// manifest — a second guard alongside `transfer_id` against resuming
    /// against a file that's since changed.
    pub expected_hash: [u8; 32],
    /// How many bytes of `part_path(dest)` are valid so far.
    pub bytes_received: u64,
}

impl ResumeState {
    /// Reads the sidecar for `dest`, if any. Returns `None` — rather than
    /// erroring — for anything missing or unparseable: a stale or corrupt
    /// sidecar just means starting the transfer over, not a hard failure.
    pub async fn load(dest: &Path) -> Option<Self> {
        let contents = tokio::fs::read(sidecar_path(dest)).await.ok()?;
        serde_json::from_slice(&contents).ok()
    }

    /// Writes this state to `dest`'s sidecar, overwriting any previous
    /// one.
    ///
    /// # Errors
    /// Returns an error if the write fails.
    ///
    /// # Panics
    /// Never in practice — `ResumeState` is plain data with no type that
    /// can fail to serialize to JSON (no maps with non-string keys, no
    /// floats).
    pub async fn save(&self, dest: &Path) -> Result<(), TransferError> {
        let contents = serde_json::to_vec(self).expect("ResumeState always serializes");
        tokio::fs::write(sidecar_path(dest), contents).await?;
        Ok(())
    }

    /// Removes `dest`'s sidecar, if any. Not finding one is not an error —
    /// this is called unconditionally once a transfer finalizes.
    ///
    /// # Errors
    /// Returns an error if the file exists but can't be removed.
    pub async fn remove(dest: &Path) -> Result<(), TransferError> {
        match tokio::fs::remove_file(sidecar_path(dest)).await {
            Ok(()) => Ok(()),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(()),
            Err(e) => Err(e.into()),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::{ResumeState, build_manifest, hash_file, sanitize_file_name};
    use crate::protocol::TransferId;

    #[tokio::test]
    async fn build_manifest_hashes_and_stats_a_real_file() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("hello.txt");
        tokio::fs::write(&path, b"hello, seam")
            .await
            .expect("write");

        let manifest = build_manifest(&path, 512 * 1024)
            .await
            .expect("build manifest");
        assert_eq!(manifest.name, "hello.txt");
        assert_eq!(manifest.size, 11);
        assert_eq!(manifest.hash, *blake3::hash(b"hello, seam").as_bytes());
    }

    #[tokio::test]
    async fn hash_file_matches_blake3_reference() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("data.bin");
        let data = vec![7u8; 3 * 1024 * 1024 + 17]; // spans several hash-buffer reads
        tokio::fs::write(&path, &data).await.expect("write");

        let hash = hash_file(&path).await.expect("hash");
        assert_eq!(hash, *blake3::hash(&data).as_bytes());
    }

    #[test]
    fn sanitize_file_name_strips_path_traversal() {
        assert_eq!(sanitize_file_name("../../etc/passwd"), "passwd");
        assert_eq!(sanitize_file_name("/etc/passwd"), "passwd");
        assert_eq!(sanitize_file_name("report.pdf"), "report.pdf");
    }

    #[test]
    fn sanitize_file_name_falls_back_for_empty_or_traversal_only() {
        assert_eq!(sanitize_file_name(""), "transferred_file");
        assert_eq!(sanitize_file_name(".."), "transferred_file");
        assert_eq!(sanitize_file_name("/"), "transferred_file");
    }

    #[tokio::test]
    async fn resume_state_roundtrips_through_its_sidecar() {
        let dir = tempfile::tempdir().expect("tempdir");
        let dest = dir.path().join("movie.mp4");
        let state = ResumeState {
            transfer_id: TransferId::new(),
            expected_hash: [7u8; 32],
            bytes_received: 12345,
        };

        assert!(ResumeState::load(&dest).await.is_none());
        state.save(&dest).await.expect("save");
        let loaded = ResumeState::load(&dest).await.expect("load");
        assert_eq!(loaded, state);

        ResumeState::remove(&dest).await.expect("remove");
        assert!(ResumeState::load(&dest).await.is_none());
        // Removing again (no sidecar present) must not error.
        ResumeState::remove(&dest)
            .await
            .expect("remove is idempotent");
    }
}
