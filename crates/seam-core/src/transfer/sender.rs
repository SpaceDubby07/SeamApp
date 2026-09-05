//! Outgoing transfer state: the open file we're reading chunks from and
//! how far we've gotten (Tier 7.5). `Session` drives the actual `Chunk`
//! sends — it owns the live `BulkChannel` — one chunk per call, so a
//! multi-gigabyte transfer never blocks its select loop for longer than
//! one chunk write. This module only ever touches the local filesystem.

use std::path::PathBuf;

use tokio::io::{AsyncReadExt, AsyncSeekExt};

use crate::protocol::{FileManifest, TransferId};
use crate::transfer::TransferError;

/// A file offered for send: open and ready to stream once the peer's
/// `TransferAccept` arrives (Tier 7.5's flow — chunks never start before
/// that, even though the manifest was hashed and the file opened up
/// front).
pub struct OutgoingTransfer {
    /// This transfer's identity, echoed on every `Chunk` and in the final
    /// `TransferComplete`.
    pub transfer_id: TransferId,
    /// The manifest already sent in the `TransferOffer`.
    pub manifest: FileManifest,
    /// The file's original path, for `TransferEvent::Completed`.
    pub original_path: PathBuf,
    file: tokio::fs::File,
    /// How many bytes have been sent (or, before `accept`, resumed from).
    pub bytes_sent: u64,
    /// Set once the peer's `TransferAccept` arrives — `Session` only
    /// starts calling `read_next_chunk` once this is `true`.
    pub accepted: bool,
}

impl OutgoingTransfer {
    /// Opens `original_path` for reading, ready to send once the peer
    /// accepts. Does not seek — that happens in `accept`, once we know
    /// `resume_from`.
    ///
    /// # Errors
    /// Returns an error if the file can't be opened.
    pub async fn open(
        transfer_id: TransferId,
        original_path: PathBuf,
        manifest: FileManifest,
    ) -> Result<Self, TransferError> {
        let file = tokio::fs::File::open(&original_path).await?;
        Ok(Self {
            transfer_id,
            manifest,
            original_path,
            file,
            bytes_sent: 0,
            accepted: false,
        })
    }

    /// Seeks to `resume_from` on `TransferAccept`, marking this transfer
    /// ready to send.
    ///
    /// # Errors
    /// Returns an error if the seek fails.
    pub async fn accept(&mut self, resume_from: u64) -> Result<(), TransferError> {
        self.file
            .seek(std::io::SeekFrom::Start(resume_from))
            .await?;
        self.bytes_sent = resume_from;
        self.accepted = true;
        Ok(())
    }

    /// Reads the next chunk (up to `manifest.chunk_size` bytes). Returns
    /// `None` once every byte has been sent — the caller sends
    /// `TransferComplete` at that point, not another `Chunk`.
    ///
    /// # Errors
    /// Returns an error if the read fails.
    ///
    /// # Panics
    /// Never in practice — `want` is capped at `manifest.chunk_size`, a
    /// `u32`, so it always fits in a `usize` on every platform this
    /// project targets.
    pub async fn read_next_chunk(&mut self) -> Result<Option<(u64, Vec<u8>)>, TransferError> {
        if self.bytes_sent >= self.manifest.size {
            return Ok(None);
        }
        let offset = self.bytes_sent;
        let remaining = self.manifest.size - self.bytes_sent;
        let want = remaining.min(u64::from(self.manifest.chunk_size));
        // `want` is bounded above by `chunk_size`, a `u32` — always fits.
        let want_usize = usize::try_from(want).expect("chunk size fits in usize");
        let mut buf = vec![0u8; want_usize];
        self.file.read_exact(&mut buf).await?;
        self.bytes_sent += want;
        Ok(Some((offset, buf)))
    }
}

#[cfg(test)]
mod tests {
    use super::OutgoingTransfer;
    use crate::protocol::TransferId;
    use crate::transfer::manifest::build_manifest;

    async fn write_temp_file(dir: &std::path::Path, name: &str, data: &[u8]) -> std::path::PathBuf {
        let path = dir.join(name);
        tokio::fs::write(&path, data).await.expect("write");
        path
    }

    #[tokio::test]
    async fn reads_the_whole_file_in_chunk_size_pieces() {
        let dir = tempfile::tempdir().expect("tempdir");
        let data = vec![9u8; 10];
        let path = write_temp_file(dir.path(), "small.bin", &data).await;
        let manifest = build_manifest(&path, 4).await.expect("manifest");

        let mut outgoing = OutgoingTransfer::open(TransferId::new(), path, manifest)
            .await
            .expect("open");
        outgoing.accept(0).await.expect("accept");

        let mut collected = Vec::new();
        let mut next_offset = 0u64;
        while let Some((offset, chunk)) = outgoing.read_next_chunk().await.expect("read chunk") {
            assert_eq!(offset, next_offset);
            next_offset += chunk.len() as u64;
            collected.extend_from_slice(&chunk);
        }
        assert_eq!(collected, data);
        assert_eq!(outgoing.bytes_sent, data.len() as u64);
    }

    #[tokio::test]
    async fn accept_with_resume_from_skips_already_sent_bytes() {
        let dir = tempfile::tempdir().expect("tempdir");
        let data: Vec<u8> = (0..20u8).collect();
        let path = write_temp_file(dir.path(), "resumed.bin", &data).await;
        let manifest = build_manifest(&path, 8).await.expect("manifest");

        let mut outgoing = OutgoingTransfer::open(TransferId::new(), path, manifest)
            .await
            .expect("open");
        outgoing.accept(12).await.expect("accept with resume");

        let (offset, chunk) = outgoing
            .read_next_chunk()
            .await
            .expect("read chunk")
            .expect("chunk present");
        assert_eq!(offset, 12);
        assert_eq!(chunk, &data[12..20]);
        assert!(
            outgoing
                .read_next_chunk()
                .await
                .expect("read chunk")
                .is_none()
        );
    }
}
