use git_core::{GitError, ObjectFormat, ObjectId, Result};
use std::fs;
use std::path::{Path, PathBuf};
use std::str::FromStr;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum ObjectType {
    Blob,
    Tree,
    Commit,
    Tag,
}

impl ObjectType {
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Blob => "blob",
            Self::Tree => "tree",
            Self::Commit => "commit",
            Self::Tag => "tag",
        }
    }
}

impl FromStr for ObjectType {
    type Err = GitError;

    fn from_str(value: &str) -> Result<Self> {
        match value {
            "blob" => Ok(Self::Blob),
            "tree" => Ok(Self::Tree),
            "commit" => Ok(Self::Commit),
            "tag" => Ok(Self::Tag),
            other => Err(GitError::InvalidObject(format!(
                "unknown object type {other}"
            ))),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct EncodedObject {
    pub object_type: ObjectType,
    pub body: Vec<u8>,
}

impl EncodedObject {
    pub fn new(object_type: ObjectType, body: impl Into<Vec<u8>>) -> Self {
        Self {
            object_type,
            body: body.into(),
        }
    }

    pub fn framed_bytes(&self) -> Vec<u8> {
        let mut out = Vec::with_capacity(self.body.len() + 32);
        out.extend_from_slice(self.object_type.as_str().as_bytes());
        out.push(b' ');
        out.extend_from_slice(self.body.len().to_string().as_bytes());
        out.push(0);
        out.extend_from_slice(&self.body);
        out
    }

    pub fn object_id(&self, format: ObjectFormat) -> Result<ObjectId> {
        git_core::object_id_for_bytes(format, self.object_type.as_str(), &self.body)
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Tree {
    pub entries: Vec<TreeEntry>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TreeEntry {
    pub mode: u32,
    pub name: Vec<u8>,
    pub oid: ObjectId,
}

impl Tree {
    pub fn parse(format: ObjectFormat, bytes: &[u8]) -> Result<Self> {
        let mut offset = 0usize;
        let mut entries = Vec::new();
        while offset < bytes.len() {
            let mode_start = offset;
            while bytes.get(offset).copied() != Some(b' ') {
                offset += 1;
                if offset >= bytes.len() {
                    return Err(GitError::InvalidFormat("unterminated tree mode".into()));
                }
            }
            let mode_text = std::str::from_utf8(&bytes[mode_start..offset])
                .map_err(|err| GitError::InvalidFormat(err.to_string()))?;
            let mode = u32::from_str_radix(mode_text, 8)
                .map_err(|_| GitError::InvalidFormat("invalid tree mode".into()))?;
            offset += 1;
            let name_start = offset;
            while bytes.get(offset).copied() != Some(0) {
                offset += 1;
                if offset >= bytes.len() {
                    return Err(GitError::InvalidFormat("unterminated tree path".into()));
                }
            }
            if offset == name_start {
                return Err(GitError::InvalidFormat("empty tree path".into()));
            }
            let name = bytes[name_start..offset].to_vec();
            offset += 1;
            let oid_end = offset
                .checked_add(format.raw_len())
                .ok_or_else(|| GitError::InvalidFormat("tree oid overflow".into()))?;
            if oid_end > bytes.len() {
                return Err(GitError::InvalidFormat("truncated tree object id".into()));
            }
            let oid = ObjectId::from_raw(format, &bytes[offset..oid_end])?;
            offset = oid_end;
            entries.push(TreeEntry { mode, name, oid });
        }
        Ok(Self { entries })
    }

    pub fn write(&self) -> Vec<u8> {
        let mut out = Vec::new();
        for entry in &self.entries {
            out.extend_from_slice(format!("{:o}", entry.mode).as_bytes());
            out.push(b' ');
            out.extend_from_slice(&entry.name);
            out.push(0);
            out.extend_from_slice(entry.oid.as_bytes());
        }
        out
    }
}

pub fn tree_entry_object_type(mode: u32) -> ObjectType {
    match mode {
        0o040000 => ObjectType::Tree,
        _ => ObjectType::Blob,
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Commit {
    pub tree: ObjectId,
    pub parents: Vec<ObjectId>,
    pub author: Vec<u8>,
    pub committer: Vec<u8>,
    pub encoding: Option<Vec<u8>>,
    pub message: Vec<u8>,
}

impl Commit {
    pub fn parse(format: ObjectFormat, bytes: &[u8]) -> Result<Self> {
        let split = bytes
            .windows(2)
            .position(|window| window == b"\n\n")
            .ok_or_else(|| GitError::InvalidObject("commit missing message separator".into()))?;
        let headers = std::str::from_utf8(&bytes[..split])
            .map_err(|err| GitError::InvalidObject(err.to_string()))?;
        let mut tree = None;
        let mut parents = Vec::new();
        let mut author = None;
        let mut committer = None;
        let mut encoding = None;
        for line in headers.lines() {
            if let Some(value) = line.strip_prefix("tree ") {
                tree = Some(ObjectId::from_hex(format, value)?);
            } else if let Some(value) = line.strip_prefix("parent ") {
                parents.push(ObjectId::from_hex(format, value)?);
            } else if let Some(value) = line.strip_prefix("author ") {
                author = Some(value.as_bytes().to_vec());
            } else if let Some(value) = line.strip_prefix("committer ") {
                committer = Some(value.as_bytes().to_vec());
            } else if let Some(value) = line.strip_prefix("encoding ") {
                encoding = Some(value.as_bytes().to_vec());
            }
        }
        Ok(Self {
            tree: tree.ok_or_else(|| GitError::InvalidObject("commit missing tree".into()))?,
            parents,
            author: author
                .ok_or_else(|| GitError::InvalidObject("commit missing author".into()))?,
            committer: committer
                .ok_or_else(|| GitError::InvalidObject("commit missing committer".into()))?,
            encoding,
            message: bytes[split + 2..].to_vec(),
        })
    }

    pub fn write(&self) -> Vec<u8> {
        let mut out = Vec::new();
        out.extend_from_slice(format!("tree {}\n", self.tree).as_bytes());
        for parent in &self.parents {
            out.extend_from_slice(format!("parent {parent}\n").as_bytes());
        }
        out.extend_from_slice(b"author ");
        out.extend_from_slice(&self.author);
        out.push(b'\n');
        out.extend_from_slice(b"committer ");
        out.extend_from_slice(&self.committer);
        if let Some(encoding) = &self.encoding {
            out.extend_from_slice(b"\nencoding ");
            out.extend_from_slice(encoding);
        }
        out.extend_from_slice(b"\n\n");
        out.extend_from_slice(&self.message);
        out
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Tag {
    pub object: ObjectId,
    pub object_type: ObjectType,
    pub name: Vec<u8>,
    pub tagger: Option<Vec<u8>>,
    pub message: Vec<u8>,
}

impl Tag {
    pub fn parse(format: ObjectFormat, bytes: &[u8]) -> Result<Self> {
        let split = bytes
            .windows(2)
            .position(|window| window == b"\n\n")
            .ok_or_else(|| GitError::InvalidObject("tag missing message separator".into()))?;
        let headers = std::str::from_utf8(&bytes[..split])
            .map_err(|err| GitError::InvalidObject(err.to_string()))?;
        let mut object = None;
        let mut object_type = None;
        let mut name = None;
        let mut tagger = None;
        for line in headers.lines() {
            if let Some(value) = line.strip_prefix("object ") {
                object = Some(ObjectId::from_hex(format, value)?);
            } else if let Some(value) = line.strip_prefix("type ") {
                object_type = Some(value.parse()?);
            } else if let Some(value) = line.strip_prefix("tag ") {
                name = Some(value.as_bytes().to_vec());
            } else if let Some(value) = line.strip_prefix("tagger ") {
                tagger = Some(value.as_bytes().to_vec());
            }
        }
        Ok(Self {
            object: object.ok_or_else(|| GitError::InvalidObject("tag missing object".into()))?,
            object_type: object_type
                .ok_or_else(|| GitError::InvalidObject("tag missing type".into()))?,
            name: name.ok_or_else(|| GitError::InvalidObject("tag missing name".into()))?,
            tagger,
            message: bytes[split + 2..].to_vec(),
        })
    }

    pub fn write(&self) -> Vec<u8> {
        let mut out = Vec::new();
        out.extend_from_slice(format!("object {}\n", self.object).as_bytes());
        out.extend_from_slice(format!("type {}\n", self.object_type.as_str()).as_bytes());
        out.extend_from_slice(b"tag ");
        out.extend_from_slice(&self.name);
        out.push(b'\n');
        if let Some(tagger) = &self.tagger {
            out.extend_from_slice(b"tagger ");
            out.extend_from_slice(tagger);
            out.push(b'\n');
        }
        out.push(b'\n');
        out.extend_from_slice(&self.message);
        out
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CommitGraph {
    pub version: u8,
    pub format: ObjectFormat,
    pub base_graph_count: u8,
    pub fanout: [u32; 256],
    pub commits: Vec<CommitGraphEntry>,
    pub chunks: Vec<CommitGraphChunk>,
    pub base_graphs: Vec<ObjectId>,
    pub bloom_filters: Option<CommitGraphBloomFilters>,
    pub checksum: ObjectId,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CommitGraphEntry {
    pub oid: ObjectId,
    pub tree: ObjectId,
    pub parents: Vec<u32>,
    pub generation: u32,
    pub commit_time: u64,
    pub corrected_commit_date_offset: Option<u64>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CommitGraphChunk {
    pub id: [u8; 4],
    pub offset: u64,
    pub len: u64,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CommitGraphBloomFilters {
    pub hash_version: u32,
    pub hash_count: u32,
    pub bits_per_entry: u32,
    pub filters: Vec<Vec<u8>>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CommitGraphWriteEntry {
    pub oid: ObjectId,
    pub tree: ObjectId,
    pub parents: Vec<ObjectId>,
    pub generation: u32,
    pub commit_time: u64,
}

impl CommitGraph {
    pub fn write(format: ObjectFormat, entries: &[CommitGraphWriteEntry]) -> Result<Vec<u8>> {
        let mut entries = entries.to_vec();
        entries.sort_by(|left, right| left.oid.as_bytes().cmp(right.oid.as_bytes()));
        validate_commit_graph_write_entries(format, &entries)?;
        let object_ids = entries
            .iter()
            .map(|entry| entry.oid.clone())
            .collect::<Vec<_>>();
        let (cdat, edge) = write_commit_graph_commit_data(&entries)?;
        let mut chunks = vec![
            (*b"OIDF", write_commit_graph_fanout(&object_ids)?),
            (*b"OIDL", write_commit_graph_oid_lookup(&object_ids)),
            (*b"CDAT", cdat),
        ];
        if !edge.is_empty() {
            chunks.push((*b"EDGE", edge));
        }
        write_commit_graph_chunks(format, 0, &chunks)
    }

    pub fn parse(bytes: &[u8], format: ObjectFormat) -> Result<Self> {
        let hash_len = format.raw_len();
        if bytes.len() < 8 + 12 + hash_len {
            return Err(GitError::InvalidFormat(
                "commit-graph file too short".into(),
            ));
        }
        if &bytes[..4] != b"CGPH" {
            return Err(GitError::InvalidFormat(
                "missing commit-graph signature".into(),
            ));
        }
        let version = bytes[4];
        if version != 1 {
            return Err(GitError::Unsupported(format!(
                "commit-graph version {version}"
            )));
        }
        let hash_id = bytes[5];
        if u32::from(hash_id) != hash_function_id(format) {
            return Err(GitError::InvalidFormat(format!(
                "commit-graph hash id {hash_id} does not match {}",
                format.name()
            )));
        }
        let chunk_count = bytes[6] as usize;
        let base_graph_count = bytes[7];
        let lookup_len = (chunk_count + 1)
            .checked_mul(12)
            .ok_or_else(|| GitError::InvalidFormat("commit-graph lookup overflow".into()))?;
        let data_start = 8usize
            .checked_add(lookup_len)
            .ok_or_else(|| GitError::InvalidFormat("commit-graph lookup overflow".into()))?;
        let checksum_offset = bytes.len() - hash_len;
        if data_start > checksum_offset {
            return Err(GitError::InvalidFormat(
                "truncated commit-graph chunk lookup".into(),
            ));
        }

        let actual_checksum = git_core::digest_bytes(format, &bytes[..checksum_offset])?;
        let checksum = ObjectId::from_raw(format, &bytes[checksum_offset..])?;
        if actual_checksum != checksum {
            return Err(GitError::InvalidFormat(format!(
                "commit-graph checksum mismatch: expected {checksum}, got {actual_checksum}"
            )));
        }

        let mut lookup = Vec::with_capacity(chunk_count + 1);
        let mut offset = 8usize;
        for _ in 0..=chunk_count {
            let id = [
                bytes[offset],
                bytes[offset + 1],
                bytes[offset + 2],
                bytes[offset + 3],
            ];
            let chunk_offset = u64_be(&bytes[offset + 4..offset + 12]);
            lookup.push((id, chunk_offset));
            offset += 12;
        }
        let Some((terminator_id, terminator_offset)) = lookup.last().copied() else {
            return Err(GitError::InvalidFormat(
                "commit-graph chunk lookup is empty".into(),
            ));
        };
        if terminator_id != [0, 0, 0, 0] {
            return Err(GitError::InvalidFormat(
                "commit-graph chunk lookup missing terminator".into(),
            ));
        }
        if terminator_offset != checksum_offset as u64 {
            return Err(GitError::InvalidFormat(
                "commit-graph terminator does not point at checksum".into(),
            ));
        }

        let mut chunks = Vec::with_capacity(chunk_count);
        let mut previous_offset = data_start as u64;
        for pair in lookup.windows(2) {
            let (id, chunk_offset) = pair[0];
            let (_next_id, next_offset) = pair[1];
            if id == [0, 0, 0, 0] {
                return Err(GitError::InvalidFormat(
                    "commit-graph chunk id is zero before terminator".into(),
                ));
            }
            if chunks.iter().any(|chunk: &CommitGraphChunk| chunk.id == id) {
                return Err(GitError::InvalidFormat(
                    "commit-graph chunk id is duplicated".into(),
                ));
            }
            if chunk_offset < data_start as u64 || chunk_offset < previous_offset {
                return Err(GitError::InvalidFormat(
                    "commit-graph chunk offsets are not monotonic".into(),
                ));
            }
            if next_offset < chunk_offset || next_offset > checksum_offset as u64 {
                return Err(GitError::InvalidFormat(
                    "commit-graph chunk length is invalid".into(),
                ));
            }
            chunks.push(CommitGraphChunk {
                id,
                offset: chunk_offset,
                len: next_offset - chunk_offset,
            });
            previous_offset = chunk_offset;
        }

        let (fanout, commit_count) = parse_commit_graph_fanout(bytes, &chunks)?;
        let oids = parse_commit_graph_oids(bytes, &chunks, format, commit_count, &fanout)?;
        let mut commits = parse_commit_graph_commit_data(bytes, &chunks, format, oids)?;
        apply_commit_graph_generation_data(bytes, &chunks, &mut commits)?;
        let bloom_filters = parse_commit_graph_bloom_filters(bytes, &chunks, commits.len())?;
        let base_graphs =
            parse_commit_graph_base_graphs(bytes, &chunks, format, base_graph_count as usize)?;

        Ok(Self {
            version,
            format,
            base_graph_count,
            fanout,
            commits,
            chunks,
            base_graphs,
            bloom_filters,
            checksum,
        })
    }

    pub fn find(&self, oid: &ObjectId) -> Option<&CommitGraphEntry> {
        self.commits
            .binary_search_by(|entry| entry.oid.as_bytes().cmp(oid.as_bytes()))
            .ok()
            .map(|idx| &self.commits[idx])
    }
}

fn validate_commit_graph_write_entries(
    format: ObjectFormat,
    entries: &[CommitGraphWriteEntry],
) -> Result<()> {
    let mut previous_oid: Option<&ObjectId> = None;
    for entry in entries {
        if entry.oid.format() != format
            || entry.tree.format() != format
            || entry.parents.iter().any(|parent| parent.format() != format)
        {
            return Err(GitError::InvalidObjectId(
                "commit-graph entry format does not match graph format".into(),
            ));
        }
        if let Some(previous) = previous_oid
            && previous.as_bytes() == entry.oid.as_bytes()
        {
            return Err(GitError::InvalidFormat(
                "commit-graph contains duplicate object ids".into(),
            ));
        }
        if entry.generation >= (1 << 30) {
            return Err(GitError::InvalidFormat(
                "commit-graph generation is too large".into(),
            ));
        }
        previous_oid = Some(&entry.oid);
    }
    Ok(())
}

fn write_commit_graph_fanout(object_ids: &[ObjectId]) -> Result<Vec<u8>> {
    let mut counts = [0u32; 256];
    for oid in object_ids {
        let first = oid.as_bytes()[0] as usize;
        counts[first] = counts[first]
            .checked_add(1)
            .ok_or_else(|| GitError::InvalidFormat("commit-graph fanout overflow".into()))?;
    }
    let mut running = 0u32;
    let mut out = Vec::with_capacity(256 * 4);
    for count in counts {
        running = running
            .checked_add(count)
            .ok_or_else(|| GitError::InvalidFormat("commit-graph fanout overflow".into()))?;
        out.extend_from_slice(&running.to_be_bytes());
    }
    Ok(out)
}

fn write_commit_graph_oid_lookup(object_ids: &[ObjectId]) -> Vec<u8> {
    let mut out = Vec::new();
    for oid in object_ids {
        out.extend_from_slice(oid.as_bytes());
    }
    out
}

fn write_commit_graph_commit_data(entries: &[CommitGraphWriteEntry]) -> Result<(Vec<u8>, Vec<u8>)> {
    let mut cdat = Vec::new();
    let mut edge = Vec::new();
    for entry in entries {
        cdat.extend_from_slice(entry.tree.as_bytes());
        let parent_positions = entry
            .parents
            .iter()
            .map(|parent| {
                entries
                    .binary_search_by(|entry| entry.oid.as_bytes().cmp(parent.as_bytes()))
                    .map(|idx| idx as u32)
                    .map_err(|_| {
                        GitError::InvalidFormat(format!(
                            "commit-graph parent {parent} is missing from graph"
                        ))
                    })
            })
            .collect::<Result<Vec<_>>>()?;
        let first_parent = parent_positions
            .first()
            .copied()
            .unwrap_or(COMMIT_GRAPH_PARENT_NONE);
        let second_parent = match parent_positions.len() {
            0 | 1 => COMMIT_GRAPH_PARENT_NONE,
            2 => parent_positions[1],
            _ => {
                let edge_start = u32::try_from(edge.len() / 4).map_err(|_| {
                    GitError::InvalidFormat("commit-graph EDGE chunk overflow".into())
                })?;
                for (idx, parent) in parent_positions[1..].iter().enumerate() {
                    let mut value = *parent;
                    if idx == parent_positions.len() - 2 {
                        value |= COMMIT_GRAPH_EXTRA_EDGE;
                    }
                    edge.extend_from_slice(&value.to_be_bytes());
                }
                COMMIT_GRAPH_EXTRA_EDGE | edge_start
            }
        };
        cdat.extend_from_slice(&first_parent.to_be_bytes());
        cdat.extend_from_slice(&second_parent.to_be_bytes());
        let generation_and_time_high =
            (entry.generation << 2) | (((entry.commit_time >> 32) as u32) & 0x3);
        cdat.extend_from_slice(&generation_and_time_high.to_be_bytes());
        cdat.extend_from_slice(&(entry.commit_time as u32).to_be_bytes());
    }
    Ok((cdat, edge))
}

fn write_commit_graph_chunks(
    format: ObjectFormat,
    base_graph_count: u8,
    chunks: &[([u8; 4], Vec<u8>)],
) -> Result<Vec<u8>> {
    if chunks.len() > u8::MAX as usize {
        return Err(GitError::InvalidFormat(
            "too many commit-graph chunks".into(),
        ));
    }
    let lookup_len = (chunks.len() + 1)
        .checked_mul(12)
        .ok_or_else(|| GitError::InvalidFormat("commit-graph lookup overflow".into()))?;
    let mut out = Vec::new();
    out.extend_from_slice(b"CGPH");
    out.push(1);
    out.push(hash_function_id(format) as u8);
    out.push(chunks.len() as u8);
    out.push(base_graph_count);
    let mut chunk_offset = (8usize)
        .checked_add(lookup_len)
        .ok_or_else(|| GitError::InvalidFormat("commit-graph lookup overflow".into()))?
        as u64;
    for (id, data) in chunks {
        out.extend_from_slice(id);
        out.extend_from_slice(&chunk_offset.to_be_bytes());
        chunk_offset = chunk_offset
            .checked_add(data.len() as u64)
            .ok_or_else(|| GitError::InvalidFormat("commit-graph size overflow".into()))?;
    }
    out.extend_from_slice(&[0, 0, 0, 0]);
    out.extend_from_slice(&chunk_offset.to_be_bytes());
    for (_id, data) in chunks {
        out.extend_from_slice(data);
    }
    let checksum = git_core::digest_bytes(format, &out)?;
    out.extend_from_slice(checksum.as_bytes());
    Ok(out)
}

const COMMIT_GRAPH_PARENT_NONE: u32 = 0x7000_0000;
const COMMIT_GRAPH_EXTRA_EDGE: u32 = 0x8000_0000;
const COMMIT_GRAPH_EXTRA_EDGE_MASK: u32 = 0x7fff_ffff;

fn parse_commit_graph_fanout(
    bytes: &[u8],
    chunks: &[CommitGraphChunk],
) -> Result<([u32; 256], usize)> {
    let data = commit_graph_chunk_data(bytes, chunks, *b"OIDF", true)?
        .ok_or_else(|| GitError::InvalidFormat("commit-graph missing OIDF chunk".into()))?;
    if data.len() != 256 * 4 {
        return Err(GitError::InvalidFormat(
            "commit-graph OIDF chunk has invalid length".into(),
        ));
    }
    let mut fanout = [0u32; 256];
    let mut previous = 0u32;
    for (idx, slot) in fanout.iter_mut().enumerate() {
        let start = idx * 4;
        *slot = u32_be(&data[start..start + 4]);
        if *slot < previous {
            return Err(GitError::InvalidFormat(
                "commit-graph OIDF fanout is not monotonic".into(),
            ));
        }
        previous = *slot;
    }
    Ok((fanout, fanout[255] as usize))
}

fn parse_commit_graph_oids(
    bytes: &[u8],
    chunks: &[CommitGraphChunk],
    format: ObjectFormat,
    commit_count: usize,
    fanout: &[u32; 256],
) -> Result<Vec<ObjectId>> {
    let data = commit_graph_chunk_data(bytes, chunks, *b"OIDL", true)?
        .ok_or_else(|| GitError::InvalidFormat("commit-graph missing OIDL chunk".into()))?;
    let expected_len = commit_count
        .checked_mul(format.raw_len())
        .ok_or_else(|| GitError::InvalidFormat("commit-graph OIDL chunk overflow".into()))?;
    if data.len() != expected_len {
        return Err(GitError::InvalidFormat(
            "commit-graph OIDL chunk has invalid length".into(),
        ));
    }

    let mut oids = Vec::with_capacity(commit_count);
    let mut counts = [0u32; 256];
    let mut previous_oid: Option<ObjectId> = None;
    for idx in 0..commit_count {
        let start = idx * format.raw_len();
        let oid = ObjectId::from_raw(format, &data[start..start + format.raw_len()])?;
        if let Some(previous) = &previous_oid
            && previous.as_bytes() >= oid.as_bytes()
        {
            return Err(GitError::InvalidFormat(
                "commit-graph OIDL object ids are not strictly sorted".into(),
            ));
        }
        counts[oid.as_bytes()[0] as usize] = counts[oid.as_bytes()[0] as usize]
            .checked_add(1)
            .ok_or_else(|| GitError::InvalidFormat("commit-graph fanout overflow".into()))?;
        previous_oid = Some(oid.clone());
        oids.push(oid);
    }

    let mut running = 0u32;
    for (idx, count) in counts.iter().enumerate() {
        running = running
            .checked_add(*count)
            .ok_or_else(|| GitError::InvalidFormat("commit-graph fanout overflow".into()))?;
        if fanout[idx] != running {
            return Err(GitError::InvalidFormat(
                "commit-graph OIDF fanout does not match OIDL".into(),
            ));
        }
    }
    Ok(oids)
}

fn parse_commit_graph_commit_data(
    bytes: &[u8],
    chunks: &[CommitGraphChunk],
    format: ObjectFormat,
    oids: Vec<ObjectId>,
) -> Result<Vec<CommitGraphEntry>> {
    let data = commit_graph_chunk_data(bytes, chunks, *b"CDAT", true)?
        .ok_or_else(|| GitError::InvalidFormat("commit-graph missing CDAT chunk".into()))?;
    let entry_len = format
        .raw_len()
        .checked_add(16)
        .ok_or_else(|| GitError::InvalidFormat("commit-graph CDAT entry overflow".into()))?;
    let expected_len = oids
        .len()
        .checked_mul(entry_len)
        .ok_or_else(|| GitError::InvalidFormat("commit-graph CDAT chunk overflow".into()))?;
    if data.len() != expected_len {
        return Err(GitError::InvalidFormat(
            "commit-graph CDAT chunk has invalid length".into(),
        ));
    }
    let extra_edges = commit_graph_chunk_data(bytes, chunks, *b"EDGE", false)?;
    if let Some(extra_edges) = extra_edges
        && extra_edges.len() % 4 != 0
    {
        return Err(GitError::InvalidFormat(
            "commit-graph EDGE chunk has invalid length".into(),
        ));
    }

    let commit_count = oids.len();
    let mut entries = Vec::with_capacity(commit_count);
    for (idx, oid) in oids.into_iter().enumerate() {
        let start = idx * entry_len;
        let tree = ObjectId::from_raw(format, &data[start..start + format.raw_len()])?;
        let parent_one = u32_be(&data[start + format.raw_len()..start + format.raw_len() + 4]);
        let parent_two = u32_be(&data[start + format.raw_len() + 4..start + format.raw_len() + 8]);
        let generation_and_time_high =
            u32_be(&data[start + format.raw_len() + 8..start + format.raw_len() + 12]);
        let time_low = u32_be(&data[start + format.raw_len() + 12..start + entry_len]);
        let generation = generation_and_time_high >> 2;
        let commit_time = (u64::from(generation_and_time_high & 0x3) << 32) | u64::from(time_low);
        let parents = commit_graph_parents(parent_one, parent_two, extra_edges, commit_count)?;
        entries.push(CommitGraphEntry {
            oid,
            tree,
            parents,
            generation,
            commit_time,
            corrected_commit_date_offset: None,
        });
    }
    Ok(entries)
}

fn apply_commit_graph_generation_data(
    bytes: &[u8],
    chunks: &[CommitGraphChunk],
    commits: &mut [CommitGraphEntry],
) -> Result<()> {
    let data = commit_graph_chunk_data(bytes, chunks, *b"GDA2", false)?;
    let overflow = commit_graph_chunk_data(bytes, chunks, *b"GDO2", false)?;
    let Some(data) = data else {
        if overflow.is_some() {
            return Err(GitError::InvalidFormat(
                "commit-graph GDO2 chunk exists without GDA2 chunk".into(),
            ));
        }
        return Ok(());
    };
    let expected_len = commits
        .len()
        .checked_mul(4)
        .ok_or_else(|| GitError::InvalidFormat("commit-graph GDA2 chunk overflow".into()))?;
    if data.len() != expected_len {
        return Err(GitError::InvalidFormat(
            "commit-graph GDA2 chunk has invalid length".into(),
        ));
    }
    if let Some(overflow) = overflow
        && overflow.len() % 8 != 0
    {
        return Err(GitError::InvalidFormat(
            "commit-graph GDO2 chunk has invalid length".into(),
        ));
    }

    let mut used_overflow = false;
    for (idx, commit) in commits.iter_mut().enumerate() {
        let start = idx * 4;
        let raw = u32_be(&data[start..start + 4]);
        let offset = if raw & 0x8000_0000 == 0 {
            u64::from(raw)
        } else {
            used_overflow = true;
            let Some(overflow) = overflow else {
                return Err(GitError::InvalidFormat(
                    "commit-graph GDA2 overflow entry missing GDO2 chunk".into(),
                ));
            };
            let overflow_idx = (raw & 0x7fff_ffff) as usize;
            let overflow_start = overflow_idx.checked_mul(8).ok_or_else(|| {
                GitError::InvalidFormat("commit-graph GDO2 index overflow".into())
            })?;
            let overflow_end = overflow_start.checked_add(8).ok_or_else(|| {
                GitError::InvalidFormat("commit-graph GDO2 index overflow".into())
            })?;
            if overflow_end > overflow.len() {
                return Err(GitError::InvalidFormat(
                    "commit-graph GDA2 overflow points past GDO2 chunk".into(),
                ));
            }
            u64_be(&overflow[overflow_start..overflow_end])
        };
        commit.corrected_commit_date_offset = Some(offset);
    }
    if overflow.is_some() && !used_overflow {
        return Err(GitError::InvalidFormat(
            "commit-graph GDO2 chunk is unused by GDA2".into(),
        ));
    }
    Ok(())
}

fn parse_commit_graph_bloom_filters(
    bytes: &[u8],
    chunks: &[CommitGraphChunk],
    commit_count: usize,
) -> Result<Option<CommitGraphBloomFilters>> {
    let index = commit_graph_chunk_data(bytes, chunks, *b"BIDX", false)?;
    let data = commit_graph_chunk_data(bytes, chunks, *b"BDAT", false)?;
    let Some(data) = data else {
        return Ok(None);
    };
    let Some(index) = index else {
        return Err(GitError::InvalidFormat(
            "commit-graph BDAT chunk exists without BIDX chunk".into(),
        ));
    };
    let expected_index_len = commit_count
        .checked_mul(4)
        .ok_or_else(|| GitError::InvalidFormat("commit-graph BIDX chunk overflow".into()))?;
    if index.len() != expected_index_len {
        return Err(GitError::InvalidFormat(
            "commit-graph BIDX chunk has invalid length".into(),
        ));
    }
    if data.len() < 12 {
        return Err(GitError::InvalidFormat(
            "commit-graph BDAT chunk has invalid length".into(),
        ));
    }
    let hash_version = u32_be(&data[0..4]);
    let hash_count = u32_be(&data[4..8]);
    let bits_per_entry = u32_be(&data[8..12]);
    let payload = &data[12..];

    let mut filters = Vec::with_capacity(commit_count);
    let mut previous = 0usize;
    for idx in 0..commit_count {
        let start = idx * 4;
        let cumulative = u32_be(&index[start..start + 4]) as usize;
        if cumulative < previous {
            return Err(GitError::InvalidFormat(
                "commit-graph BIDX offsets are not monotonic".into(),
            ));
        }
        if cumulative > payload.len() {
            return Err(GitError::InvalidFormat(
                "commit-graph BIDX offset points past BDAT payload".into(),
            ));
        }
        filters.push(payload[previous..cumulative].to_vec());
        previous = cumulative;
    }
    if previous != payload.len() {
        return Err(GitError::InvalidFormat(
            "commit-graph BDAT payload has trailing bytes".into(),
        ));
    }

    Ok(Some(CommitGraphBloomFilters {
        hash_version,
        hash_count,
        bits_per_entry,
        filters,
    }))
}

fn commit_graph_parents(
    parent_one: u32,
    parent_two: u32,
    extra_edges: Option<&[u8]>,
    commit_count: usize,
) -> Result<Vec<u32>> {
    let mut parents = Vec::new();
    if parent_one != COMMIT_GRAPH_PARENT_NONE {
        validate_commit_graph_parent_position(parent_one, commit_count)?;
        parents.push(parent_one);
    }
    if parent_two == COMMIT_GRAPH_PARENT_NONE {
        return Ok(parents);
    }
    if parent_two & COMMIT_GRAPH_EXTRA_EDGE == 0 {
        validate_commit_graph_parent_position(parent_two, commit_count)?;
        parents.push(parent_two);
        return Ok(parents);
    }

    let Some(extra_edges) = extra_edges else {
        return Err(GitError::InvalidFormat(
            "commit-graph octopus edge missing EDGE chunk".into(),
        ));
    };
    let mut edge_idx = (parent_two & COMMIT_GRAPH_EXTRA_EDGE_MASK) as usize;
    loop {
        let start = edge_idx
            .checked_mul(4)
            .ok_or_else(|| GitError::InvalidFormat("commit-graph EDGE index overflow".into()))?;
        let end = start
            .checked_add(4)
            .ok_or_else(|| GitError::InvalidFormat("commit-graph EDGE index overflow".into()))?;
        if end > extra_edges.len() {
            return Err(GitError::InvalidFormat(
                "commit-graph EDGE entry points past chunk".into(),
            ));
        }
        let edge = u32_be(&extra_edges[start..end]);
        let parent = edge & COMMIT_GRAPH_EXTRA_EDGE_MASK;
        validate_commit_graph_parent_position(parent, commit_count)?;
        parents.push(parent);
        if edge & COMMIT_GRAPH_EXTRA_EDGE != 0 {
            return Ok(parents);
        }
        edge_idx = edge_idx
            .checked_add(1)
            .ok_or_else(|| GitError::InvalidFormat("commit-graph EDGE index overflow".into()))?;
    }
}

fn validate_commit_graph_parent_position(parent: u32, commit_count: usize) -> Result<()> {
    if parent as usize >= commit_count {
        return Err(GitError::InvalidFormat(
            "commit-graph parent points past commit table".into(),
        ));
    }
    Ok(())
}

fn parse_commit_graph_base_graphs(
    bytes: &[u8],
    chunks: &[CommitGraphChunk],
    format: ObjectFormat,
    base_graph_count: usize,
) -> Result<Vec<ObjectId>> {
    let data = commit_graph_chunk_data(bytes, chunks, *b"BASE", base_graph_count != 0)?;
    let Some(data) = data else {
        return Ok(Vec::new());
    };
    let expected_len = base_graph_count
        .checked_mul(format.raw_len())
        .ok_or_else(|| GitError::InvalidFormat("commit-graph BASE chunk overflow".into()))?;
    if data.len() != expected_len {
        return Err(GitError::InvalidFormat(
            "commit-graph BASE chunk has invalid length".into(),
        ));
    }
    let mut base_graphs = Vec::with_capacity(base_graph_count);
    for idx in 0..base_graph_count {
        let start = idx * format.raw_len();
        base_graphs.push(ObjectId::from_raw(
            format,
            &data[start..start + format.raw_len()],
        )?);
    }
    Ok(base_graphs)
}

fn commit_graph_chunk_data<'a>(
    bytes: &'a [u8],
    chunks: &[CommitGraphChunk],
    id: [u8; 4],
    required: bool,
) -> Result<Option<&'a [u8]>> {
    let Some(chunk) = chunks.iter().find(|chunk| chunk.id == id) else {
        if required {
            return Err(GitError::InvalidFormat(format!(
                "commit-graph missing {} chunk",
                std::str::from_utf8(&id).unwrap_or("required")
            )));
        }
        return Ok(None);
    };
    let start = usize::try_from(chunk.offset)
        .map_err(|_| GitError::InvalidFormat("commit-graph chunk offset overflow".into()))?;
    let len = usize::try_from(chunk.len)
        .map_err(|_| GitError::InvalidFormat("commit-graph chunk length overflow".into()))?;
    let end = start
        .checked_add(len)
        .ok_or_else(|| GitError::InvalidFormat("commit-graph chunk range overflow".into()))?;
    let Some(data) = bytes.get(start..end) else {
        return Err(GitError::InvalidFormat(
            "commit-graph chunk extends past file".into(),
        ));
    };
    Ok(Some(data))
}

pub fn parse_framed_object(bytes: &[u8]) -> Result<EncodedObject> {
    let nul = bytes
        .iter()
        .position(|byte| *byte == 0)
        .ok_or_else(|| GitError::InvalidObject("missing object header terminator".into()))?;
    let header = std::str::from_utf8(&bytes[..nul])
        .map_err(|err| GitError::InvalidObject(err.to_string()))?;
    let (kind, size) = header
        .split_once(' ')
        .ok_or_else(|| GitError::InvalidObject("missing object size".into()))?;
    let size: usize = size
        .parse()
        .map_err(|_| GitError::InvalidObject("invalid object size".into()))?;
    let body = &bytes[nul + 1..];
    if body.len() != size {
        return Err(GitError::InvalidObject(format!(
            "object declared {size} bytes, found {}",
            body.len()
        )));
    }
    Ok(EncodedObject::new(kind.parse()?, body.to_vec()))
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Bundle {
    pub version: u8,
    pub format: ObjectFormat,
    pub capabilities: Vec<BundleCapability>,
    pub prerequisites: Vec<BundlePrerequisite>,
    pub references: Vec<BundleReference>,
    pub pack: Vec<u8>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BundleCapability {
    pub key: String,
    pub value: Option<Vec<u8>>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BundlePrerequisite {
    pub oid: ObjectId,
    pub comment: Vec<u8>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BundleReference {
    pub oid: ObjectId,
    pub name: String,
}

impl Bundle {
    pub fn parse(bytes: &[u8], default_format: ObjectFormat) -> Result<Self> {
        let (signature, mut offset) = next_lf_line(bytes, 0)
            .ok_or_else(|| GitError::InvalidFormat("bundle missing signature".into()))?;
        let version = match signature {
            b"# v2 git bundle" => 2,
            b"# v3 git bundle" => 3,
            _ => {
                return Err(GitError::InvalidFormat("missing bundle signature".into()));
            }
        };

        let mut format = default_format;
        let mut capabilities = Vec::new();
        let mut prerequisites = Vec::new();
        let mut references = Vec::new();
        let mut seen_non_capability = false;
        loop {
            let Some((line, next_offset)) = next_lf_line(bytes, offset) else {
                return Err(GitError::InvalidFormat(
                    "bundle header missing pack separator".into(),
                ));
            };
            offset = next_offset;
            if line.is_empty() {
                break;
            }
            match line[0] {
                b'@' => {
                    if version != 3 {
                        return Err(GitError::InvalidFormat(
                            "bundle v2 cannot contain capabilities".into(),
                        ));
                    }
                    if seen_non_capability {
                        return Err(GitError::InvalidFormat(
                            "bundle capability appears after prerequisites or references".into(),
                        ));
                    }
                    let capability = parse_bundle_capability(&line[1..])?;
                    if capability.key == "object-format" {
                        let Some(value) = &capability.value else {
                            return Err(GitError::InvalidFormat(
                                "bundle object-format capability is missing a value".into(),
                            ));
                        };
                        let text = std::str::from_utf8(value)
                            .map_err(|err| GitError::InvalidFormat(err.to_string()))?;
                        format = text.parse()?;
                    }
                    capabilities.push(capability);
                }
                b'-' => {
                    seen_non_capability = true;
                    prerequisites.push(parse_bundle_prerequisite(&line[1..], format)?);
                }
                _ => {
                    seen_non_capability = true;
                    references.push(parse_bundle_reference(line, format)?);
                }
            }
        }

        Ok(Self {
            version,
            format,
            capabilities,
            prerequisites,
            references,
            pack: bytes[offset..].to_vec(),
        })
    }

    pub fn write(&self) -> Result<Vec<u8>> {
        if self.version != 2 && self.version != 3 {
            return Err(GitError::Unsupported(format!(
                "bundle version {}",
                self.version
            )));
        }
        if self.version == 2 && !self.capabilities.is_empty() {
            return Err(GitError::InvalidFormat(
                "bundle v2 cannot contain capabilities".into(),
            ));
        }
        let mut out = Vec::new();
        match self.version {
            2 => out.extend_from_slice(b"# v2 git bundle\n"),
            3 => out.extend_from_slice(b"# v3 git bundle\n"),
            _ => unreachable!(),
        }
        if self.version == 3 {
            let mut wrote_object_format = false;
            for capability in &self.capabilities {
                write_bundle_capability(&mut out, capability)?;
                if capability.key == "object-format" {
                    let Some(value) = &capability.value else {
                        return Err(GitError::InvalidFormat(
                            "bundle object-format capability is missing a value".into(),
                        ));
                    };
                    let text = std::str::from_utf8(value)
                        .map_err(|err| GitError::InvalidFormat(err.to_string()))?;
                    let format: ObjectFormat = text.parse()?;
                    if format != self.format {
                        return Err(GitError::InvalidFormat(format!(
                            "bundle object-format capability is {}, bundle uses {}",
                            format.name(),
                            self.format.name()
                        )));
                    }
                    wrote_object_format = true;
                }
            }
            if self.format != ObjectFormat::Sha1 && !wrote_object_format {
                out.extend_from_slice(b"@object-format=");
                out.extend_from_slice(self.format.name().as_bytes());
                out.push(b'\n');
            }
        }
        for prerequisite in &self.prerequisites {
            ensure_bundle_oid_format(&prerequisite.oid, self.format, "prerequisite")?;
            out.push(b'-');
            out.extend_from_slice(prerequisite.oid.to_hex().as_bytes());
            out.push(b' ');
            out.extend_from_slice(&prerequisite.comment);
            out.push(b'\n');
        }
        for reference in &self.references {
            ensure_bundle_oid_format(&reference.oid, self.format, "reference")?;
            if reference.name.is_empty() || reference.name.as_bytes().contains(&b'\n') {
                return Err(GitError::InvalidFormat(
                    "bundle reference has invalid name".into(),
                ));
            }
            out.extend_from_slice(reference.oid.to_hex().as_bytes());
            out.push(b' ');
            out.extend_from_slice(reference.name.as_bytes());
            out.push(b'\n');
        }
        out.push(b'\n');
        out.extend_from_slice(&self.pack);
        Ok(out)
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct GitConfig {
    pub sections: Vec<ConfigSection>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ConfigSection {
    pub name: String,
    pub subsection: Option<String>,
    pub entries: Vec<ConfigEntry>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ConfigEntry {
    pub key: String,
    pub value: Option<String>,
}

impl GitConfig {
    pub fn parse(bytes: &[u8]) -> Result<Self> {
        let text =
            std::str::from_utf8(bytes).map_err(|err| GitError::InvalidFormat(err.to_string()))?;
        let mut config = Self::default();
        let mut current = None;
        for (line_idx, raw_line) in text.lines().enumerate() {
            let line = raw_line.trim();
            if line.is_empty() || line.starts_with('#') || line.starts_with(';') {
                continue;
            }
            if line.starts_with('[') {
                let section = parse_config_section(line).map_err(|err| {
                    GitError::InvalidFormat(format!("config line {}: {err}", line_idx + 1))
                })?;
                config.sections.push(section);
                current = Some(config.sections.len() - 1);
                continue;
            }
            let Some(section_idx) = current else {
                return Err(GitError::InvalidFormat(format!(
                    "config line {} appears before a section",
                    line_idx + 1
                )));
            };
            let entry = parse_config_entry(line).map_err(|err| {
                GitError::InvalidFormat(format!("config line {}: {err}", line_idx + 1))
            })?;
            config.sections[section_idx].entries.push(entry);
        }
        Ok(config)
    }

    pub fn read(path: impl AsRef<Path>) -> Result<Self> {
        Self::parse(&fs::read(path)?)
    }

    pub fn get(&self, section: &str, subsection: Option<&str>, key: &str) -> Option<&str> {
        self.sections
            .iter()
            .rev()
            .filter(|candidate| {
                eq_ignore_ascii_case(&candidate.name, section)
                    && candidate.subsection.as_deref() == subsection
            })
            .flat_map(|candidate| candidate.entries.iter().rev())
            .find(|entry| eq_ignore_ascii_case(&entry.key, key))
            .and_then(|entry| entry.value.as_deref())
    }

    pub fn get_bool(&self, section: &str, subsection: Option<&str>, key: &str) -> Option<bool> {
        let entry = self
            .sections
            .iter()
            .rev()
            .filter(|candidate| {
                eq_ignore_ascii_case(&candidate.name, section)
                    && candidate.subsection.as_deref() == subsection
            })
            .flat_map(|candidate| candidate.entries.iter().rev())
            .find(|entry| eq_ignore_ascii_case(&entry.key, key))?;
        match entry.value.as_deref().map(str::trim) {
            None | Some("") => Some(true),
            Some(value) if eq_ignore_ascii_case(value, "true") => Some(true),
            Some(value) if eq_ignore_ascii_case(value, "yes") => Some(true),
            Some(value) if eq_ignore_ascii_case(value, "on") => Some(true),
            Some("1") => Some(true),
            Some(value) if eq_ignore_ascii_case(value, "false") => Some(false),
            Some(value) if eq_ignore_ascii_case(value, "no") => Some(false),
            Some(value) if eq_ignore_ascii_case(value, "off") => Some(false),
            Some("0") => Some(false),
            _ => None,
        }
    }

    pub fn repository_object_format(&self) -> Result<ObjectFormat> {
        self.get("extensions", None, "objectformat")
            .unwrap_or("sha1")
            .parse()
    }

    pub fn to_canonical_bytes(&self) -> Vec<u8> {
        let mut out = Vec::new();
        for section in &self.sections {
            out.extend_from_slice(b"[");
            out.extend_from_slice(section.name.as_bytes());
            if let Some(subsection) = &section.subsection {
                out.extend_from_slice(b" \"");
                out.extend_from_slice(escape_config_quoted_content(subsection).as_bytes());
                out.extend_from_slice(b"\"");
            }
            out.extend_from_slice(b"]\n");
            for entry in &section.entries {
                out.extend_from_slice(b"\t");
                out.extend_from_slice(entry.key.as_bytes());
                if let Some(value) = &entry.value {
                    out.extend_from_slice(b" = ");
                    out.extend_from_slice(quote_config_value(value).as_bytes());
                }
                out.extend_from_slice(b"\n");
            }
        }
        out
    }
}

fn parse_config_section(line: &str) -> std::result::Result<ConfigSection, String> {
    let Some(inner) = line
        .strip_prefix('[')
        .and_then(|line| line.strip_suffix(']'))
    else {
        return Err("invalid section header".into());
    };
    let inner = inner.trim();
    if inner.is_empty() {
        return Err("empty section header".into());
    }
    let (name, subsection) = if let Some((name, rest)) = inner.split_once(char::is_whitespace) {
        let rest = rest.trim();
        let subsection = parse_quoted_config_value(rest)?;
        (name.trim(), Some(subsection))
    } else {
        (inner, None)
    };
    if !is_config_name(name) {
        return Err(format!("invalid section name {name}"));
    }
    Ok(ConfigSection {
        name: name.to_ascii_lowercase(),
        subsection,
        entries: Vec::new(),
    })
}

fn next_lf_line(bytes: &[u8], offset: usize) -> Option<(&[u8], usize)> {
    let relative = bytes
        .get(offset..)?
        .iter()
        .position(|byte| *byte == b'\n')?;
    let end = offset + relative;
    Some((&bytes[offset..end], end + 1))
}

fn parse_bundle_capability(line: &[u8]) -> Result<BundleCapability> {
    let (key, value) = match line.iter().position(|byte| *byte == b'=') {
        Some(idx) => (&line[..idx], Some(line[idx + 1..].to_vec())),
        None => (line, None),
    };
    if key.is_empty()
        || !key
            .iter()
            .all(|byte| byte.is_ascii_alphanumeric() || *byte == b'-')
    {
        return Err(GitError::InvalidFormat(
            "bundle capability has invalid key".into(),
        ));
    }
    let key = std::str::from_utf8(key)
        .map_err(|err| GitError::InvalidFormat(err.to_string()))?
        .to_string();
    Ok(BundleCapability { key, value })
}

fn write_bundle_capability(out: &mut Vec<u8>, capability: &BundleCapability) -> Result<()> {
    if capability.key.is_empty()
        || !capability
            .key
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || byte == b'-')
    {
        return Err(GitError::InvalidFormat(
            "bundle capability has invalid key".into(),
        ));
    }
    if capability.key.as_bytes().contains(&b'\n') {
        return Err(GitError::InvalidFormat(
            "bundle capability has invalid key".into(),
        ));
    }
    out.push(b'@');
    out.extend_from_slice(capability.key.as_bytes());
    if let Some(value) = &capability.value {
        if value.contains(&b'\n') {
            return Err(GitError::InvalidFormat(
                "bundle capability has invalid value".into(),
            ));
        }
        out.push(b'=');
        out.extend_from_slice(value);
    }
    out.push(b'\n');
    Ok(())
}

fn ensure_bundle_oid_format(oid: &ObjectId, format: ObjectFormat, kind: &str) -> Result<()> {
    if oid.format() != format {
        return Err(GitError::InvalidObjectId(format!(
            "bundle {kind} {oid} uses {}, bundle uses {}",
            oid.format().name(),
            format.name()
        )));
    }
    Ok(())
}

fn parse_bundle_prerequisite(line: &[u8], format: ObjectFormat) -> Result<BundlePrerequisite> {
    let hex_len = format.hex_len();
    if line.len() < hex_len + 1 || line.get(hex_len).copied() != Some(b' ') {
        return Err(GitError::InvalidFormat(
            "bundle prerequisite line is malformed".into(),
        ));
    }
    let hex = std::str::from_utf8(&line[..hex_len])
        .map_err(|err| GitError::InvalidFormat(err.to_string()))?;
    Ok(BundlePrerequisite {
        oid: ObjectId::from_hex(format, hex)?,
        comment: line[hex_len + 1..].to_vec(),
    })
}

fn parse_bundle_reference(line: &[u8], format: ObjectFormat) -> Result<BundleReference> {
    let hex_len = format.hex_len();
    if line.len() <= hex_len + 1 || line.get(hex_len).copied() != Some(b' ') {
        return Err(GitError::InvalidFormat(
            "bundle reference line is malformed".into(),
        ));
    }
    let hex = std::str::from_utf8(&line[..hex_len])
        .map_err(|err| GitError::InvalidFormat(err.to_string()))?;
    let name = std::str::from_utf8(&line[hex_len + 1..])
        .map_err(|err| GitError::InvalidFormat(err.to_string()))?;
    if name.is_empty() {
        return Err(GitError::InvalidFormat(
            "bundle reference has empty name".into(),
        ));
    }
    Ok(BundleReference {
        oid: ObjectId::from_hex(format, hex)?,
        name: name.to_string(),
    })
}

fn parse_config_entry(line: &str) -> std::result::Result<ConfigEntry, String> {
    let (key, value) = if let Some((key, value)) = line.split_once('=') {
        (key.trim(), Some(parse_config_value(value.trim())?))
    } else {
        (line.trim(), None)
    };
    if !is_config_name(key) {
        return Err(format!("invalid key {key}"));
    }
    Ok(ConfigEntry {
        key: key.to_ascii_lowercase(),
        value,
    })
}

fn parse_config_value(value: &str) -> std::result::Result<String, String> {
    let value = strip_config_comment(value).trim();
    if value.starts_with('"') {
        parse_quoted_config_value(value)
    } else {
        Ok(value.to_string())
    }
}

fn parse_quoted_config_value(value: &str) -> std::result::Result<String, String> {
    let Some(value) = value.strip_prefix('"') else {
        return Err("expected quoted value".into());
    };
    let mut out = String::new();
    let mut escaped = false;
    for ch in value.chars() {
        if escaped {
            match ch {
                'n' => out.push('\n'),
                't' => out.push('\t'),
                'b' => out.push('\u{0008}'),
                '"' => out.push('"'),
                '\\' => out.push('\\'),
                other => out.push(other),
            }
            escaped = false;
        } else if ch == '\\' {
            escaped = true;
        } else if ch == '"' {
            return Ok(out);
        } else {
            out.push(ch);
        }
    }
    Err("unterminated quoted value".into())
}

fn quote_config_value(value: &str) -> String {
    let needs_quotes = value
        .bytes()
        .any(|byte| byte.is_ascii_whitespace() || matches!(byte, b'"' | b'\\' | b'#' | b';'));
    if !needs_quotes {
        return value.to_string();
    }
    let mut out = String::from("\"");
    out.push_str(&escape_config_quoted_content(value));
    out.push('"');
    out
}

fn escape_config_quoted_content(value: &str) -> String {
    let mut out = String::new();
    for ch in value.chars() {
        match ch {
            '\n' => out.push_str("\\n"),
            '\t' => out.push_str("\\t"),
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            other => out.push(other),
        }
    }
    out
}

fn strip_config_comment(value: &str) -> &str {
    let mut in_quote = false;
    let mut escaped = false;
    for (idx, ch) in value.char_indices() {
        if escaped {
            escaped = false;
        } else if ch == '\\' {
            escaped = true;
        } else if ch == '"' {
            in_quote = !in_quote;
        } else if !in_quote && (ch == '#' || ch == ';') {
            return &value[..idx];
        }
    }
    value
}

fn is_config_name(value: &str) -> bool {
    !value.is_empty()
        && value
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || byte == b'-')
}

fn eq_ignore_ascii_case(left: &str, right: &str) -> bool {
    left.eq_ignore_ascii_case(right)
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RepositoryLayout {
    pub git_dir: PathBuf,
    pub object_format: ObjectFormat,
    pub bare: bool,
}

impl RepositoryLayout {
    pub fn init_at(
        path: impl AsRef<Path>,
        object_format: ObjectFormat,
        bare: bool,
    ) -> Result<Self> {
        Self::init_at_with_initial_branch(path, object_format, bare, "main")
    }

    pub fn init_at_with_initial_branch(
        path: impl AsRef<Path>,
        object_format: ObjectFormat,
        bare: bool,
        initial_branch: &str,
    ) -> Result<Self> {
        let root = path.as_ref();
        let git_dir = if bare {
            root.to_path_buf()
        } else {
            root.join(".git")
        };
        fs::create_dir_all(git_dir.join("objects/info"))?;
        fs::create_dir_all(git_dir.join("objects/pack"))?;
        fs::create_dir_all(git_dir.join("refs/heads"))?;
        fs::create_dir_all(git_dir.join("refs/tags"))?;
        let head_path = git_dir.join("HEAD");
        if !head_path.exists() {
            fs::write(head_path, format!("ref: refs/heads/{initial_branch}\n"))?;
        }
        let mut config = GitConfig {
            sections: vec![ConfigSection {
                name: "core".into(),
                subsection: None,
                entries: vec![
                    ConfigEntry {
                        key: "repositoryformatversion".into(),
                        value: Some(
                            if object_format == ObjectFormat::Sha1 {
                                "0"
                            } else {
                                "1"
                            }
                            .into(),
                        ),
                    },
                    ConfigEntry {
                        key: "filemode".into(),
                        value: Some("true".into()),
                    },
                    ConfigEntry {
                        key: "bare".into(),
                        value: Some(if bare { "true" } else { "false" }.into()),
                    },
                ],
            }],
        };
        if object_format == ObjectFormat::Sha256 {
            config.sections.push(ConfigSection {
                name: "extensions".into(),
                subsection: None,
                entries: vec![ConfigEntry {
                    key: "objectformat".into(),
                    value: Some("sha256".into()),
                }],
            });
        }
        fs::write(git_dir.join("config"), config.to_canonical_bytes())?;
        Ok(Self {
            git_dir,
            object_format,
            bare,
        })
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Index {
    pub version: u32,
    pub entries: Vec<IndexEntry>,
    pub extensions: Vec<u8>,
    pub checksum: Option<ObjectId>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct IndexEntry {
    pub ctime_seconds: u32,
    pub ctime_nanoseconds: u32,
    pub mtime_seconds: u32,
    pub mtime_nanoseconds: u32,
    pub dev: u32,
    pub ino: u32,
    pub mode: u32,
    pub uid: u32,
    pub gid: u32,
    pub size: u32,
    pub oid: ObjectId,
    pub flags: u16,
    pub flags_extended: u16,
    pub path: Vec<u8>,
}

impl Index {
    pub fn parse_v2_sha1(bytes: &[u8]) -> Result<Self> {
        let hash_len = ObjectFormat::Sha1.raw_len();
        if bytes.len() < 12 + hash_len {
            return Err(GitError::InvalidFormat("index header too short".into()));
        }
        let checksum_offset = bytes.len() - hash_len;
        let actual_checksum =
            git_core::digest_bytes(ObjectFormat::Sha1, &bytes[..checksum_offset])?;
        let checksum = ObjectId::from_raw(ObjectFormat::Sha1, &bytes[checksum_offset..])?;
        if actual_checksum != checksum {
            return Err(GitError::InvalidFormat(format!(
                "index checksum mismatch: expected {checksum}, got {actual_checksum}"
            )));
        }
        if &bytes[..4] != b"DIRC" {
            return Err(GitError::InvalidFormat("missing DIRC signature".into()));
        }
        let version = u32_be(&bytes[4..8]);
        if !(2..=4).contains(&version) {
            return Err(GitError::Unsupported(format!("index version {version}")));
        }
        let count = u32_be(&bytes[8..12]) as usize;
        let mut offset = 12;
        let mut entries = Vec::with_capacity(count);
        for _ in 0..count {
            if checksum_offset.saturating_sub(offset) < 62 {
                return Err(GitError::InvalidFormat("truncated index entry".into()));
            }
            let start = offset;
            let oid = ObjectId::from_raw(ObjectFormat::Sha1, &bytes[offset + 40..offset + 60])?;
            let flags = u16_be(&bytes[offset + 60..offset + 62]);
            offset += 62;
            let flags_extended = if flags & INDEX_FLAG_EXTENDED != 0 {
                if checksum_offset.saturating_sub(offset) < 2 {
                    return Err(GitError::InvalidFormat(
                        "truncated index extended flags".into(),
                    ));
                }
                let flags_extended = u16_be(&bytes[offset..offset + 2]);
                offset += 2;
                flags_extended
            } else {
                0
            };
            let path = if version == 4 {
                let previous_path = entries
                    .last()
                    .map(|entry: &IndexEntry| entry.path.as_slice())
                    .unwrap_or(&[]);
                let strip_len =
                    decode_index_v4_path_strip_len(bytes, &mut offset, checksum_offset)?;
                if strip_len > previous_path.len() {
                    return Err(GitError::InvalidFormat(
                        "index v4 path compression removes too much prefix".into(),
                    ));
                }
                let path_start = offset;
                while bytes.get(offset).copied() != Some(0) {
                    offset += 1;
                    if offset >= checksum_offset {
                        return Err(GitError::InvalidFormat("unterminated index path".into()));
                    }
                }
                let mut path = previous_path[..previous_path.len() - strip_len].to_vec();
                path.extend_from_slice(&bytes[path_start..offset]);
                offset += 1;
                path
            } else {
                let path_start = offset;
                while bytes.get(offset).copied() != Some(0) {
                    offset += 1;
                    if offset >= checksum_offset {
                        return Err(GitError::InvalidFormat("unterminated index path".into()));
                    }
                }
                let path = bytes[path_start..offset].to_vec();
                offset += 1;
                while (offset - start) % 8 != 0 {
                    offset += 1;
                    if offset > checksum_offset {
                        return Err(GitError::InvalidFormat("truncated index padding".into()));
                    }
                }
                path
            };
            entries.push(IndexEntry {
                ctime_seconds: u32_be(&bytes[start..start + 4]),
                ctime_nanoseconds: u32_be(&bytes[start + 4..start + 8]),
                mtime_seconds: u32_be(&bytes[start + 8..start + 12]),
                mtime_nanoseconds: u32_be(&bytes[start + 12..start + 16]),
                dev: u32_be(&bytes[start + 16..start + 20]),
                ino: u32_be(&bytes[start + 20..start + 24]),
                mode: u32_be(&bytes[start + 24..start + 28]),
                uid: u32_be(&bytes[start + 28..start + 32]),
                gid: u32_be(&bytes[start + 32..start + 36]),
                size: u32_be(&bytes[start + 36..start + 40]),
                oid,
                flags,
                flags_extended,
                path,
            });
        }
        Ok(Self {
            version,
            entries,
            extensions: bytes[offset..checksum_offset].to_vec(),
            checksum: Some(checksum),
        })
    }

    pub fn write_v2_sha1(&self) -> Result<Vec<u8>> {
        if self.version != 2 {
            return Err(GitError::Unsupported(
                "canonical writer currently emits index v2".into(),
            ));
        }
        if self
            .entries
            .iter()
            .any(|entry| entry.flags & INDEX_FLAG_EXTENDED != 0 || entry.flags_extended != 0)
        {
            return Err(GitError::Unsupported(
                "index v2 writer cannot emit extended flags".into(),
            ));
        }
        self.write_sha1()
    }

    pub fn write_sha1(&self) -> Result<Vec<u8>> {
        if !(2..=4).contains(&self.version) {
            return Err(GitError::Unsupported(
                "canonical writer currently emits index v2/v3/v4".into(),
            ));
        }
        let mut out = Vec::new();
        out.extend_from_slice(b"DIRC");
        out.extend_from_slice(&self.version.to_be_bytes());
        out.extend_from_slice(&(self.entries.len() as u32).to_be_bytes());
        let mut previous_path = Vec::new();
        for entry in &self.entries {
            let start = out.len();
            out.extend_from_slice(&entry.ctime_seconds.to_be_bytes());
            out.extend_from_slice(&entry.ctime_nanoseconds.to_be_bytes());
            out.extend_from_slice(&entry.mtime_seconds.to_be_bytes());
            out.extend_from_slice(&entry.mtime_nanoseconds.to_be_bytes());
            out.extend_from_slice(&entry.dev.to_be_bytes());
            out.extend_from_slice(&entry.ino.to_be_bytes());
            out.extend_from_slice(&entry.mode.to_be_bytes());
            out.extend_from_slice(&entry.uid.to_be_bytes());
            out.extend_from_slice(&entry.gid.to_be_bytes());
            out.extend_from_slice(&entry.size.to_be_bytes());
            if entry.oid.format() != ObjectFormat::Sha1 {
                return Err(GitError::Unsupported(
                    "index v2 writer expects sha1 ids".into(),
                ));
            }
            out.extend_from_slice(entry.oid.as_bytes());
            let has_extended_flags =
                entry.flags & INDEX_FLAG_EXTENDED != 0 || entry.flags_extended != 0;
            if has_extended_flags && self.version < 3 {
                return Err(GitError::Unsupported(
                    "index extended flags require version 3".into(),
                ));
            }
            let flags = if has_extended_flags {
                entry.flags | INDEX_FLAG_EXTENDED
            } else {
                entry.flags & !INDEX_FLAG_EXTENDED
            };
            out.extend_from_slice(&flags.to_be_bytes());
            if has_extended_flags {
                out.extend_from_slice(&entry.flags_extended.to_be_bytes());
            }
            if self.version == 4 {
                let common_prefix_len = common_prefix_len(&previous_path, &entry.path);
                let strip_len = previous_path.len() - common_prefix_len;
                encode_index_v4_path_strip_len(strip_len, &mut out);
                out.extend_from_slice(&entry.path[common_prefix_len..]);
                out.push(0);
                previous_path = entry.path.clone();
            } else {
                out.extend_from_slice(&entry.path);
                out.push(0);
                while (out.len() - start) % 8 != 0 {
                    out.push(0);
                }
            }
        }
        out.extend_from_slice(&self.extensions);
        let checksum = git_core::digest_bytes(ObjectFormat::Sha1, &out)?;
        out.extend_from_slice(checksum.as_bytes());
        Ok(out)
    }
}

const INDEX_FLAG_EXTENDED: u16 = 0x4000;

fn decode_index_v4_path_strip_len(
    bytes: &[u8],
    offset: &mut usize,
    checksum_offset: usize,
) -> Result<usize> {
    let Some(first) = bytes.get(*offset).copied() else {
        return Err(GitError::InvalidFormat(
            "truncated index v4 path compression".into(),
        ));
    };
    *offset += 1;
    let mut value = (first & 0x7f) as usize;
    let mut byte = first;
    while byte & 0x80 != 0 {
        if *offset >= checksum_offset {
            return Err(GitError::InvalidFormat(
                "truncated index v4 path compression".into(),
            ));
        }
        byte = bytes[*offset];
        *offset += 1;
        value = value
            .checked_add(1)
            .and_then(|value| value.checked_shl(7))
            .and_then(|value| value.checked_add((byte & 0x7f) as usize))
            .ok_or_else(|| GitError::InvalidFormat("index v4 path compression overflow".into()))?;
    }
    Ok(value)
}

fn encode_index_v4_path_strip_len(strip_len: usize, out: &mut Vec<u8>) {
    let mut bytes = Vec::new();
    bytes.push((strip_len & 0x7f) as u8);
    let mut value = strip_len >> 7;
    while value != 0 {
        value -= 1;
        bytes.push(((value & 0x7f) as u8) | 0x80);
        value >>= 7;
    }
    for byte in bytes.iter().rev() {
        out.push(*byte);
    }
}

fn common_prefix_len(left: &[u8], right: &[u8]) -> usize {
    left.iter()
        .zip(right.iter())
        .take_while(|(left, right)| left == right)
        .count()
}

fn u32_be(bytes: &[u8]) -> u32 {
    u32::from_be_bytes([bytes[0], bytes[1], bytes[2], bytes[3]])
}

fn u64_be(bytes: &[u8]) -> u64 {
    u64::from_be_bytes([
        bytes[0], bytes[1], bytes[2], bytes[3], bytes[4], bytes[5], bytes[6], bytes[7],
    ])
}

fn u16_be(bytes: &[u8]) -> u16 {
    u16::from_be_bytes([bytes[0], bytes[1]])
}

fn hash_function_id(format: ObjectFormat) -> u32 {
    match format {
        ObjectFormat::Sha1 => 1,
        ObjectFormat::Sha256 => 2,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn framed_object_round_trips() {
        let object = EncodedObject::new(ObjectType::Blob, b"hello\n".to_vec());
        assert_eq!(parse_framed_object(&object.framed_bytes()).unwrap(), object);
    }

    #[test]
    fn tree_round_trips_entries() {
        let blob = ObjectId::from_hex(
            ObjectFormat::Sha1,
            "ce013625030ba8dba906f756967f9e9ca394464a",
        )
        .unwrap();
        let tree = Tree {
            entries: vec![TreeEntry {
                mode: 0o100644,
                name: b"hello.txt".to_vec(),
                oid: blob,
            }],
        };
        assert_eq!(
            Tree::parse(ObjectFormat::Sha1, &tree.write()).unwrap(),
            tree
        );
    }

    #[test]
    fn commit_round_trips_headers_and_message() {
        let tree = ObjectId::from_hex(
            ObjectFormat::Sha1,
            "4b825dc642cb6eb9a060e54bf8d69288fbee4904",
        )
        .unwrap();
        let commit = Commit {
            tree,
            parents: Vec::new(),
            author: b"A U Thor <a@example.invalid> 0 +0000".to_vec(),
            committer: b"C O Mitter <c@example.invalid> 0 +0000".to_vec(),
            encoding: Some(b"ISO-8859-1".to_vec()),
            message: b"subject\n\nbody\n".to_vec(),
        };
        assert_eq!(
            Commit::parse(ObjectFormat::Sha1, &commit.write()).unwrap(),
            commit
        );
    }

    #[test]
    fn parses_commit_graph_core_chunks() {
        let tree = oid("aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa");
        let commits = vec![
            (
                oid("1111111111111111111111111111111111111111"),
                tree.clone(),
                Vec::new(),
                1,
                1,
            ),
            (
                oid("2222222222222222222222222222222222222222"),
                tree.clone(),
                vec![0],
                2,
                2,
            ),
            (
                oid("3333333333333333333333333333333333333333"),
                tree.clone(),
                vec![1],
                3,
                3,
            ),
            (
                oid("4444444444444444444444444444444444444444"),
                tree,
                vec![0, 1, 2],
                4,
                0x1_0000_0001,
            ),
        ];
        let bytes = commit_graph(ObjectFormat::Sha1, 0, &commit_graph_chunks(&commits));

        let parsed = CommitGraph::parse(&bytes, ObjectFormat::Sha1).unwrap();
        assert_eq!(parsed.version, 1);
        assert_eq!(parsed.format, ObjectFormat::Sha1);
        assert_eq!(parsed.base_graph_count, 0);
        assert_eq!(parsed.commits.len(), 4);
        let merge = parsed.find(&commits[3].0).unwrap();
        assert_eq!(merge.parents, vec![0, 1, 2]);
        assert_eq!(merge.generation, 4);
        assert_eq!(merge.commit_time, 0x1_0000_0001);
        assert_eq!(merge.corrected_commit_date_offset, None);
        assert!(parsed.base_graphs.is_empty());
        assert_eq!(parsed.bloom_filters, None);
        assert_eq!(parsed.chunks[0].id, *b"OIDF");
    }

    #[test]
    fn writes_commit_graph_core_chunks_that_round_trip() {
        let base = oid("1111111111111111111111111111111111111111");
        let main = oid("2222222222222222222222222222222222222222");
        let side = oid("3333333333333333333333333333333333333333");
        let merge = oid("4444444444444444444444444444444444444444");
        let tree = oid("aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa");
        let bytes = CommitGraph::write(
            ObjectFormat::Sha1,
            &[
                CommitGraphWriteEntry {
                    oid: merge.clone(),
                    tree: tree.clone(),
                    parents: vec![main.clone(), side.clone()],
                    generation: 3,
                    commit_time: 30,
                },
                CommitGraphWriteEntry {
                    oid: base.clone(),
                    tree: tree.clone(),
                    parents: Vec::new(),
                    generation: 1,
                    commit_time: 10,
                },
                CommitGraphWriteEntry {
                    oid: main.clone(),
                    tree: tree.clone(),
                    parents: vec![base.clone()],
                    generation: 2,
                    commit_time: 20,
                },
                CommitGraphWriteEntry {
                    oid: side.clone(),
                    tree,
                    parents: vec![base.clone()],
                    generation: 2,
                    commit_time: 21,
                },
            ],
        )
        .unwrap();

        let parsed = CommitGraph::parse(&bytes, ObjectFormat::Sha1).unwrap();
        assert_eq!(parsed.commits.len(), 4);
        assert_eq!(parsed.find(&base).unwrap().parents, Vec::<u32>::new());
        assert_eq!(parsed.find(&main).unwrap().parents, vec![0]);
        assert_eq!(parsed.find(&side).unwrap().parents, vec![0]);
        assert_eq!(parsed.find(&merge).unwrap().parents, vec![1, 2]);
        assert_eq!(parsed.find(&merge).unwrap().generation, 3);
        assert_eq!(parsed.find(&merge).unwrap().commit_time, 30);
    }

    #[test]
    fn parses_commit_graph_bloom_filters() {
        let tree = oid("aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa");
        let commits = vec![
            (
                oid("1111111111111111111111111111111111111111"),
                tree.clone(),
                Vec::new(),
                1,
                1,
            ),
            (
                oid("2222222222222222222222222222222222222222"),
                tree,
                vec![0],
                2,
                2,
            ),
        ];
        let mut chunks = commit_graph_chunks(&commits);
        chunks.push((*b"BIDX", commit_graph_bidx(&[2, 3])));
        chunks.push((*b"BDAT", commit_graph_bdat(2, 7, 10, &[0xaa, 0xbb, 0xcc])));
        let bytes = commit_graph(ObjectFormat::Sha1, 0, &chunks);

        let parsed = CommitGraph::parse(&bytes, ObjectFormat::Sha1).unwrap();
        assert_eq!(
            parsed.bloom_filters,
            Some(CommitGraphBloomFilters {
                hash_version: 2,
                hash_count: 7,
                bits_per_entry: 10,
                filters: vec![vec![0xaa, 0xbb], vec![0xcc]],
            })
        );
    }

    #[test]
    fn parses_commit_graph_generation_data() {
        let tree = oid("aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa");
        let commits = vec![
            (
                oid("1111111111111111111111111111111111111111"),
                tree.clone(),
                Vec::new(),
                1,
                1,
            ),
            (
                oid("2222222222222222222222222222222222222222"),
                tree,
                vec![0],
                2,
                2,
            ),
        ];
        let mut chunks = commit_graph_chunks(&commits);
        chunks.push((*b"GDA2", commit_graph_gda2(&[7, 0x8000_0000])));
        chunks.push((*b"GDO2", commit_graph_gdo2(&[0x1_0000_0007])));
        let bytes = commit_graph(ObjectFormat::Sha1, 0, &chunks);

        let parsed = CommitGraph::parse(&bytes, ObjectFormat::Sha1).unwrap();
        assert_eq!(parsed.commits[0].corrected_commit_date_offset, Some(7));
        assert_eq!(
            parsed.commits[1].corrected_commit_date_offset,
            Some(0x1_0000_0007)
        );
    }

    #[test]
    fn parses_commit_graph_base_graph_hashes() {
        let commit = (
            oid("1111111111111111111111111111111111111111"),
            oid("aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa"),
            Vec::new(),
            1,
            1,
        );
        let base_graph = oid("bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb");
        let mut chunks = commit_graph_chunks(&[commit]);
        chunks.push((*b"BASE", base_graph.as_bytes().to_vec()));
        let bytes = commit_graph(ObjectFormat::Sha1, 1, &chunks);

        let parsed = CommitGraph::parse(&bytes, ObjectFormat::Sha1).unwrap();
        assert_eq!(parsed.base_graph_count, 1);
        assert_eq!(parsed.base_graphs, vec![base_graph]);
    }

    #[test]
    fn rejects_bad_commit_graph_shape() {
        let commit = (
            oid("1111111111111111111111111111111111111111"),
            oid("aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa"),
            Vec::new(),
            1,
            1,
        );
        let chunks = commit_graph_chunks(std::slice::from_ref(&commit));
        let mut bad_checksum = commit_graph(ObjectFormat::Sha1, 0, &chunks);
        let last = bad_checksum.len() - 1;
        bad_checksum[last] ^= 1;
        assert!(CommitGraph::parse(&bad_checksum, ObjectFormat::Sha1).is_err());

        let missing_cdat = commit_graph(
            ObjectFormat::Sha1,
            0,
            &chunks
                .iter()
                .filter(|(id, _data)| id != b"CDAT")
                .cloned()
                .collect::<Vec<_>>(),
        );
        assert!(CommitGraph::parse(&missing_cdat, ObjectFormat::Sha1).is_err());

        let bad_fanout = commit_graph(
            ObjectFormat::Sha1,
            0,
            &[
                (*b"OIDF", vec![0; 256 * 4]),
                (*b"OIDL", commit.0.as_bytes().to_vec()),
                (*b"CDAT", commit_graph_cdat(&[commit.clone()]).0),
            ],
        );
        assert!(CommitGraph::parse(&bad_fanout, ObjectFormat::Sha1).is_err());

        let octopus = (
            oid("2222222222222222222222222222222222222222"),
            oid("aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa"),
            vec![0, 0, 0],
            2,
            2,
        );
        let missing_edge = commit_graph(
            ObjectFormat::Sha1,
            0,
            &commit_graph_chunks(&[commit.clone(), octopus])
                .into_iter()
                .filter(|(id, _data)| id != b"EDGE")
                .collect::<Vec<_>>(),
        );
        assert!(CommitGraph::parse(&missing_edge, ObjectFormat::Sha1).is_err());

        let mut bad_base = commit_graph_chunks(&[commit]);
        bad_base.push((*b"BASE", vec![0]));
        let bad_base = commit_graph(ObjectFormat::Sha1, 1, &bad_base);
        assert!(CommitGraph::parse(&bad_base, ObjectFormat::Sha1).is_err());
    }

    #[test]
    fn rejects_bad_commit_graph_generation_data() {
        let commit = (
            oid("1111111111111111111111111111111111111111"),
            oid("aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa"),
            Vec::new(),
            1,
            1,
        );

        let mut short_gda2 = commit_graph_chunks(std::slice::from_ref(&commit));
        short_gda2.push((*b"GDA2", vec![0]));
        let short_gda2 = commit_graph(ObjectFormat::Sha1, 0, &short_gda2);
        assert!(CommitGraph::parse(&short_gda2, ObjectFormat::Sha1).is_err());

        let mut missing_gdo2 = commit_graph_chunks(std::slice::from_ref(&commit));
        missing_gdo2.push((*b"GDA2", commit_graph_gda2(&[0x8000_0000])));
        let missing_gdo2 = commit_graph(ObjectFormat::Sha1, 0, &missing_gdo2);
        assert!(CommitGraph::parse(&missing_gdo2, ObjectFormat::Sha1).is_err());

        let mut bad_gdo2 = commit_graph_chunks(std::slice::from_ref(&commit));
        bad_gdo2.push((*b"GDA2", commit_graph_gda2(&[0x8000_0000])));
        bad_gdo2.push((*b"GDO2", vec![0]));
        let bad_gdo2 = commit_graph(ObjectFormat::Sha1, 0, &bad_gdo2);
        assert!(CommitGraph::parse(&bad_gdo2, ObjectFormat::Sha1).is_err());

        let mut unused_gdo2 = commit_graph_chunks(std::slice::from_ref(&commit));
        unused_gdo2.push((*b"GDA2", commit_graph_gda2(&[1])));
        unused_gdo2.push((*b"GDO2", commit_graph_gdo2(&[2])));
        let unused_gdo2 = commit_graph(ObjectFormat::Sha1, 0, &unused_gdo2);
        assert!(CommitGraph::parse(&unused_gdo2, ObjectFormat::Sha1).is_err());

        let mut orphan_gdo2 = commit_graph_chunks(&[commit]);
        orphan_gdo2.push((*b"GDO2", commit_graph_gdo2(&[2])));
        let orphan_gdo2 = commit_graph(ObjectFormat::Sha1, 0, &orphan_gdo2);
        assert!(CommitGraph::parse(&orphan_gdo2, ObjectFormat::Sha1).is_err());
    }

    #[test]
    fn rejects_bad_commit_graph_bloom_filters() {
        let commit = (
            oid("1111111111111111111111111111111111111111"),
            oid("aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa"),
            Vec::new(),
            1,
            1,
        );

        let mut bidx_without_bdat = commit_graph_chunks(std::slice::from_ref(&commit));
        bidx_without_bdat.push((*b"BIDX", commit_graph_bidx(&[1])));
        let bidx_without_bdat = commit_graph(ObjectFormat::Sha1, 0, &bidx_without_bdat);
        assert_eq!(
            CommitGraph::parse(&bidx_without_bdat, ObjectFormat::Sha1)
                .unwrap()
                .bloom_filters,
            None
        );

        let mut bdat_without_bidx = commit_graph_chunks(std::slice::from_ref(&commit));
        bdat_without_bidx.push((*b"BDAT", commit_graph_bdat(2, 7, 10, &[0xaa])));
        let bdat_without_bidx = commit_graph(ObjectFormat::Sha1, 0, &bdat_without_bidx);
        assert!(CommitGraph::parse(&bdat_without_bidx, ObjectFormat::Sha1).is_err());

        let mut short_bidx = commit_graph_chunks(std::slice::from_ref(&commit));
        short_bidx.push((*b"BIDX", vec![0]));
        short_bidx.push((*b"BDAT", commit_graph_bdat(2, 7, 10, &[0xaa])));
        let short_bidx = commit_graph(ObjectFormat::Sha1, 0, &short_bidx);
        assert!(CommitGraph::parse(&short_bidx, ObjectFormat::Sha1).is_err());

        let mut short_bdat = commit_graph_chunks(std::slice::from_ref(&commit));
        short_bdat.push((*b"BIDX", commit_graph_bidx(&[0])));
        short_bdat.push((*b"BDAT", vec![0]));
        let short_bdat = commit_graph(ObjectFormat::Sha1, 0, &short_bdat);
        assert!(CommitGraph::parse(&short_bdat, ObjectFormat::Sha1).is_err());

        let mut bidx_past_payload = commit_graph_chunks(std::slice::from_ref(&commit));
        bidx_past_payload.push((*b"BIDX", commit_graph_bidx(&[2])));
        bidx_past_payload.push((*b"BDAT", commit_graph_bdat(2, 7, 10, &[0xaa])));
        let bidx_past_payload = commit_graph(ObjectFormat::Sha1, 0, &bidx_past_payload);
        assert!(CommitGraph::parse(&bidx_past_payload, ObjectFormat::Sha1).is_err());

        let mut trailing_payload = commit_graph_chunks(&[commit]);
        trailing_payload.push((*b"BIDX", commit_graph_bidx(&[1])));
        trailing_payload.push((*b"BDAT", commit_graph_bdat(2, 7, 10, &[0xaa, 0xbb])));
        let trailing_payload = commit_graph(ObjectFormat::Sha1, 0, &trailing_payload);
        assert!(CommitGraph::parse(&trailing_payload, ObjectFormat::Sha1).is_err());
    }

    #[test]
    fn tag_round_trips_headers_and_message() {
        let object = ObjectId::from_hex(
            ObjectFormat::Sha1,
            "e7556fb3ba7b8f5b1f4772180772a4d6a7323e15",
        )
        .unwrap();
        let tag = Tag {
            object,
            object_type: ObjectType::Commit,
            name: b"v1.0".to_vec(),
            tagger: Some(b"Example User <example@example.invalid> 0 +0000".to_vec()),
            message: b"release\n".to_vec(),
        };
        assert_eq!(Tag::parse(ObjectFormat::Sha1, &tag.write()).unwrap(), tag);
    }

    #[test]
    fn parses_bundle_v2_header_and_pack() {
        let prerequisite = oid("1111111111111111111111111111111111111111");
        let reference = oid("2222222222222222222222222222222222222222");
        let bytes = format!(
            "# v2 git bundle\n-{prerequisite} prerequisite comment\n{reference} refs/heads/main\n\n"
        )
        .into_bytes()
        .into_iter()
        .chain(b"PACKdata".iter().copied())
        .collect::<Vec<_>>();

        let parsed = Bundle::parse(&bytes, ObjectFormat::Sha1).unwrap();
        assert_eq!(parsed.version, 2);
        assert_eq!(parsed.format, ObjectFormat::Sha1);
        assert!(parsed.capabilities.is_empty());
        assert_eq!(
            parsed.prerequisites,
            vec![BundlePrerequisite {
                oid: prerequisite,
                comment: b"prerequisite comment".to_vec(),
            }]
        );
        assert_eq!(
            parsed.references,
            vec![BundleReference {
                oid: reference,
                name: "refs/heads/main".into(),
            }]
        );
        assert_eq!(parsed.pack, b"PACKdata");
    }

    #[test]
    fn parses_bundle_v3_capabilities_and_sha256_ids() {
        let prerequisite = ObjectId::from_hex(
            ObjectFormat::Sha256,
            "1111111111111111111111111111111111111111111111111111111111111111",
        )
        .unwrap();
        let reference = ObjectId::from_hex(
            ObjectFormat::Sha256,
            "2222222222222222222222222222222222222222222222222222222222222222",
        )
        .unwrap();
        let bytes = format!(
            "# v3 git bundle\n@object-format=sha256\n@filter=blob:none\n-{prerequisite} base\n{reference} refs/heads/main\n\n"
        )
        .into_bytes();

        let parsed = Bundle::parse(&bytes, ObjectFormat::Sha1).unwrap();
        assert_eq!(parsed.version, 3);
        assert_eq!(parsed.format, ObjectFormat::Sha256);
        assert_eq!(
            parsed.capabilities,
            vec![
                BundleCapability {
                    key: "object-format".into(),
                    value: Some(b"sha256".to_vec()),
                },
                BundleCapability {
                    key: "filter".into(),
                    value: Some(b"blob:none".to_vec()),
                },
            ]
        );
        assert_eq!(parsed.prerequisites[0].oid, prerequisite);
        assert_eq!(parsed.references[0].oid, reference);
    }

    #[test]
    fn writes_bundle_v2_header_and_pack() {
        let prerequisite =
            git_core::object_id_for_bytes(ObjectFormat::Sha1, "blob", b"base\n").unwrap();
        let reference =
            git_core::object_id_for_bytes(ObjectFormat::Sha1, "blob", b"tip\n").unwrap();
        let bundle = Bundle {
            version: 2,
            format: ObjectFormat::Sha1,
            capabilities: Vec::new(),
            prerequisites: vec![BundlePrerequisite {
                oid: prerequisite.clone(),
                comment: b"base comment".to_vec(),
            }],
            references: vec![BundleReference {
                oid: reference.clone(),
                name: "refs/heads/main".into(),
            }],
            pack: b"PACKv2".to_vec(),
        };

        let bytes = bundle.write().unwrap();
        let expected = format!(
            "# v2 git bundle\n-{prerequisite} base comment\n{reference} refs/heads/main\n\n"
        )
        .into_bytes()
        .into_iter()
        .chain(b"PACKv2".iter().copied())
        .collect::<Vec<_>>();
        assert_eq!(bytes, expected);
        assert_eq!(Bundle::parse(&bytes, ObjectFormat::Sha1).unwrap(), bundle);
    }

    #[test]
    fn writes_bundle_v3_sha256_object_format_capability() {
        let oid = git_core::object_id_for_bytes(ObjectFormat::Sha256, "blob", b"tip\n").unwrap();
        let bundle = Bundle {
            version: 3,
            format: ObjectFormat::Sha256,
            capabilities: vec![BundleCapability {
                key: "filter".into(),
                value: Some(b"blob:none".to_vec()),
            }],
            prerequisites: Vec::new(),
            references: vec![BundleReference {
                oid: oid.clone(),
                name: "refs/heads/main".into(),
            }],
            pack: b"PACKv3".to_vec(),
        };

        let bytes = bundle.write().unwrap();
        let text = String::from_utf8(bytes.clone()).unwrap();
        assert!(text.starts_with("# v3 git bundle\n@filter=blob:none\n@object-format=sha256\n"));
        let parsed = Bundle::parse(&bytes, ObjectFormat::Sha1).unwrap();
        assert_eq!(parsed.format, ObjectFormat::Sha256);
        assert_eq!(parsed.references[0].oid, oid);
        assert_eq!(parsed.pack, b"PACKv3");
    }

    #[test]
    fn rejects_bad_bundle_write_inputs() {
        let sha1 = git_core::object_id_for_bytes(ObjectFormat::Sha1, "blob", b"tip\n").unwrap();
        let sha256 = git_core::object_id_for_bytes(ObjectFormat::Sha256, "blob", b"tip\n").unwrap();
        let mut bundle = Bundle {
            version: 2,
            format: ObjectFormat::Sha1,
            capabilities: vec![BundleCapability {
                key: "filter".into(),
                value: Some(b"blob:none".to_vec()),
            }],
            prerequisites: Vec::new(),
            references: Vec::new(),
            pack: Vec::new(),
        };
        assert!(bundle.write().is_err());

        bundle.version = 3;
        bundle.capabilities = vec![BundleCapability {
            key: "bad_key".into(),
            value: None,
        }];
        assert!(bundle.write().is_err());

        bundle.capabilities = Vec::new();
        bundle.references = vec![BundleReference {
            oid: sha256,
            name: "refs/heads/main".into(),
        }];
        assert!(bundle.write().is_err());

        bundle.references = vec![BundleReference {
            oid: sha1,
            name: "bad\nname".into(),
        }];
        assert!(bundle.write().is_err());
    }

    #[test]
    fn rejects_bad_bundle_headers() {
        assert!(Bundle::parse(b"# v4 git bundle\n\n", ObjectFormat::Sha1).is_err());
        assert!(
            Bundle::parse(
                b"# v2 git bundle\n@filter=blob:none\n\n",
                ObjectFormat::Sha1
            )
            .is_err()
        );
        assert!(Bundle::parse(b"# v3 git bundle\n@bad_key=value\n\n", ObjectFormat::Sha1).is_err());
        assert!(
            Bundle::parse(
                b"# v3 git bundle\n1111111111111111111111111111111111111111 refs/heads/main\n@filter=blob:none\n\n",
                ObjectFormat::Sha1,
            )
            .is_err()
        );
        assert!(
            Bundle::parse(
                b"# v3 git bundle\n@object-format=unknown\n\n",
                ObjectFormat::Sha1,
            )
            .is_err()
        );
        assert!(
            Bundle::parse(
                b"# v2 git bundle\n1111111111111111111111111111111111111111 refs/heads/main",
                ObjectFormat::Sha1,
            )
            .is_err()
        );
        assert!(
            Bundle::parse(
                b"# v2 git bundle\n1111111111111111111111111111111111111111 \n\n",
                ObjectFormat::Sha1,
            )
            .is_err()
        );
    }

    #[test]
    fn config_parses_sections_values_and_comments() {
        let config = GitConfig::parse(
            br#"
[core]
    filemode = true
    bare = false ; comment
[remote "origin"]
    url = "https://example.invalid/repo.git"
    fetch = +refs/heads/*:refs/remotes/origin/*
[feature]
    enabled
"#,
        )
        .unwrap();
        assert_eq!(config.get_bool("core", None, "filemode"), Some(true));
        assert_eq!(config.get_bool("core", None, "bare"), Some(false));
        assert_eq!(
            config.get("remote", Some("origin"), "url"),
            Some("https://example.invalid/repo.git")
        );
        assert_eq!(config.get_bool("feature", None, "enabled"), Some(true));
    }

    #[test]
    fn config_reports_repository_object_format() {
        let config = GitConfig::parse(b"[extensions]\n\tobjectformat = sha256\n").unwrap();
        assert_eq!(
            config.repository_object_format().unwrap(),
            ObjectFormat::Sha256
        );
    }

    #[test]
    fn config_canonical_writer_round_trips() {
        let config = GitConfig {
            sections: vec![ConfigSection {
                name: "remote".into(),
                subsection: Some("origin repo".into()),
                entries: vec![ConfigEntry {
                    key: "url".into(),
                    value: Some("https://example.invalid/repo.git".into()),
                }],
            }],
        };
        let parsed = GitConfig::parse(&config.to_canonical_bytes()).unwrap();
        assert_eq!(parsed, config);
    }

    #[test]
    fn index_v2_round_trips_entry() {
        let index = Index {
            version: 2,
            entries: vec![IndexEntry {
                ctime_seconds: 1,
                ctime_nanoseconds: 2,
                mtime_seconds: 3,
                mtime_nanoseconds: 4,
                dev: 5,
                ino: 6,
                mode: 0o100644,
                uid: 7,
                gid: 8,
                size: 6,
                oid: ObjectId::from_hex(
                    ObjectFormat::Sha1,
                    "ce013625030ba8dba906f756967f9e9ca394464a",
                )
                .unwrap(),
                flags: 5,
                flags_extended: 0,
                path: b"a.txt".to_vec(),
            }],
            extensions: Vec::new(),
            checksum: None,
        };
        let bytes = index.write_v2_sha1().unwrap();
        let parsed = Index::parse_v2_sha1(&bytes).unwrap();
        assert_eq!(parsed.version, index.version);
        assert_eq!(parsed.entries, index.entries);
        assert_eq!(parsed.extensions, index.extensions);
        assert!(parsed.checksum.is_some());
    }

    #[test]
    fn index_v4_round_trips_prefix_compressed_paths() {
        let long_path = vec![b'a'; 140];
        let index = Index {
            version: 4,
            entries: vec![
                IndexEntry {
                    ctime_seconds: 1,
                    ctime_nanoseconds: 2,
                    mtime_seconds: 3,
                    mtime_nanoseconds: 4,
                    dev: 5,
                    ino: 6,
                    mode: 0o100644,
                    uid: 7,
                    gid: 8,
                    size: 1,
                    oid: ObjectId::from_hex(
                        ObjectFormat::Sha1,
                        "ce013625030ba8dba906f756967f9e9ca394464a",
                    )
                    .unwrap(),
                    flags: long_path.len() as u16,
                    flags_extended: 0,
                    path: long_path,
                },
                IndexEntry {
                    ctime_seconds: 9,
                    ctime_nanoseconds: 10,
                    mtime_seconds: 11,
                    mtime_nanoseconds: 12,
                    dev: 13,
                    ino: 14,
                    mode: 0o100644,
                    uid: 15,
                    gid: 16,
                    size: 1,
                    oid: ObjectId::from_hex(
                        ObjectFormat::Sha1,
                        "2e65efe2a145dda7ee51d1741299f848e5bf752e",
                    )
                    .unwrap(),
                    flags: 1,
                    flags_extended: 0,
                    path: b"b".to_vec(),
                },
            ],
            extensions: Vec::new(),
            checksum: None,
        };
        let bytes = index.write_sha1().unwrap();
        assert!(bytes.windows(3).any(|window| window == [0x80, 0x0c, b'b']));
        let parsed = Index::parse_v2_sha1(&bytes).unwrap();
        assert_eq!(parsed.version, index.version);
        assert_eq!(parsed.entries, index.entries);
        assert_eq!(parsed.extensions, index.extensions);
        assert!(parsed.checksum.is_some());
    }

    #[test]
    fn index_rejects_bad_checksum() {
        let index = Index {
            version: 2,
            entries: Vec::new(),
            extensions: Vec::new(),
            checksum: None,
        };
        let mut bytes = index.write_v2_sha1().unwrap();
        let last = bytes.len() - 1;
        bytes[last] ^= 1;
        assert!(Index::parse_v2_sha1(&bytes).is_err());
    }

    fn oid(hex: &str) -> ObjectId {
        ObjectId::from_hex(ObjectFormat::Sha1, hex).unwrap()
    }

    fn commit_graph(
        format: ObjectFormat,
        base_graph_count: u8,
        chunks: &[([u8; 4], Vec<u8>)],
    ) -> Vec<u8> {
        let lookup_len = (chunks.len() + 1) * 12;
        let mut out = Vec::new();
        out.extend_from_slice(b"CGPH");
        out.push(1);
        out.push(hash_function_id(format) as u8);
        out.push(chunks.len() as u8);
        out.push(base_graph_count);
        let mut chunk_offset = (8 + lookup_len) as u64;
        for (id, data) in chunks {
            out.extend_from_slice(id);
            out.extend_from_slice(&chunk_offset.to_be_bytes());
            chunk_offset += data.len() as u64;
        }
        out.extend_from_slice(&[0, 0, 0, 0]);
        out.extend_from_slice(&chunk_offset.to_be_bytes());
        for (_id, data) in chunks {
            out.extend_from_slice(data);
        }
        let checksum = git_core::digest_bytes(format, &out).unwrap();
        out.extend_from_slice(checksum.as_bytes());
        out
    }

    fn commit_graph_chunks(
        entries: &[(ObjectId, ObjectId, Vec<u32>, u32, u64)],
    ) -> Vec<([u8; 4], Vec<u8>)> {
        let mut entries = entries.to_vec();
        entries.sort_by(|left, right| left.0.as_bytes().cmp(right.0.as_bytes()));
        let object_ids: Vec<ObjectId> = entries.iter().map(|entry| entry.0.clone()).collect();
        let (cdat, edge) = commit_graph_cdat(&entries);
        let mut chunks = vec![
            (*b"OIDF", commit_graph_fanout(&object_ids)),
            (*b"OIDL", commit_graph_oid_lookup(&object_ids)),
            (*b"CDAT", cdat),
        ];
        if !edge.is_empty() {
            chunks.push((*b"EDGE", edge));
        }
        chunks
    }

    fn commit_graph_fanout(object_ids: &[ObjectId]) -> Vec<u8> {
        let mut counts = [0u32; 256];
        for oid in object_ids {
            counts[oid.as_bytes()[0] as usize] += 1;
        }
        let mut running = 0u32;
        let mut out = Vec::new();
        for count in counts {
            running += count;
            out.extend_from_slice(&running.to_be_bytes());
        }
        out
    }

    fn commit_graph_oid_lookup(object_ids: &[ObjectId]) -> Vec<u8> {
        let mut out = Vec::new();
        for oid in object_ids {
            out.extend_from_slice(oid.as_bytes());
        }
        out
    }

    fn commit_graph_cdat(
        entries: &[(ObjectId, ObjectId, Vec<u32>, u32, u64)],
    ) -> (Vec<u8>, Vec<u8>) {
        let mut cdat = Vec::new();
        let mut edge = Vec::new();
        for (_oid, tree, parents, generation, commit_time) in entries {
            cdat.extend_from_slice(tree.as_bytes());
            let first_parent = parents.first().copied().unwrap_or(COMMIT_GRAPH_PARENT_NONE);
            let second_parent = match parents.len() {
                0 | 1 => COMMIT_GRAPH_PARENT_NONE,
                2 => parents[1],
                _ => {
                    let edge_start = (edge.len() / 4) as u32;
                    for (idx, parent) in parents[1..].iter().enumerate() {
                        let mut value = *parent;
                        if idx == parents.len() - 2 {
                            value |= COMMIT_GRAPH_EXTRA_EDGE;
                        }
                        edge.extend_from_slice(&value.to_be_bytes());
                    }
                    COMMIT_GRAPH_EXTRA_EDGE | edge_start
                }
            };
            cdat.extend_from_slice(&first_parent.to_be_bytes());
            cdat.extend_from_slice(&second_parent.to_be_bytes());
            let generation_and_time_high =
                (*generation << 2) | (((*commit_time >> 32) as u32) & 0x3);
            cdat.extend_from_slice(&generation_and_time_high.to_be_bytes());
            cdat.extend_from_slice(&(*commit_time as u32).to_be_bytes());
        }
        (cdat, edge)
    }

    fn commit_graph_gda2(values: &[u32]) -> Vec<u8> {
        let mut out = Vec::new();
        for value in values {
            out.extend_from_slice(&value.to_be_bytes());
        }
        out
    }

    fn commit_graph_gdo2(values: &[u64]) -> Vec<u8> {
        let mut out = Vec::new();
        for value in values {
            out.extend_from_slice(&value.to_be_bytes());
        }
        out
    }

    fn commit_graph_bidx(values: &[u32]) -> Vec<u8> {
        let mut out = Vec::new();
        for value in values {
            out.extend_from_slice(&value.to_be_bytes());
        }
        out
    }

    fn commit_graph_bdat(
        hash_version: u32,
        hash_count: u32,
        bits_per_entry: u32,
        payload: &[u8],
    ) -> Vec<u8> {
        let mut out = Vec::new();
        out.extend_from_slice(&hash_version.to_be_bytes());
        out.extend_from_slice(&hash_count.to_be_bytes());
        out.extend_from_slice(&bits_per_entry.to_be_bytes());
        out.extend_from_slice(payload);
        out
    }
}
