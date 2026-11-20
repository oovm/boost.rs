//! Typed index model, strict parsing, structural validation and the
//! semantic digest defined by the `.dxo` contract (design doc 43 §2).

use sha2::{Digest, Sha256};

use crate::{
    error::DxoError,
    json::{Json, canonical_bytes, decode_hex32, encode_hex32, jarr, jstr, ju64, obj},
};

/// Format version accepted by this implementation.
pub const FORMAT_MAJOR: u16 = 1;
pub const FORMAT_MINOR: u16 = 0;

/// Chunk offsets must be 64-byte aligned inside their file.
pub const CHUNK_ALIGN: u64 = 64;

/// Asset kind declared by the index.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Kind {
    Dataset,
    Weights,
}

impl Kind {
    pub(crate) fn as_str(self) -> &'static str {
        match self {
            Kind::Dataset => "dataset",
            Kind::Weights => "weights",
        }
    }

    pub(crate) fn parse(s: &str) -> Result<Self, DxoError> {
        match s {
            "dataset" => Ok(Kind::Dataset),
            "weights" => Ok(Kind::Weights),
            // `checkpoint` is reserved by the contract and rejected until a
            // recovery contract is frozen.
            other => Err(DxoError::UnsupportedKind(other.to_string())),
        }
    }
}

/// Element dtype supported in v1.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Dtype {
    U8,
    F32,
}

impl Dtype {
    pub(crate) fn as_str(self) -> &'static str {
        match self {
            Dtype::U8 => "u8",
            Dtype::F32 => "f32",
        }
    }

    pub(crate) fn elem_bytes(self) -> u64 {
        match self {
            Dtype::U8 => 1,
            Dtype::F32 => 4,
        }
    }

    pub(crate) fn parse(s: &str) -> Result<Self, DxoError> {
        match s {
            "u8" => Ok(Dtype::U8),
            "f32" => Ok(Dtype::F32),
            other => Err(DxoError::UnsupportedDtype(other.to_string())),
        }
    }
}

/// Physical description of one numbered volume.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct VolumeDesc {
    /// Numeric suffix value used in the volume file name.
    pub volume_index: u64,
    /// Exact file length of this volume.
    pub file_bytes: u64,
}

/// One stored chunk: the unit of random read and digest verification.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ChunkDesc {
    pub chunk_id: u64,
    /// 0 = main file, n+1 = volume with `volume_index == n`.
    pub file_index: u64,
    pub offset: u64,
    pub stored_bytes: u64,
    pub logical_bytes: u64,
    pub stored_sha256: [u8; 32],
}

/// Reference from a tensor to one of its chunks.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ChunkRef {
    pub chunk_id: u64,
    /// Logical offset of this chunk inside the tensor.
    pub logical_offset: u64,
}

/// Logical description of one tensor and its chunk coverage.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TensorDesc {
    pub key: String,
    pub dtype: Dtype,
    pub shape: Vec<u64>,
    pub logical_bytes: u64,
    pub logical_sha256: [u8; 32],
    /// Sorted by `logical_offset`.
    pub chunks: Vec<ChunkRef>,
}

/// Dataset field mapping inside a split.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FieldDesc {
    pub name: String,
    pub tensor_key: String,
    pub role: String,
}

/// One named dataset split.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SplitDesc {
    pub name: String,
    pub sample_count: u64,
    pub fields: Vec<FieldDesc>,
}

/// Kind description for `dataset` assets.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DatasetDesc {
    pub splits: Vec<SplitDesc>,
}

/// One weights entry mapping a tensor to a role.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WeightEntry {
    pub tensor_key: String,
    pub role: String,
}

/// Kind description for `weights` assets.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WeightsDesc {
    pub architecture_id: String,
    pub state_dict_contract: String,
    pub entries: Vec<WeightEntry>,
}

/// Optional provenance record; unknown evidence stays the literal "unknown".
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct Provenance {
    pub upstream_uri: Option<String>,
    pub revision: Option<String>,
    pub input_sha256: Vec<String>,
    pub license: Option<String>,
    pub tool_version: Option<String>,
}

