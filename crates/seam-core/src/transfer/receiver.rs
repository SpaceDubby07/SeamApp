//! Incoming transfer state: the `.part` file being written to, resume
//! bookkeeping, and hash verification on completion (Tier 7.5).

use std::path::PathBuf;

use tokio::io::{AsyncSeekExt, AsyncWriteExt};

use crate::protocol::{FileManifest, TransferId};
use crate::transfer::TransferError;
use crate::transfer::manifest::{ResumeState, hash_file, part_path};

/// A file being received: bytes land in `part_path(dest_path)` until
/// `finalize` verifies the whole thing and renames it into place.
pub struct IncomingTransfer {
    /// This transfer's identity, matched against every `Chunk` and the
    /// final `TransferComplete`.
    pub transfer_id: TransferId,
    /// The manifest this transfer was offered with.
    pub manifest: FileManifest,
    /// Where the file lands once verified — bytes accumulate at
    /// `part_path(dest_path)` until then.
    pub dest_path: PathBuf,
    file: tokio::fs::File,
    /// How many bytes of the `.part` file are valid so far. Chunks arrive
    /// in order (a single ordered TCP+TLS bulk connection can't reorder
    /// them), so in practice this only ever grows by exactly the size of
    /// each chunk — tracked via `max` regardless, since trusting the
    /// wire's offset rather than assuming contiguity costs nothing and
    /// makes no ordering assumption load-bearing.
    pub bytes_received: u64,
    /// The hash from `TransferComplete`, which can arrive before the last
    /// `Chunk` does — they travel on different connections (control vs.
    /// bulk) with no ordering guarantee between them. `finalize` only
    /// actually runs once BOTH this is set AND `bytes_received` reaches
    /// `manifest.size` (see `is_ready_to_finalize`).
    pub complete_hash: Option<[u8; 32]>,
}

impl IncomingTransfer {
    /// Opens (or resumes) the `.part` file for `dest_path`, creating its
    /// parent directory if needed. Returns the byte offset to tell the
    /// sender to resume from via `TransferAccept`.
    ///
    /// A prior sidecar is only trusted if it names the SAME `transfer_id`
    /// and `expected_hash` as this offer — a different transfer, or a
    /// source file that's changed since, starts over from zero rather
    /// than resuming against bytes that may no longer mean what the
    /// sender thinks they mean.
    ///
    /// # Errors
    /// Returns an error if the parent directory can't be created, or the
    /// `.part` file can't be opened/seeked, or the sidecar can't be
    /// written.
    pub async fn open(
        transfer_id: TransferId,
        manifest: FileManifest,
        dest_path: PathBuf,
    ) -> Result<(Self, u64), TransferError> {
        if let Some(parent) = dest_path.parent() {
            tokio::fs::create_dir_all(parent).await?;
        }

        let part = part_path(&dest_path);
        let resume_from = match ResumeState::load(&dest_path).await {
            Some(state)
                if state.transfer_id == transfer_id && state.expected_hash == manifest.hash =>
            {
                state.bytes_received
            }
            _ => 0,
        };

        let mut file = tokio::fs::OpenOptions::new()
            .create(true)
            .write(true)
            .truncate(resume_from == 0)
            .open(&part)
            .await?;
        file.seek(std::io::SeekFrom::Start(resume_from)).await?;

        ResumeState {
            transfer_id,
            expected_hash: manifest.hash,
            bytes_received: resume_from,
        }
        .save(&dest_path)
        .await?;

        Ok((
            Self {
                transfer_id,
                manifest,
                dest_path,
                file,
                bytes_received: resume_from,
                complete_hash: None,
            },
            resume_from,
        ))
    }

    /// Writes one chunk at `offset` and updates the resume sidecar.
    ///
    /// # Errors
    /// Returns an error if the seek, write, or sidecar update fails.
    pub async fn write_chunk(&mut self, offset: u64, data: &[u8]) -> Result<(), TransferError> {
        self.file.seek(std::io::SeekFrom::Start(offset)).await?;
        self.file.write_all(data).await?;
        self.bytes_received = self.bytes_received.max(offset + data.len() as u64);
        ResumeState {
            transfer_id: self.transfer_id,
            expected_hash: self.manifest.hash,
            bytes_received: self.bytes_received,
        }
        .save(&self.dest_path)
        .await?;
        Ok(())
    }

