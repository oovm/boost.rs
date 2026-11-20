//! Writer: deterministic layout of tensors into chunks and volumes, then
//! publication in the contract order (volumes first, main entry last).

use std::{
    collections::BTreeMap,
    fs,
    io::Write,
    path::{Path, PathBuf},
};

use crate::{
    error::DxoError,
    index::{CHUNK_ALIGN, ChunkDesc, ChunkRef, DatasetDesc, Dtype, Index, Kind, Provenance, TensorDesc, VolumeDesc, WeightsDesc, sha256_bytes},
    shell::{self, MAIN_HEADER_BYTES, VOLUME_HEADER_BYTES},
};

/// One tensor with its full logical bytes.
#[derive(Debug, Clone)]
pub struct TensorInput {
    pub key: String,
    pub dtype: Dtype,
    pub shape: Vec<u64>,
    pub data: Vec<u8>,
}

/// Everything needed to build one asset deterministically.
#[derive(Debug, Clone)]
pub struct AssetSpec {
    pub kind: Kind,
    pub tensors: Vec<TensorInput>,
    /// Internal split points per tensor key (logical byte offsets, strictly
    /// inside `(0, logical_bytes)`). Tensors without an entry become one
    /// chunk.
    pub boundaries: BTreeMap<String, Vec<u64>>,
    pub dataset: Option<DatasetDesc>,
    pub weights: Option<WeightsDesc>,
    pub provenance: Option<Provenance>,
    /// 0 for a single-file asset, otherwise 2..=8.
    pub volume_width: u64,
    /// Soft cap per volume file; required when `volume_width > 0`.
    pub max_volume_bytes: Option<u64>,
}

/// A fully built asset, not yet written anywhere.
#[derive(Debug, Clone)]
pub struct Artifact {
    pub index: Index,
    pub main: Vec<u8>,
    /// `(volume_index, bytes)` in creation order.
    pub volumes: Vec<(u64, Vec<u8>)>,
}

fn align64(x: u64) -> u64 {
    x.div_ceil(CHUNK_ALIGN) * CHUNK_ALIGN
}

/// One planned chunk before digests/offsets are final.
struct Planned {
    chunk_id: u64,
    tensor_key: String,
    logical_offset: u64,
    data: Vec<u8>,
}