/// The complete v1 index.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Index {
    pub kind: Kind,
    pub semantic_sha256: [u8; 32],
    pub required_features: Vec<String>,
    /// 0 for single-file assets, otherwise 2..=8.
    pub volume_width: u64,
    pub volumes: Vec<VolumeDesc>,
    pub tensors: Vec<TensorDesc>,
    pub chunks: Vec<ChunkDesc>,
    pub dataset: Option<DatasetDesc>,
    pub weights: Option<WeightsDesc>,
    pub provenance: Option<Provenance>,
}

impl Index {
    /// Parse raw index bytes and run the full structural validation.
    /// `main_file_bytes` bounds chunks placed in the main file.
    pub fn parse(raw: &[u8], main_file_bytes: u64) -> Result<Self, DxoError> {
        let text = std::str::from_utf8(raw).map_err(|_| DxoError::BadJson("index is not valid UTF-8"))?;
        if text.starts_with('\u{feff}') {
            return Err(DxoError::BadJson("index must not carry a BOM"));
        }
        let value = crate::json::parse(text)?;
        let index = Self::from_json(&value)?;
        index.validate_with_floor(main_file_bytes, crate::shell::MAIN_HEADER_BYTES as u64 + raw.len() as u64)?;
        Ok(index)
    }

    fn from_json(v: &Json) -> Result<Self, DxoError> {
        v.deny_extra(&[
            "chunks",
            "dataset",
            "kind",
            "provenance",
            "required_features",
            "semantic_sha256",
            "tensors",
            "volume_width",
            "volumes",
            "weights",
        ])?;
        let kind = Kind::parse(v.get("kind")?.as_str()?)?;
        let semantic_sha256 = v.get("semantic_sha256")?.as_hex32()?;
        let required_features =
            v.get("required_features")?.as_arr()?.iter().map(|f| f.as_str().map(str::to_string)).collect::<Result<Vec<_>, _>>()?;
        let volume_width = v.get("volume_width")?.as_u64_str()?;
        let volumes = v
            .get("volumes")?
            .as_arr()?
            .iter()
            .map(|item| {
                item.deny_extra(&["file_bytes", "volume_index"])?;
                Ok(VolumeDesc { volume_index: item.get("volume_index")?.as_u64_str()?, file_bytes: item.get("file_bytes")?.as_u64_str()? })
            })
            .collect::<Result<Vec<_>, _>>()?;
        let tensors = v.get("tensors")?.as_arr()?.iter().map(Self::tensor_from_json).collect::<Result<Vec<_>, _>>()?;
        let chunks = v.get("chunks")?.as_arr()?.iter().map(Self::chunk_from_json).collect::<Result<Vec<_>, _>>()?;
        let dataset = match v.get_opt("dataset")? {
            Some(d) => Some(Self::dataset_from_json(d)?),
            None => None,
        };
        let weights = match v.get_opt("weights")? {
            Some(w) => Some(Self::weights_from_json(w)?),
            None => None,
        };
        let provenance = match v.get_opt("provenance")? {
            Some(p) => Some(Self::provenance_from_json(p)?),
            None => None,
        };
        Ok(Index { kind, semantic_sha256, required_features, volume_width, volumes, tensors, chunks, dataset, weights, provenance })
    }

    fn tensor_from_json(v: &Json) -> Result<TensorDesc, DxoError> {
        v.deny_extra(&["chunks", "dtype", "key", "layout", "logical_bytes", "logical_sha256", "shape"])?;
        if v.get("layout")?.as_str()? != "row-major-dense" {
            return Err(DxoError::UnsupportedLayout(v.get("layout")?.as_str()?.to_string()));
        }
        let shape = v.get("shape")?.as_arr()?.iter().map(Json::as_u64_str).collect::<Result<Vec<_>, _>>()?;
        let chunks = v
            .get("chunks")?
            .as_arr()?
            .iter()
            .map(|c| {
                c.deny_extra(&["chunk_id", "logical_offset"])?;
                Ok(ChunkRef { chunk_id: c.get("chunk_id")?.as_u64_str()?, logical_offset: c.get("logical_offset")?.as_u64_str()? })
            })
            .collect::<Result<Vec<_>, _>>()?;
        Ok(TensorDesc {
            key: v.get("key")?.as_str()?.to_string(),
            dtype: Dtype::parse(v.get("dtype")?.as_str()?)?,
            shape,
            logical_bytes: v.get("logical_bytes")?.as_u64_str()?,
            logical_sha256: v.get("logical_sha256")?.as_hex32()?,
            chunks,
        })
    }