    /// True once every byte has arrived AND the sender's `TransferComplete`
    /// hash has been seen — the point at which `finalize` should run.
    #[must_use]
    pub fn is_ready_to_finalize(&self) -> bool {
        self.complete_hash.is_some() && self.bytes_received >= self.manifest.size
    }

    /// Re-hashes the completed `.part` file, and on a match flushes it,
    /// renames it into place, restores its original modification time (if
    /// the sender's manifest had one), and removes the resume sidecar.
    ///
    /// On a hash mismatch, the `.part` file and sidecar are left in place
    /// — Tier 7.5 doesn't specify recovery, and leaving them lets a fresh
    /// offer resume rather than silently losing the download.
    ///
    /// # Errors
    /// Returns [`TransferError::HashMismatch`] if the re-hash doesn't
    /// match `complete_hash`, or an I/O error from any of the flush/hash/
    /// rename/sidecar-removal steps.
    ///
    /// # Panics
    /// Panics if called before `is_ready_to_finalize` is `true` — `Session`
    /// is the only caller and always checks first.
    pub async fn finalize(&mut self) -> Result<PathBuf, TransferError> {
        let expected = self
            .complete_hash
            .expect("finalize only called once is_ready_to_finalize is true");
        self.file.flush().await?;

        let part = part_path(&self.dest_path);
        let actual = hash_file(&part).await?;
        if actual != expected {
            return Err(TransferError::HashMismatch);
        }

        tokio::fs::rename(&part, &self.dest_path).await?;
        if let Some(modified) = self.manifest.modified {
            let mtime =
                filetime::FileTime::from_unix_time(i64::try_from(modified).unwrap_or(i64::MAX), 0);
            let dest = self.dest_path.clone();
            // `filetime` is a blocking filesystem call — off the async
            // runtime's worker thread, matching the same care every other
            // real I/O call in this module already gets via `tokio::fs`.
            tokio::task::spawn_blocking(move || filetime::set_file_mtime(&dest, mtime))
                .await
                .expect("blocking task panicked")?;
        }
        ResumeState::remove(&self.dest_path).await?;
        Ok(self.dest_path.clone())
    }
}

#[cfg(test)]
mod tests {
    use super::IncomingTransfer;
    use crate::protocol::{FileManifest, TransferId};
    use crate::transfer::TransferError;
    use crate::transfer::manifest::{ResumeState, hash_file, part_path};

    fn manifest(hash: [u8; 32], size: u64) -> FileManifest {
        FileManifest {
            name: "movie.mp4".to_string(),
            size,
            hash,
            chunk_size: 4,
            modified: None,
        }
    }

    #[tokio::test]
    async fn writes_chunks_and_finalizes_on_matching_hash() {
        let dir = tempfile::tempdir().expect("tempdir");
        let dest = dir.path().join("movie.mp4");
        let data = b"hello, seam transfer!";
        let hash = *blake3::hash(data).as_bytes();
        let transfer_id = TransferId::new();

        let (mut incoming, resume_from) =
            IncomingTransfer::open(transfer_id, manifest(hash, data.len() as u64), dest.clone())
                .await
                .expect("open");
        assert_eq!(resume_from, 0);

        incoming
            .write_chunk(0, &data[..10])
            .await
            .expect("write first chunk");
        assert!(!incoming.is_ready_to_finalize());
        incoming
            .write_chunk(10, &data[10..])
            .await
            .expect("write second chunk");

        incoming.complete_hash = Some(hash);
        assert!(incoming.is_ready_to_finalize());

        let final_path = incoming.finalize().await.expect("finalize");
        assert_eq!(final_path, dest);
        let contents = tokio::fs::read(&dest).await.expect("read final file");
        assert_eq!(contents, data);
        assert!(!part_path(&dest).exists());
        assert!(ResumeState::load(&dest).await.is_none());
    }