/// Build the artifact: chunk plan, file assignment, offsets, index, shells.
pub fn build(spec: &AssetSpec) -> Result<Artifact, DxoError> {
    if spec.volume_width != 0 && !(2..=8).contains(&spec.volume_width) {
        return Err(DxoError::WriterInput("volume_width must be 0 or 2..=8".into()));
    }
    if spec.volume_width > 0 && spec.max_volume_bytes.is_none() {
        return Err(DxoError::WriterInput("volume_width > 0 requires max_volume_bytes".into()));
    }

    // 1. Split tensors into chunks at the declared boundaries.
    let mut planned: Vec<Planned> = Vec::new();
    let mut next_id = 0u64;
    for t in &spec.tensors {
        let logical = t.data.len() as u64;
        let mut cuts: Vec<u64> = Vec::new();
        cuts.push(0);
        if let Some(b) = spec.boundaries.get(&t.key) {
            let mut prev = 0u64;
            for &c in b {
                if c <= prev || c >= logical {
                    return Err(DxoError::WriterInput(format!("boundary {c} out of range for tensor {}", t.key)));
                }
                cuts.push(c);
                prev = c;
            }
        }
        cuts.push(logical);
        for w in cuts.windows(2) {
            if w[1] == w[0] {
                continue;
            }
            planned.push(Planned {
                chunk_id: next_id,
                tensor_key: t.key.clone(),
                logical_offset: w[0],
                data: t.data[w[0] as usize..w[1] as usize].to_vec(),
            });
            next_id += 1;
        }
    }

    // 2. Assign chunks to files. With volumes, the main file carries only
    //    header + index; chunks fill numbered volumes in order.
    let mut files: Vec<Vec<u64>> = Vec::new(); // per file_index: chunk ids in order
    if spec.volume_width == 0 {
        files.push(planned.iter().map(|p| p.chunk_id).collect());
    }
    else {
        files.push(Vec::new()); // main file holds no chunks
        let cap = spec.max_volume_bytes.unwrap();
        let mut current: Option<usize> = None;
        let mut cursor = 0u64;
        for p in &planned {
            let stored = p.data.len() as u64;
            let need_new = current.is_none() || align64(cursor) + stored > cap;
            if need_new {
                files.push(Vec::new());
                current = Some(files.len() - 1);
                cursor = VOLUME_HEADER_BYTES as u64;
            }
            let off = align64(cursor);
            if off + stored > cap {
                return Err(DxoError::WriterInput(format!("chunk {} exceeds max_volume_bytes", p.chunk_id)));
            }
            files[current.unwrap()].push(p.chunk_id);
            cursor = off + stored;
        }
    }
    let volume_count = files.len() - 1;
    if volume_count as u64 > 10u64.checked_pow(spec.volume_width as u32).unwrap_or(u64::MAX) {
        return Err(DxoError::WriterInput("too many volumes for volume_width".into()));
    }

    // 3. Volume bytes are independent of the index; build them first.
    let mut volume_bytes: Vec<Vec<u8>> = Vec::new();
    let mut volume_offsets: BTreeMap<u64, (u64, u64)> = BTreeMap::new(); // chunk_id -> (file_index, offset)
    for vi in 0..volume_count {
        let mut buf: Vec<u8> = Vec::new();
        buf.resize(VOLUME_HEADER_BYTES, 0);
        for &id in &files[vi + 1] {
            let p = &planned[id as usize];
            let off = align64(buf.len() as u64);
            buf.resize(off as usize, 0);
            buf.extend_from_slice(&p.data);
            volume_offsets.insert(id, ((vi + 1) as u64, off));
        }
        volume_bytes.push(buf);
    }
    let volumes: Vec<VolumeDesc> =
        (0..volume_count).map(|vi| VolumeDesc { volume_index: vi as u64, file_bytes: volume_bytes[vi].len() as u64 }).collect();

    // 4. Main-file chunk offsets depend on the index length, which contains
    //    those offsets. Iterate to a fixed point.
    let mut index_len_guess: u64 = 4096;
    let mut main_offsets: BTreeMap<u64, u64> = BTreeMap::new();
    let mut index: Index;
    let mut rounds = 0u32;
    loop {
        let mut cursor = align64(MAIN_HEADER_BYTES as u64 + index_len_guess);
        main_offsets.clear();
        for &id in &files[0] {
            main_offsets.insert(id, cursor);
            cursor = align64(cursor + planned[id as usize].data.len() as u64);
        }
        index = assemble_index(spec, &planned, &volumes, &volume_offsets, &main_offsets)?;
        let real_len = index.canonical_bytes().len() as u64;
        if real_len == index_len_guess {
            break;
        }
        rounds += 1;
        if rounds > 16 || real_len > 64 * 1024 * 1024 {
            return Err(DxoError::NotConverged);
        }
        index_len_guess = real_len;
    }

    // 5. Assemble the main file.
    let mut main_buf: Vec<u8> = Vec::new();
    main_buf.resize(MAIN_HEADER_BYTES, 0);
    let raw_index = index.canonical_bytes();
    main_buf.extend_from_slice(&raw_index);
    for &id in &files[0] {
        let off = main_offsets[&id];
        main_buf.resize(off as usize, 0);
        main_buf.extend_from_slice(&planned[id as usize].data);
    }
    let index_sha256 = sha256_bytes(&raw_index);
    let header = shell::write_main(raw_index.len() as u64, main_buf.len() as u64, &index_sha256);
    main_buf[..MAIN_HEADER_BYTES].copy_from_slice(&header);

    // 6. Volume headers carry the same index digest.
    let mut out_volumes: Vec<(u64, Vec<u8>)> = Vec::new();
    for (vi, mut buf) in volume_bytes.into_iter().enumerate() {
        let head = shell::write_volume(vi as u32, buf.len() as u64, &index_sha256);
        buf[..VOLUME_HEADER_BYTES].copy_from_slice(&head);
        out_volumes.push((vi as u64, buf));
    }

    // 7. Final self-check with the real bounds.
    index.validate_with_floor(main_buf.len() as u64, align64(MAIN_HEADER_BYTES as u64 + raw_index.len() as u64))?;

    Ok(Artifact { index, main: main_buf, volumes: out_volumes })
}

