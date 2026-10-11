//! Lossless, chunk-level transformations of Roblox binary model files.
//!
//! Asset files are stored exactly as Roblox Studio serialized them, except that
//! every chunk is stored uncompressed. Studio compresses each chunk with LZ4
//! independently, which means that a one byte change to a property rewrites the
//! whole compressed chunk. Uncompressed chunks let git delta-compress the
//! history of asset files far better, and Studio loads them without issue.
//!
//! Nothing in this module interprets chunk contents. Decoding and re-encoding
//! instances with rbx_binary would silently drop properties whose types it does
//! not understand yet, so assets only ever go through these byte-level
//! transformations to guarantee that what Studio wrote is what Studio reads.

use std::fmt;

const FILE_MAGIC: &[u8] = b"<roblox!";
const FILE_SIGNATURE: &[u8] = b"\x89\xff\x0d\x0a\x1a\x0a";
const HEADER_LEN: usize = 32;
const CHUNK_HEADER_LEN: usize = 16;
const ZSTD_MAGIC: &[u8] = &[0x28, 0xb5, 0x2f, 0xfd];
const END_CHUNK_NAME: &[u8; 4] = b"END\0";

/// Chunks larger than this are rejected. Roblox's own format stores lengths as
/// u32, but no legitimate model chunk gets anywhere close to this, so a larger
/// value means the file is corrupt and we'd rather not allocate for it.
const MAX_CHUNK_LEN: usize = 1 << 30;

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RbxmError {
    NotABinaryModel,
    Truncated,
    ChunkTooLarge { name: String, len: usize },
    Decompression { name: String, message: String },
    MissingEndChunk,
}

impl fmt::Display for RbxmError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            RbxmError::NotABinaryModel => write!(f, "not a Roblox binary model file"),
            RbxmError::Truncated => write!(f, "binary model file is truncated"),
            RbxmError::ChunkTooLarge { name, len } => {
                write!(f, "chunk {name} is unreasonably large ({len} bytes)")
            }
            RbxmError::Decompression { name, message } => {
                write!(f, "could not decompress chunk {name}: {message}")
            }
            RbxmError::MissingEndChunk => write!(f, "binary model file has no END chunk"),
        }
    }
}

impl std::error::Error for RbxmError {}

/// How chunk payloads should be stored when a file is rewritten.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ChunkCompression {
    /// Every chunk is stored as-is. This is the canonical on-disk form.
    None,

    /// Chunks are LZ4 compressed when that makes them smaller. Used to keep
    /// transfers to Studio small; Studio reads this just like its own output.
    Lz4,
}

struct RawChunk<'a> {
    name: [u8; 4],
    reserved: [u8; 4],
    /// The chunk's payload, still in whatever form it was stored in.
    stored: &'a [u8],
    compressed: bool,
    uncompressed_len: usize,
}

impl RawChunk<'_> {
    fn display_name(&self) -> String {
        String::from_utf8_lossy(&self.name)
            .trim_end_matches('\0')
            .to_owned()
    }

    fn decompress(&self) -> Result<Vec<u8>, RbxmError> {
        if !self.compressed {
            return Ok(self.stored.to_vec());
        }

        let result = if self.stored.starts_with(ZSTD_MAGIC) {
            zstd::bulk::decompress(self.stored, self.uncompressed_len).map_err(|e| e.to_string())
        } else {
            lz4_flex::block::decompress(self.stored, self.uncompressed_len)
                .map_err(|e| e.to_string())
        };

        let data = result.map_err(|message| RbxmError::Decompression {
            name: self.display_name(),
            message,
        })?;

        if data.len() != self.uncompressed_len {
            return Err(RbxmError::Decompression {
                name: self.display_name(),
                message: format!(
                    "expected {} bytes, got {}",
                    self.uncompressed_len,
                    data.len()
                ),
            });
        }

        Ok(data)
    }
}

/// A binary model split into its parts.
struct ParsedModel<'a> {
    header: &'a [u8],
    chunks: Vec<RawChunk<'a>>,
    /// Any bytes that follow the END chunk.
    trailing: &'a [u8],
}