    fn chunk_from_json(v: &Json) -> Result<ChunkDesc, DxoError> {
        v.deny_extra(&["chunk_id", "codec", "file_index", "logical_bytes", "offset", "stored_bytes", "stored_sha256"])?;
        if v.get("codec")?.as_str()? != "raw" {
            return Err(DxoError::UnsupportedCodec(v.get("codec")?.as_str()?.to_string()));
        }
        Ok(ChunkDesc {
            chunk_id: v.get("chunk_id")?.as_u64_str()?,
            file_index: v.get("file_index")?.as_u64_str()?,
            offset: v.get("offset")?.as_u64_str()?,
            stored_bytes: v.get("stored_bytes")?.as_u64_str()?,
            logical_bytes: v.get("logical_bytes")?.as_u64_str()?,
            stored_sha256: v.get("stored_sha256")?.as_hex32()?,
        })
    }

    fn dataset_from_json(v: &Json) -> Result<DatasetDesc, DxoError> {
        v.deny_extra(&["splits"])?;
        let splits = v
            .get("splits")?
            .as_arr()?
            .iter()
            .map(|s| {
                s.deny_extra(&["fields", "name", "sample_count"])?;
                let fields = s
                    .get("fields")?
                    .as_arr()?
                    .iter()
                    .map(|f| {
                        f.deny_extra(&["name", "role", "tensor_key"])?;
                        Ok(FieldDesc {
                            name: f.get("name")?.as_str()?.to_string(),
                            tensor_key: f.get("tensor_key")?.as_str()?.to_string(),
                            role: f.get("role")?.as_str()?.to_string(),
                        })
                    })
                    .collect::<Result<Vec<_>, _>>()?;
                Ok(SplitDesc { name: s.get("name")?.as_str()?.to_string(), sample_count: s.get("sample_count")?.as_u64_str()?, fields })
            })
            .collect::<Result<Vec<_>, _>>()?;
        Ok(DatasetDesc { splits })
    }

    fn weights_from_json(v: &Json) -> Result<WeightsDesc, DxoError> {
        v.deny_extra(&["architecture_id", "entries", "state_dict_contract"])?;
        let entries = v
            .get("entries")?
            .as_arr()?
            .iter()
            .map(|e| {
                e.deny_extra(&["role", "tensor_key"])?;
                Ok(WeightEntry { tensor_key: e.get("tensor_key")?.as_str()?.to_string(), role: e.get("role")?.as_str()?.to_string() })
            })
            .collect::<Result<Vec<_>, _>>()?;
        Ok(WeightsDesc {
            architecture_id: v.get("architecture_id")?.as_str()?.to_string(),
            state_dict_contract: v.get("state_dict_contract")?.as_str()?.to_string(),
            entries,
        })
    }

    fn provenance_from_json(v: &Json) -> Result<Provenance, DxoError> {
        v.deny_extra(&["input_sha256", "license", "revision", "tool_version", "upstream_uri"])?;
        let opt_str = |key: &str| -> Result<Option<String>, DxoError> {
            match v.get_opt(key)? {
                Some(j) => Ok(Some(j.as_str()?.to_string())),
                None => Ok(None),
            }
        };
        Ok(Provenance {
            upstream_uri: opt_str("upstream_uri")?,
            revision: opt_str("revision")?,
            input_sha256: v
                .get_opt("input_sha256")?
                .map(|a| a.as_arr().map(<[Json]>::to_vec))
                .transpose()?
                .map(|items| items.iter().map(|i| i.as_str().map(str::to_string)).collect::<Result<Vec<_>, _>>())
                .transpose()?
                .unwrap_or_default(),
            license: opt_str("license")?,
            tool_version: opt_str("tool_version")?,
        })
    }

