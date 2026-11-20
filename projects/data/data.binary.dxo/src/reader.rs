//! Reader: open a `.dxo` main file, validate everything declared, then
//! serve bounded chunk reads with digest verification and full-asset
//! acceptance (`verify_all`).

use std::{
    collections::HashSet,
    fs::File,
    io::{self, Read, Seek, SeekFrom},
    path::{Path, PathBuf},
};

use crate::{
    error::DxoError,
    index::{ChunkDesc, Index, sha256_bytes},
    shell::{self, MAIN_HEADER_BYTES, VOLUME_HEADER_BYTES},
};

/// An opened, structurally verified `.dxo` asset.
#[derive(Debug)]
pub struct Asset {
    main_path: PathBuf,
    file_bytes: u64,
    index_bytes: u64,
    index_sha256: [u8; 32],
    index: Index,
    verified_chunks: HashSet<u64>,
}

impl Asset {
    /// Open the main file, verify the shell, index digest and full index
    /// structure. Volumes are only checked when first touched or by
    /// [`Asset::verify_all`].
    pub fn open(main_path: impl AsRef<Path>) -> Result<Self, DxoError> {
        let main_path = main_path.as_ref().to_path_buf();
        let mut file = File::open(&main_path).map_err(|e| DxoError::io(&main_path, e))?;
        let actual = file.metadata().map_err(|e| DxoError::io(&main_path, e))?.len();
        if actual < MAIN_HEADER_BYTES as u64 {
            return Err(DxoError::TooShort { need: MAIN_HEADER_BYTES as u64, have: actual });
        }
        let mut head = vec![0u8; MAIN_HEADER_BYTES];
        read_at(&mut file, &mut head, 0, &main_path)?;
        let main = shell::parse_main(&head, actual).map_err(|e| annotate(e, &main_path))?;
        let mut raw_index = vec![0u8; main.index_bytes as usize];
        read_at(&mut file, &mut raw_index, MAIN_HEADER_BYTES as u64, &main_path)?;
        if sha256_bytes(&raw_index) != main.index_sha256 {
            return Err(DxoError::DigestMismatch { what: format!("index bytes of {}", main_path.display()) });
        }
        let index = Index::parse(&raw_index, main.file_bytes).map_err(|e| annotate(e, &main_path))?;
        Ok(Asset {
            main_path,
            file_bytes: main.file_bytes,
            index_bytes: main.index_bytes,
            index_sha256: main.index_sha256,
            index,
            verified_chunks: HashSet::new(),
        })
    }

    pub fn index(&self) -> &Index {
        &self.index
    }

    pub fn main_path(&self) -> &Path {
        &self.main_path
    }

    /// Path of a numbered volume as derived from the main file name.
    pub fn volume_path(&self, volume_index: u64) -> PathBuf {
        shell::volume_path(&self.main_path, self.index.volume_width, volume_index)
    }

    /// Read one chunk's stored bytes, verifying its digest on first use.
    pub fn read_chunk(&mut self, chunk_id: u64) -> Result<Vec<u8>, DxoError> {
        let chunk = self.index.chunk_by_id(chunk_id).ok_or(DxoError::Range("unknown chunk id"))?.clone();
        let first_use = !self.verified_chunks.contains(&chunk_id);
        let data = self.read_stored(&chunk)?;
        if first_use {
            if sha256_bytes(&data) != chunk.stored_sha256 {
                return Err(DxoError::DigestMismatch { what: format!("chunk {chunk_id} of {}", self.main_path.display()) });
            }
            self.verified_chunks.insert(chunk_id);
        }
        Ok(data)
    }

    /// Read the full logical bytes of one tensor.
    pub fn read_tensor(&mut self, key: &str) -> Result<Vec<u8>, DxoError> {
        let refs = self.index.tensor_by_key(key).ok_or(DxoError::BadIndex("unknown tensor key"))?.chunks.clone();
        let mut out = Vec::new();
        for r in &refs {
            out.extend_from_slice(&self.read_chunk(r.chunk_id)?);
        }
        let t = self.index.tensor_by_key(key).unwrap();
        if t.logical_bytes > 0 && sha256_bytes(&out) != t.logical_sha256 {
            return Err(DxoError::DigestMismatch { what: format!("tensor {key} of {}", self.main_path.display()) });
        }
        Ok(out)
    }

    /// Read a logical byte range of one tensor, touching only the chunks
    /// that overlap the range.
    pub fn read_tensor_range(&mut self, key: &str, start: u64, len: u64) -> Result<Vec<u8>, DxoError> {
        let t = self.index.tensor_by_key(key).ok_or(DxoError::BadIndex("unknown tensor key"))?.clone();
        if start.checked_add(len).map_or(true, |end| end > t.logical_bytes) {
            return Err(DxoError::Range("tensor range beyond logical bytes"));
        }
        let mut out = Vec::new();
        let mut done = 0u64;
        for r in &t.chunks {
            if done >= len {
                break;
            }
            let c = self.index.chunk_by_id(r.chunk_id).unwrap().clone();
            let c_start = r.logical_offset;
            let c_end = c_start + c.logical_bytes;
            if c_end <= start {
                continue;
            }
            let local_start = start.max(c_start) - c_start;
            let take = (c_end.min(start + len) - start.max(c_start)).min(len - done);
            let data = self.read_chunk(r.chunk_id)?;
            out.extend_from_slice(&data[local_start as usize..(local_start + take) as usize]);
            done += take;
        }
        Ok(out)
    }