/// Splits a binary model into its header, chunks, and any bytes that trail the
/// END chunk, validating the container format along the way.
fn parse(contents: &[u8]) -> Result<ParsedModel<'_>, RbxmError> {
    if contents.len() < HEADER_LEN
        || !contents.starts_with(FILE_MAGIC)
        || &contents[FILE_MAGIC.len()..FILE_MAGIC.len() + FILE_SIGNATURE.len()] != FILE_SIGNATURE
    {
        return Err(RbxmError::NotABinaryModel);
    }

    let header = &contents[..HEADER_LEN];
    let mut cursor = HEADER_LEN;
    let mut chunks = Vec::new();

    loop {
        let chunk_header = contents
            .get(cursor..cursor + CHUNK_HEADER_LEN)
            .ok_or(RbxmError::MissingEndChunk)?;

        let name: [u8; 4] = chunk_header[0..4].try_into().unwrap();
        let compressed_len = read_u32(&chunk_header[4..8]) as usize;
        let uncompressed_len = read_u32(&chunk_header[8..12]) as usize;
        let reserved: [u8; 4] = chunk_header[12..16].try_into().unwrap();

        let stored_len = if compressed_len == 0 {
            uncompressed_len
        } else {
            compressed_len
        };

        if stored_len > MAX_CHUNK_LEN || uncompressed_len > MAX_CHUNK_LEN {
            return Err(RbxmError::ChunkTooLarge {
                name: String::from_utf8_lossy(&name).into_owned(),
                len: stored_len.max(uncompressed_len),
            });
        }

        let body_start = cursor + CHUNK_HEADER_LEN;
        let stored = contents
            .get(body_start..body_start + stored_len)
            .ok_or(RbxmError::Truncated)?;
        cursor = body_start + stored_len;

        chunks.push(RawChunk {
            name,
            reserved,
            stored,
            compressed: compressed_len != 0,
            uncompressed_len,
        });

        if &name == END_CHUNK_NAME {
            break;
        }
    }

    Ok(ParsedModel {
        header,
        chunks,
        trailing: &contents[cursor..],
    })
}

fn read_u32(bytes: &[u8]) -> u32 {
    u32::from_le_bytes(bytes.try_into().unwrap())
}

fn write_chunk(output: &mut Vec<u8>, chunk: &RawChunk, data: &[u8], compression: ChunkCompression) {
    output.extend_from_slice(&chunk.name);

    // Roblox never compresses its END chunk, and there's nothing to gain from
    // compressing tiny chunks either way.
    let compressed = match compression {
        ChunkCompression::Lz4 if &chunk.name != END_CHUNK_NAME => {
            let compressed = lz4_flex::block::compress(data);
            (compressed.len() < data.len()).then_some(compressed)
        }
        _ => None,
    };

    match compressed {
        Some(compressed) => {
            output.extend_from_slice(&(compressed.len() as u32).to_le_bytes());
            output.extend_from_slice(&(data.len() as u32).to_le_bytes());
            output.extend_from_slice(&chunk.reserved);
            output.extend_from_slice(&compressed);
        }
        None => {
            output.extend_from_slice(&0u32.to_le_bytes());
            output.extend_from_slice(&(data.len() as u32).to_le_bytes());
            output.extend_from_slice(&chunk.reserved);
            output.extend_from_slice(data);
        }
    }
}

/// Rewrites a binary model so that every chunk uses the given compression,
/// keeping every byte of every chunk's decompressed contents intact.
pub fn recompress(contents: &[u8], compression: ChunkCompression) -> Result<Vec<u8>, RbxmError> {
    let ParsedModel {
        header,
        chunks,
        trailing,
    } = parse(contents)?;

    let mut output = Vec::with_capacity(match compression {
        ChunkCompression::None => {
            HEADER_LEN
                + chunks
                    .iter()
                    .map(|chunk| CHUNK_HEADER_LEN + chunk.uncompressed_len)
                    .sum::<usize>()
                + trailing.len()
        }
        ChunkCompression::Lz4 => contents.len(),
    });

    output.extend_from_slice(header);

    for chunk in &chunks {
        if !chunk.compressed && compression == ChunkCompression::None {
            write_chunk(&mut output, chunk, chunk.stored, compression);
        } else {
            let data = chunk.decompress()?;
            write_chunk(&mut output, chunk, &data, compression);
        }
    }

    output.extend_from_slice(trailing);

    Ok(output)
}

/// Converts a binary model into the canonical form that asset files are
/// stored in: identical to the input except that no chunk is compressed.
///
/// Two serializations of the same instances produce the same canonical bytes
/// regardless of how they were compressed, so hashes of canonical bytes can be
/// used to compare asset contents.
pub fn canonicalize(contents: &[u8]) -> Result<Vec<u8>, RbxmError> {
    recompress(contents, ChunkCompression::None)
}