    /// Full structural validation, including the semantic digest check.
    /// `main_data_floor` is the lowest offset a main-file chunk may use
    /// (header plus index bytes).
    pub fn validate(&self, main_file_bytes: u64) -> Result<(), DxoError> {
        self.validate_with_floor(main_file_bytes, crate::shell::MAIN_HEADER_BYTES as u64)
    }

    pub(crate) fn validate_with_floor(&self, main_file_bytes: u64, main_data_floor: u64) -> Result<(), DxoError> {
        if !self.required_features.is_empty() {
            return Err(DxoError::UnknownFeature(self.required_features[0].clone()));
        }
        if self.volume_width != 0 && !(2..=8).contains(&self.volume_width) {
            return Err(DxoError::Range("volume_width must be 0 or 2..=8"));
        }

        // Volumes: consecutive numeric indexes from 0, bounded by width.
        if self.volume_width == 0 {
            if !self.volumes.is_empty() {
                return Err(DxoError::Range("single-file asset declares volumes"));
            }
        }
        else {
            let capacity = 10u64.checked_pow(self.volume_width as u32).ok_or(DxoError::Range("volume width overflow"))?;
            if (self.volumes.len() as u64) > capacity {
                return Err(DxoError::Range("more volumes than volume_width allows"));
            }
            for (i, vol) in self.volumes.iter().enumerate() {
                if vol.volume_index != i as u64 {
                    return Err(DxoError::Range("volume indexes must run from 0 consecutively"));
                }
                if vol.file_bytes < crate::shell::VOLUME_HEADER_BYTES as u64 {
                    return Err(DxoError::Range("volume shorter than its header"));
                }
            }
        }

        // Chunks: unique ids, raw codec sizes, alignment, bounds, overlap.
        for c in &self.chunks {
            if c.stored_bytes == 0 || c.stored_bytes != c.logical_bytes {
                return Err(DxoError::Range("raw chunks must store exactly their logical bytes"));
            }
            if c.offset % CHUNK_ALIGN != 0 {
                return Err(DxoError::Unaligned(c.chunk_id));
            }
            let limit = if c.file_index == 0 {
                main_file_bytes
            }
            else {
                let vol = self.volumes.get((c.file_index - 1) as usize).ok_or(DxoError::Range("chunk references unknown volume"))?;
                vol.file_bytes
            };
            let end = c.offset.checked_add(c.stored_bytes).ok_or(DxoError::Range("chunk range overflow"))?;
            if end > limit {
                return Err(DxoError::Range("chunk extends beyond its file"));
            }
            if c.file_index == 0 && c.offset < main_data_floor {
                // The main file starts with header + index; data chunks live after both.
                return Err(DxoError::Range("main-file chunk overlaps the header or index"));
            }
        }
        let mut ids: Vec<u64> = self.chunks.iter().map(|c| c.chunk_id).collect();
        ids.sort_unstable();
        if ids.windows(2).any(|w| w[0] == w[1]) {
            return Err(DxoError::DuplicateChunkId(ids[0]));
        }
        for file in 0..=self.volumes.len() as u64 {
            let mut list: Vec<&ChunkDesc> = self.chunks.iter().filter(|c| c.file_index == file).collect();
            list.sort_by_key(|c| c.offset);
            for pair in list.windows(2) {
                if pair[0].offset + pair[0].stored_bytes > pair[1].offset {
                    return Err(DxoError::Overlap { file, a: pair[0].chunk_id, b: pair[1].chunk_id });
                }
            }
        }

        // Tensors: unique keys, shape/product consistency, exact coverage.
        let mut chunk_owner: Vec<Option<&str>> = vec![None; self.chunks.len()];
        let mut seen_keys: Vec<&str> = Vec::new();
        for t in &self.tensors {
            if seen_keys.contains(&t.key.as_str()) {
                return Err(DxoError::DuplicateTensorKey(t.key.clone()));
            }
            seen_keys.push(&t.key);
            let mut elems = 1u64;
            let mut zero_dim = false;
            for &d in &t.shape {
                if d == 0 {
                    zero_dim = true;
                }
                elems = elems.checked_mul(d).ok_or(DxoError::Range("shape product overflow"))?;
            }
            let logical = elems.checked_mul(t.dtype.elem_bytes()).ok_or(DxoError::Range("logical bytes overflow"))?;
            if zero_dim && logical != 0 {
                return Err(DxoError::Range("zero dimension must yield empty tensor"));
            }
            if logical != t.logical_bytes {
                return Err(DxoError::Coverage("shape and logical_bytes disagree"));
            }
            if t.logical_bytes == 0 {
                if !t.chunks.is_empty() {
                    return Err(DxoError::Coverage("empty tensor must have no chunks"));
                }
                continue;
            }
            if t.chunks.is_empty() {
                return Err(DxoError::Coverage("tensor without chunks"));
            }
            let mut cursor = 0u64;
            for r in &t.chunks {
                if r.logical_offset != cursor {
                    return Err(DxoError::Coverage("chunk references must tile from 0 without gaps"));
                }
                let c = self.chunk_by_id(r.chunk_id).ok_or(DxoError::Coverage("unknown chunk id referenced"))?;
                cursor = cursor.checked_add(c.logical_bytes).ok_or(DxoError::Range("coverage overflow"))?;
                let slot = &mut chunk_owner[self.chunk_index(r.chunk_id).unwrap()];
                if slot.is_some() {
                    return Err(DxoError::MultiOwnedChunk(r.chunk_id));
                }
                *slot = Some(&t.key);
            }
            if cursor != t.logical_bytes {
                return Err(DxoError::Coverage("chunks do not cover the tensor exactly"));
            }
        }
        for (i, owner) in chunk_owner.iter().enumerate() {
            if owner.is_none() {
                return Err(DxoError::UnreferencedChunk(self.chunks[i].chunk_id));
            }
        }

        // Kind description cross-checks.
        match self.kind {
            Kind::Dataset => {
                let d = self.dataset.as_ref().ok_or(DxoError::BadIndex("dataset kind requires a dataset description"))?;
                if self.weights.is_some() {
                    return Err(DxoError::BadIndex("dataset kind must not carry weights description"));
                }
                let mut split_names: Vec<&str> = Vec::new();
                for s in &d.splits {
                    if split_names.contains(&s.name.as_str()) {
                        return Err(DxoError::BadIndex("duplicate split name"));
                    }
                    split_names.push(&s.name);
                    let mut field_names: Vec<&str> = Vec::new();
                    for f in &s.fields {
                        if field_names.contains(&f.name.as_str()) {
                            return Err(DxoError::BadIndex("duplicate field name in split"));
                        }
                        field_names.push(&f.name);
                        let t = self.tensor_by_key(&f.tensor_key).ok_or(DxoError::BadIndex("field references unknown tensor"))?;
                        if t.shape.first().copied() != Some(s.sample_count) {
                            return Err(DxoError::Range("field tensor first dimension must equal sample_count"));
                        }
                    }
                }
            }
            Kind::Weights => {
                let w = self.weights.as_ref().ok_or(DxoError::BadIndex("weights kind requires a weights description"))?;
                if self.dataset.is_some() {
                    return Err(DxoError::BadIndex("weights kind must not carry dataset description"));
                }
                let mut referenced: Vec<&str> = Vec::new();
                for e in &w.entries {
                    if referenced.contains(&e.tensor_key.as_str()) {
                        return Err(DxoError::BadIndex("duplicate weights entry"));
                    }
                    if self.tensor_by_key(&e.tensor_key).is_none() {
                        return Err(DxoError::BadIndex("weights entry references unknown tensor"));
                    }
                    referenced.push(&e.tensor_key);
                }
                for t in &self.tensors {
                    if !referenced.contains(&t.key.as_str()) {
                        return Err(DxoError::BadIndex("tensor not referenced by weights description"));
                    }
                }
            }
        }

        // Semantic digest: recomputed from the logical description only.
        let recomputed = self.compute_semantic_sha256();
        if recomputed != self.semantic_sha256 {
            return Err(DxoError::SemanticMismatch { declared: encode_hex32(&self.semantic_sha256) });
        }
        Ok(())
    }