    #[tokio::test]
    async fn hash_mismatch_leaves_the_part_file_in_place() {
        let dir = tempfile::tempdir().expect("tempdir");
        let dest = dir.path().join("corrupt.bin");
        let data = b"not what the sender claimed";
        let wrong_hash = [0xAAu8; 32];
        let transfer_id = TransferId::new();

        let (mut incoming, _) = IncomingTransfer::open(
            transfer_id,
            manifest(wrong_hash, data.len() as u64),
            dest.clone(),
        )
        .await
        .expect("open");
        incoming.write_chunk(0, data).await.expect("write chunk");
        incoming.complete_hash = Some(wrong_hash);

        let result = incoming.finalize().await;
        assert!(matches!(result, Err(TransferError::HashMismatch)));
        assert!(part_path(&dest).exists());
        assert!(!dest.exists());
    }

    #[tokio::test]
    async fn reopening_with_a_matching_sidecar_resumes_from_the_recorded_offset() {
        let dir = tempfile::tempdir().expect("tempdir");
        let dest = dir.path().join("resumable.bin");
        let hash = [3u8; 32];
        let transfer_id = TransferId::new();

        let (mut first, _) = IncomingTransfer::open(transfer_id, manifest(hash, 100), dest.clone())
            .await
            .expect("first open");
        first.write_chunk(0, &[1u8; 40]).await.expect("write");
        drop(first);

        let (second, resume_from) = IncomingTransfer::open(transfer_id, manifest(hash, 100), dest)
            .await
            .expect("reopen");
        assert_eq!(resume_from, 40);
        assert_eq!(second.bytes_received, 40);
    }

    #[tokio::test]
    async fn reopening_with_a_mismatched_transfer_id_starts_over() {
        let dir = tempfile::tempdir().expect("tempdir");
        let dest = dir.path().join("restarted.bin");
        let hash = [5u8; 32];

        let (mut first, _) =
            IncomingTransfer::open(TransferId::new(), manifest(hash, 100), dest.clone())
                .await
                .expect("first open");
        first.write_chunk(0, &[1u8; 40]).await.expect("write");
        drop(first);

        // A different transfer id for what LOOKS like the same file must
        // not trust the old sidecar's offset.
        let (second, resume_from) =
            IncomingTransfer::open(TransferId::new(), manifest(hash, 100), dest)
                .await
                .expect("reopen with new transfer id");
        assert_eq!(resume_from, 0);
        assert_eq!(second.bytes_received, 0);
    }

    #[tokio::test]
    async fn restores_original_mtime_on_finalize() {
        let dir = tempfile::tempdir().expect("tempdir");
        let dest = dir.path().join("timestamped.bin");
        let data = b"time travel";
        let hash = *blake3::hash(data).as_bytes();
        let mut m = manifest(hash, data.len() as u64);
        // An arbitrary fixed point well in the past, so the assertion
        // can't accidentally match "just created".
        m.modified = Some(1_000_000_000);

        let (mut incoming, _) = IncomingTransfer::open(TransferId::new(), m, dest.clone())
            .await
            .expect("open");
        incoming.write_chunk(0, data).await.expect("write");
        incoming.complete_hash = Some(hash);
        incoming.finalize().await.expect("finalize");

        let metadata = tokio::fs::metadata(&dest).await.expect("metadata");
        let mtime = metadata.modified().expect("modified");
        let unix_secs = mtime
            .duration_since(std::time::UNIX_EPOCH)
            .expect("after epoch")
            .as_secs();
        assert_eq!(unix_secs, 1_000_000_000);
    }

    #[tokio::test]
    async fn hash_file_helper_matches_finalized_content() {
        let dir = tempfile::tempdir().expect("tempdir");
        let dest = dir.path().join("verify.bin");
        let data = b"verify me";
        let hash = *blake3::hash(data).as_bytes();

        let (mut incoming, _) =
            IncomingTransfer::open(TransferId::new(), manifest(hash, data.len() as u64), dest)
                .await
                .expect("open");
        incoming.write_chunk(0, data).await.expect("write");
        let part = part_path(&incoming.dest_path.clone());
        assert_eq!(hash_file(&part).await.expect("hash"), hash);
    }
}