/// Whether the given binary model is already in canonical form, meaning that
/// none of its chunks are compressed.
pub fn is_canonical(contents: &[u8]) -> Result<bool, RbxmError> {
    let chunks = parse(contents)?.chunks;
    Ok(chunks.iter().all(|chunk| !chunk.compressed))
}

#[cfg(test)]
mod test {
    use super::*;

    use rbx_dom_weak::{InstanceBuilder, WeakDom};

    fn sample_model(compression: rbx_binary::CompressionType) -> Vec<u8> {
        let mut dom = WeakDom::new(InstanceBuilder::new("DataModel"));
        let model = dom.insert(
            dom.root_ref(),
            InstanceBuilder::new("Model").with_name("Tree"),
        );
        for i in 0..50 {
            dom.insert(
                model,
                InstanceBuilder::new("Part")
                    .with_name(format!("Leaf{i}"))
                    .with_property("Transparency", i as f32 / 50.0),
            );
        }

        let mut output = Vec::new();
        rbx_binary::Serializer::new()
            .compression_type(compression)
            .serialize(&mut output, &dom, &[model])
            .unwrap();
        output
    }

    #[test]
    fn canonicalize_matches_uncompressed_serialization() {
        let lz4 = sample_model(rbx_binary::CompressionType::Lz4);
        let uncompressed = sample_model(rbx_binary::CompressionType::None);

        assert_ne!(lz4, uncompressed);
        assert_eq!(canonicalize(&lz4).unwrap(), uncompressed);
    }

    #[test]
    fn canonicalize_handles_zstd() {
        let zstd = sample_model(rbx_binary::CompressionType::Zstd);
        let uncompressed = sample_model(rbx_binary::CompressionType::None);

        assert_eq!(canonicalize(&zstd).unwrap(), uncompressed);
    }

    #[test]
    fn canonicalize_is_idempotent() {
        let lz4 = sample_model(rbx_binary::CompressionType::Lz4);
        let canonical = canonicalize(&lz4).unwrap();

        assert_eq!(canonicalize(&canonical).unwrap(), canonical);
        assert!(is_canonical(&canonical).unwrap());
        assert!(!is_canonical(&lz4).unwrap());
    }

    #[test]
    fn lz4_round_trip_is_lossless() {
        let canonical = canonicalize(&sample_model(rbx_binary::CompressionType::Lz4)).unwrap();
        let compressed = recompress(&canonical, ChunkCompression::Lz4).unwrap();

        assert!(compressed.len() < canonical.len());
        assert_eq!(canonicalize(&compressed).unwrap(), canonical);

        // The compressed form must still be readable as a normal model.
        let dom = rbx_binary::from_reader(compressed.as_slice()).unwrap();
        let model = dom.get_by_ref(dom.root().children()[0]).unwrap();
        assert_eq!(model.name, "Tree");
        assert_eq!(model.children().len(), 50);
    }

    #[test]
    fn trailing_bytes_are_preserved() {
        let mut contents = sample_model(rbx_binary::CompressionType::Lz4);
        contents.extend_from_slice(b"trailing");

        let canonical = canonicalize(&contents).unwrap();
        assert!(canonical.ends_with(b"trailing"));
    }

    #[test]
    fn rejects_non_models() {
        assert_eq!(
            canonicalize(b"hello world, this is not a model at all").unwrap_err(),
            RbxmError::NotABinaryModel
        );
        assert_eq!(canonicalize(b"").unwrap_err(), RbxmError::NotABinaryModel);
    }

    #[test]
    fn rejects_truncated_models() {
        let contents = sample_model(rbx_binary::CompressionType::Lz4);

        for len in [
            HEADER_LEN,
            HEADER_LEN + 10,
            contents.len() / 2,
            contents.len() - 1,
        ] {
            assert!(
                canonicalize(&contents[..len]).is_err(),
                "truncating to {len} bytes should fail"
            );
        }
    }

    #[test]
    fn rejects_corrupt_compressed_chunks() {
        let mut contents = sample_model(rbx_binary::CompressionType::Lz4);

        // Corrupt the body of the first chunk, which is compressed.
        let body_start = HEADER_LEN + CHUNK_HEADER_LEN;
        for byte in &mut contents[body_start..body_start + 8] {
            *byte = 0xff;
        }

        assert!(matches!(
            canonicalize(&contents),
            Err(RbxmError::Decompression { .. })
        ));
    }
}