    pub(crate) fn chunk_by_id(&self, id: u64) -> Option<&ChunkDesc> {
        self.chunks.iter().find(|c| c.chunk_id == id)
    }

    pub(crate) fn chunk_index(&self, id: u64) -> Option<usize> {
        self.chunks.iter().position(|c| c.chunk_id == id)
    }

    pub(crate) fn tensor_by_key(&self, key: &str) -> Option<&TensorDesc> {
        self.tensors.iter().find(|t| t.key == key)
    }

    /// SHA-256 over the RFC 8785 canonical semantic description:
    /// kind, kind description and key-sorted logical tensor descriptions.
    pub fn compute_semantic_sha256(&self) -> [u8; 32] {
        let mut tensors: Vec<&TensorDesc> = self.tensors.iter().collect();
        tensors.sort_by(|a, b| a.key.as_bytes().cmp(b.key.as_bytes()));
        let tensor_items: Vec<Json> = tensors
            .iter()
            .map(|t| {
                obj([
                    ("dtype", jstr(t.dtype.as_str())),
                    ("key", jstr(t.key.clone())),
                    ("layout", jstr("row-major-dense")),
                    ("logical_bytes", ju64(t.logical_bytes)),
                    ("logical_sha256", jstr(encode_hex32(&t.logical_sha256))),
                    ("shape", jarr(t.shape.iter().copied().map(ju64).collect())),
                ])
            })
            .collect();
        let mut pairs: Vec<(&str, Json)> = vec![("kind", jstr(self.kind.as_str())), ("tensors", jarr(tensor_items))];
        match self.kind {
            Kind::Dataset => pairs.push(("dataset", dataset_json(self.dataset.as_ref().expect("validated")))),
            Kind::Weights => pairs.push(("weights", weights_json(self.weights.as_ref().expect("validated")))),
        }
        pairs.sort_by(|a, b| a.0.as_bytes().cmp(b.0.as_bytes()));
        let value = Json::Obj(pairs.into_iter().map(|(k, v)| (k.to_string(), v)).collect());
        let digest = Sha256::digest(canonical_bytes(&value));
        let mut out = [0u8; 32];
        out.copy_from_slice(&digest);
        out
    }