fn assemble_index(
    spec: &AssetSpec,
    planned: &[Planned],
    volumes: &[VolumeDesc],
    volume_offsets: &BTreeMap<u64, (u64, u64)>,
    main_offsets: &BTreeMap<u64, u64>,
) -> Result<Index, DxoError> {
    let mut chunks: Vec<ChunkDesc> = Vec::new();
    for p in planned {
        let (file_index, offset) = if let Some(&o) = main_offsets.get(&p.chunk_id) {
            (0u64, o)
        }
        else if let Some(&(f, o)) = volume_offsets.get(&p.chunk_id) {
            (f, o)
        }
        else {
            return Err(DxoError::WriterInput(format!("chunk {} was not placed", p.chunk_id)));
        };
        let stored = p.data.len() as u64;
        chunks.push(ChunkDesc {
            chunk_id: p.chunk_id,
            file_index,
            offset,
            stored_bytes: stored,
            logical_bytes: stored,
            stored_sha256: sha256_bytes(&p.data),
        });
    }
    let mut tensors: Vec<TensorDesc> = Vec::new();
    for t in &spec.tensors {
        let refs: Vec<ChunkRef> = planned
            .iter()
            .filter(|p| p.tensor_key == t.key)
            .map(|p| ChunkRef { chunk_id: p.chunk_id, logical_offset: p.logical_offset })
            .collect();
        if t.data.is_empty() && !refs.is_empty() {
            return Err(DxoError::WriterInput("empty tensor must have no chunks".into()));
        }
        tensors.push(TensorDesc {
            key: t.key.clone(),
            dtype: t.dtype,
            shape: t.shape.clone(),
            logical_bytes: t.data.len() as u64,
            logical_sha256: sha256_bytes(&t.data),
            chunks: refs,
        });
    }
    let mut idx = Index {
        kind: spec.kind,
        semantic_sha256: [0u8; 32],
        required_features: Vec::new(),
        volume_width: spec.volume_width,
        volumes: volumes.to_vec(),
        tensors,
        chunks,
        dataset: spec.dataset.clone(),
        weights: spec.weights.clone(),
        provenance: spec.provenance.clone(),
    };
    idx.semantic_sha256 = idx.compute_semantic_sha256();
    Ok(idx)
}

impl Artifact {
    /// Publish under `dir` with the entry name (e.g. `mnist.dxo`).
    /// Volumes land first, then the main file, each via a temporary file
    /// renamed into place. Failed publishes remove their temporaries.
    pub fn publish(&self, dir: &Path, name: &str) -> Result<(), DxoError> {
        fs::create_dir_all(dir).map_err(|e| DxoError::io(dir, e))?;
        let mut written: Vec<PathBuf> = Vec::new();
        let result = self.publish_inner(dir, name, &mut written);
        if result.is_err() {
            for p in &written {
                let _ = fs::remove_file(p);
            }
        }
        result
    }

    fn publish_inner(&self, dir: &Path, name: &str, written: &mut Vec<PathBuf>) -> Result<(), DxoError> {
        for (vi, bytes) in &self.volumes {
            let path = shell::volume_path(&dir.join(name), self.index.volume_width, *vi);
            write_atomic(&path, bytes)?;
            written.push(path);
        }
        let main = dir.join(name);
        write_atomic(&main, &self.main)?;
        written.push(main);
        Ok(())
    }
}

fn write_atomic(path: &Path, bytes: &[u8]) -> Result<(), DxoError> {
    let tmp = path.with_extension("dxo-tmp");
    {
        let mut f = fs::File::create(&tmp).map_err(|e| DxoError::io(&tmp, e))?;
        f.write_all(bytes).map_err(|e| DxoError::io(&tmp, e))?;
        f.sync_all().map_err(|e| DxoError::io(&tmp, e))?;
    }
    fs::rename(&tmp, path).map_err(|e| DxoError::io(path, e))?;
    Ok(())
}

/// Sample-aligned chunk boundaries for one dataset field tensor:
/// `samples_per_chunk` samples per chunk, tail chunk shorter.
pub fn sample_boundaries(sample_count: u64, bytes_per_sample: u64, samples_per_chunk: u64) -> Vec<u64> {
    let mut out = Vec::new();
    if samples_per_chunk == 0 || bytes_per_sample == 0 || sample_count <= samples_per_chunk {
        return out;
    }
    let mut done = samples_per_chunk;
    while done < sample_count {
        out.push(done * bytes_per_sample);
        done = done.saturating_add(samples_per_chunk);
    }
    out
}
