use crate::chunker::MAX_CHUNK;
use crate::model::{Blob, ChunkRef, EntryKind, ObjectId, ObjectKind, Snapshot};
use crate::store::{object_id, ObjectStore};
use std::io::{self, Read};

fn valid_lengths(blob: &Blob) -> bool {
    blob.chunks
        .iter()
        .all(|chunk| chunk.len > 0 && chunk.len as usize <= MAX_CHUNK)
        && blob
            .chunks
            .iter()
            .map(|chunk| u64::from(chunk.len))
            .sum::<u64>()
            == blob.total_len
}

/// Recorded boundaries let readers verify content without knowing the writer's capture profile.
pub(crate) fn matches_reader(
    store: &ObjectStore,
    expected: &ObjectId,
    actual: Option<ObjectId>,
    mut reader: impl Read,
) -> anyhow::Result<bool> {
    if actual.as_ref() == Some(expected) {
        return Ok(true);
    }
    let blob: Blob = store.read_struct(expected, ObjectKind::Blob)?;
    visit_matching_chunks(&blob, &mut reader, |_, _| Ok(()))
}

/// Visiting verified working bytes permits capture to repair expired chunks without reading stored chunks.
pub(crate) fn visit_matching_chunks(
    blob: &Blob,
    mut reader: impl Read,
    mut visit: impl FnMut(&ChunkRef, &[u8]) -> anyhow::Result<()>,
) -> anyhow::Result<bool> {
    if !valid_lengths(blob) {
        return Ok(false);
    }
    let mut buffer = Vec::new();
    for chunk in &blob.chunks {
        buffer.resize(chunk.len as usize, 0);
        if let Err(error) = reader.read_exact(&mut buffer) {
            return if error.kind() == io::ErrorKind::UnexpectedEof {
                Ok(false)
            } else {
                Err(error.into())
            };
        }
        if object_id(ObjectKind::Chunk, &buffer) != chunk.id {
            return Ok(false);
        }
        visit(chunk, &buffer)?;
    }
    Ok(reader.read(&mut [0])? == 0)
}

/// A cache row takes precedence over the baseline so a changed file is checked against only one layout.
pub(crate) fn recorded_for_path(
    store: &ObjectStore,
    workspace: &str,
    path: &[u8],
    length: u64,
) -> anyhow::Result<Option<(ObjectId, Blob)>> {
    if let Some(row) = store
        .cached_file(workspace, path)?
        .filter(|row| row.size == length)
    {
        let blob: Blob = store.read_struct(&row.blob_id, ObjectKind::Blob)?;
        if blob.total_len == length {
            return Ok(Some((row.blob_id, blob)));
        }
    }
    let id = if let Some(head) = store.workspace_head(workspace)? {
        let snapshot: Snapshot = store.read_struct(&head, ObjectKind::Snapshot)?;
        crate::tree::find_path(store, &snapshot.root_tree, path)?
            .filter(|entry| entry.kind == EntryKind::File)
            .and_then(|entry| entry.target)
    } else {
        None
    };
    let Some(id) = id else { return Ok(None) };
    let blob: Blob = store.read_struct(&id, ObjectKind::Blob)?;
    Ok((blob.total_len == length).then_some((id, blob)))
}