    /// Canonical index bytes (RFC 8785) for embedding into a main file.
    pub fn canonical_bytes(&self) -> Vec<u8> {
        canonical_bytes(&self.to_json())
    }

    pub(crate) fn to_json(&self) -> Json {
        let volumes: Vec<Json> =
            self.volumes.iter().map(|v| obj([("file_bytes", ju64(v.file_bytes)), ("volume_index", ju64(v.volume_index))])).collect();
        let chunks: Vec<Json> = self
            .chunks
            .iter()
            .map(|c| {
                obj([
                    ("chunk_id", ju64(c.chunk_id)),
                    ("codec", jstr("raw")),
                    ("file_index", ju64(c.file_index)),
                    ("logical_bytes", ju64(c.logical_bytes)),
                    ("offset", ju64(c.offset)),
                    ("stored_bytes", ju64(c.stored_bytes)),
                    ("stored_sha256", jstr(encode_hex32(&c.stored_sha256))),
                ])
            })
            .collect();
        let tensors: Vec<Json> = self
            .tensors
            .iter()
            .map(|t| {
                obj([
                    (
                        "chunks",
                        jarr(
                            t.chunks
                                .iter()
                                .map(|r| obj([("chunk_id", ju64(r.chunk_id)), ("logical_offset", ju64(r.logical_offset))]))
                                .collect(),
                        ),
                    ),
                    ("dtype", jstr(t.dtype.as_str())),
                    ("key", jstr(t.key.clone())),
                    ("layout", jstr("row-major-dense")),
                    ("logical_bytes", ju64(t.logical_bytes)),
                    ("logical_sha256", jstr(encode_hex32(&t.logical_sha256))),
                    ("shape", jarr(t.shape.iter().copied().map(ju64).collect())),
                ])
            })
            .collect();
        let mut pairs: Vec<(&str, Json)> = vec![
            ("chunks", jarr(chunks)),
            ("kind", jstr(self.kind.as_str())),
            ("required_features", jarr(self.required_features.iter().cloned().map(jstr).collect())),
            ("semantic_sha256", jstr(encode_hex32(&self.semantic_sha256))),
            ("tensors", jarr(tensors)),
            ("volume_width", ju64(self.volume_width)),
            ("volumes", jarr(volumes)),
        ];
        if let Some(d) = &self.dataset {
            pairs.push(("dataset", dataset_json(d)));
        }
        if let Some(w) = &self.weights {
            pairs.push(("weights", weights_json(w)));
        }
        if let Some(p) = &self.provenance {
            pairs.push(("provenance", provenance_json(p)));
        }
        Json::Obj(pairs.into_iter().map(|(k, v)| (k.to_string(), v)).collect())
    }
}