    /// Read a sample range of one dataset field (common sample chunking).
    pub fn read_samples(&mut self, split: &str, field: &str, start_sample: u64, count: u64) -> Result<Vec<u8>, DxoError> {
        let s = self.index.dataset.as_ref().ok_or(DxoError::BadIndex("asset is not a dataset"))?;
        let split = s.splits.iter().find(|x| x.name == split).ok_or(DxoError::BadIndex("unknown split"))?;
        if start_sample.checked_add(count).map_or(true, |e| e > split.sample_count) {
            return Err(DxoError::Range("sample range beyond split"));
        }
        let f = split.fields.iter().find(|x| x.name == field).ok_or(DxoError::BadIndex("unknown field"))?;
        let key = f.tensor_key.clone();
        let t = self.index.tensor_by_key(&key).ok_or(DxoError::BadIndex("field references unknown tensor"))?;
        let bytes_per_sample = t.logical_bytes.checked_div(split.sample_count).ok_or(DxoError::Range("indivisible field"))?;
        self.read_tensor_range(&key, start_sample * bytes_per_sample, count * bytes_per_sample)
    }

    /// Full acceptance: every volume file and every chunk digest, plus the
    /// logical digest of every tensor.
    pub fn verify_all(&mut self) -> Result<(), DxoError> {
        for vol in &self.index.volumes {
            self.open_volume(vol.volume_index)?;
        }
        let ids: Vec<u64> = self.index.chunks.iter().map(|c| c.chunk_id).collect();
        for id in ids {
            self.read_chunk(id)?;
        }
        let keys: Vec<String> = self.index.tensors.iter().map(|t| t.key.clone()).collect();
        for key in keys {
            self.read_tensor(&key)?;
        }
        Ok(())
    }

    fn open_volume(&self, volume_index: u64) -> Result<File, DxoError> {
        let path = self.volume_path(volume_index);
        let mut file = File::open(&path).map_err(|e| match e.kind() {
            io::ErrorKind::NotFound => DxoError::VolumeMissing { path: path.clone() },
            _ => DxoError::io(&path, e),
        })?;
        let actual = file.metadata().map_err(|e| DxoError::io(&path, e))?.len();
        let mut head = vec![0u8; VOLUME_HEADER_BYTES];
        let n = actual.min(VOLUME_HEADER_BYTES as u64) as usize;
        read_at(&mut file, &mut head[..n], 0, &path)?;
        let vol = shell::parse_volume(&head[..n], actual).map_err(|e| annotate(e, &path))?;
        if vol.volume_index as u64 != volume_index {
            return Err(DxoError::VolumeMismatch { path, expected: volume_index });
        }
        if vol.index_sha256 != self.index_sha256 {
            return Err(DxoError::DigestMismatch { what: format!("volume index digest of {}", path.display()) });
        }
        if let Some(declared) = self.index.volumes.iter().find(|v| v.volume_index == volume_index) {
            if declared.file_bytes != actual {
                return Err(DxoError::FileLengthMismatch { path, expected: declared.file_bytes, actual });
            }
        }
        Ok(file)
    }

    fn read_stored(&self, chunk: &ChunkDesc) -> Result<Vec<u8>, DxoError> {
        let (mut file, path) = if chunk.file_index == 0 {
            let p = self.main_path.clone();
            (File::open(&p).map_err(|e| DxoError::io(&p, e))?, p)
        }
        else {
            let vi = chunk.file_index - 1;
            let path = self.volume_path(vi);
            (self.open_volume(vi)?, path)
        };
        let mut data = vec![0u8; chunk.stored_bytes as usize];
        read_at(&mut file, &mut data, chunk.offset, &path)?;
        let _ = self.file_bytes;
        Ok(data)
    }
}

fn read_at(file: &mut File, buf: &mut [u8], offset: u64, path: &Path) -> Result<(), DxoError> {
    file.seek(SeekFrom::Start(offset)).map_err(|e| DxoError::io(path, e))?;
    file.read_exact(buf).map_err(|e| {
        if e.kind() == io::ErrorKind::UnexpectedEof {
            DxoError::TooShort { need: offset + buf.len() as u64, have: offset }
        }
        else {
            DxoError::io(path, e)
        }
    })?;
    Ok(())
}

/// Attach the asset path to structural errors that lack one.
fn annotate(e: DxoError, path: &Path) -> DxoError {
    match e {
        DxoError::TooShort { need, have } => DxoError::FileLengthMismatch { path: path.to_path_buf(), expected: need, actual: have },
        other => other,
    }
}