pub(crate) fn dataset_json(d: &DatasetDesc) -> Json {
    Json::Obj(vec![(
        "splits".to_string(),
        jarr(
            d.splits
                .iter()
                .map(|s| {
                    obj([
                        (
                            "fields",
                            jarr(
                                s.fields
                                    .iter()
                                    .map(|f| {
                                        obj([
                                            ("name", jstr(f.name.clone())),
                                            ("role", jstr(f.role.clone())),
                                            ("tensor_key", jstr(f.tensor_key.clone())),
                                        ])
                                    })
                                    .collect(),
                            ),
                        ),
                        ("name", jstr(s.name.clone())),
                        ("sample_count", ju64(s.sample_count)),
                    ])
                })
                .collect(),
        ),
    )])
}

pub(crate) fn weights_json(w: &WeightsDesc) -> Json {
    obj([
        ("architecture_id", jstr(w.architecture_id.clone())),
        (
            "entries",
            jarr(w.entries.iter().map(|e| obj([("role", jstr(e.role.clone())), ("tensor_key", jstr(e.tensor_key.clone()))])).collect()),
        ),
        ("state_dict_contract", jstr(w.state_dict_contract.clone())),
    ])
}

pub(crate) fn provenance_json(p: &Provenance) -> Json {
    let mut pairs: Vec<(String, Json)> = Vec::new();
    if let Some(u) = &p.upstream_uri {
        pairs.push(("upstream_uri".into(), jstr(u.clone())));
    }
    if let Some(r) = &p.revision {
        pairs.push(("revision".into(), jstr(r.clone())));
    }
    if !p.input_sha256.is_empty() {
        pairs.push(("input_sha256".into(), jarr(p.input_sha256.iter().cloned().map(jstr).collect())));
    }
    if let Some(l) = &p.license {
        pairs.push(("license".into(), jstr(l.clone())));
    }
    if let Some(t) = &p.tool_version {
        pairs.push(("tool_version".into(), jstr(t.clone())));
    }
    Json::Obj(pairs)
}

/// SHA-256 of arbitrary bytes, lowercase hex.
pub fn sha256_hex(data: &[u8]) -> String {
    encode_hex32(&sha256_bytes(data))
}

/// SHA-256 of arbitrary bytes.
pub fn sha256_bytes(data: &[u8]) -> [u8; 32] {
    let digest = Sha256::digest(data);
    let mut out = [0u8; 32];
    out.copy_from_slice(&digest);
    out
}

/// Expose hex decoding for tests and adapters.
pub fn hex_to_bytes32(s: &str) -> Option<[u8; 32]> {
    decode_hex32(s)
}
