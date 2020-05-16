//! Region file format storage for chunks.
//!
//! More information about format can be found https://wiki.vg/Region_Files.
//!
//! # Example
//!
//! ## Read
//!
//! ```
//! use anvil_region::AnvilChunkProvider;
//!
//! let chunk_provider = AnvilChunkProvider::new("test/region");
//!
//! let chunk_compound_tag = chunk_provider.load_chunk(4, 2).unwrap();
//! let level_compound_tag = chunk_compound_tag.get_compound_tag("Level").unwrap();
//!
//! assert_eq!(level_compound_tag.get_i32("xPos").unwrap(), 4);
//! assert_eq!(level_compound_tag.get_i32("zPos").unwrap(), 2);
//! ```
//!
//! ## Write
//!
//! ```
//! use anvil_region::AnvilChunkProvider;
//! use nbt::CompoundTag;
//!
//! let chunk_provider = AnvilChunkProvider::new("test/region");
//! let mut chunk_compound_tag = CompoundTag::new();
//! let mut level_compound_tag = CompoundTag::new();
//!
//! // To simplify example we add only coordinates.
//! // Full list of required tags https://minecraft.gamepedia.com/Chunk_format.
//! level_compound_tag.insert_i32("xPos", 31);
//! level_compound_tag.insert_i32("zPos", 16);
//!
//! chunk_compound_tag.insert_compound_tag("Level", level_compound_tag);
//!
//! chunk_provider.save_chunk(31, 16, chunk_compound_tag);
//! ```
use bitvec::prelude::*;
use byteorder::{BigEndian, ReadBytesExt, WriteBytesExt};
use nbt::decode::TagDecodeError;
use nbt::decode::{read_gzip_compound_tag, read_zlib_compound_tag};
use nbt::encode::write_zlib_compound_tag;
use nbt::CompoundTag;
use std::fs::{File, OpenOptions};
use std::io::{Cursor, Read, Seek, SeekFrom, Write};
use std::path::Path;
use std::time::{SystemTime, UNIX_EPOCH};
use std::{fs, io};

/// Amount of chunks in region.
const REGION_CHUNKS: usize = 1024;
/// Length of chunks metadata in region.
const REGION_CHUNKS_METADATA_LENGTH: usize = 2 * REGION_CHUNKS;
/// Region header length in bytes.
const REGION_HEADER_BYTES_LENGTH: u64 = 8 * REGION_CHUNKS as u64;
/// Region sector length in bytes.
const REGION_SECTOR_BYTES_LENGTH: u16 = 4096;
/// Maximum chunk length in bytes.
const CHUNK_MAXIMUM_BYTES_LENGTH: u32 = REGION_SECTOR_BYTES_LENGTH as u32 * 256;
/// Gzip compression type value.
const GZIP_COMPRESSION_TYPE: u8 = 1;
/// Zlib compression type value.
const ZLIB_COMPRESSION_TYPE: u8 = 2;

/// Possible errors while loading the chunk.
#[derive(Debug)]
pub enum ChunkLoadError {
    /// Region at specified coordinates not found.
    RegionNotFound { region_x: i32, region_z: i32 },
    /// Chunk at specified coordinates inside region not found.
    ChunkNotFound { chunk_x: u8, chunk_z: u8 },
    /// Chunk length overlaps declared maximum.
    ///
    /// This should not occur under normal conditions.
    ///
    /// Region file are corrupted.
    LengthExceedsMaximum {
        /// Chunk length.
        length: u32,
        /// Chunk maximum expected length.
        maximum_length: u32,
    },
    /// Currently are only 2 types of compression: Gzip and Zlib.
    ///
    /// This should not occur under normal conditions.
    ///
    /// Region file are corrupted or was introduced new compression type.
    UnsupportedCompressionScheme {
        /// Compression scheme type id.
        compression_scheme: u8,
    },
    /// I/O Error which happened while were reading chunk data from region file.
    ReadError { io_error: io::Error },
    /// Error while decoding binary data to NBT tag.
    ///
    /// This should not occur under normal conditions.
    ///
    /// Region file are corrupted or a developer error in the NBT library.
    TagDecodeError { tag_decode_error: TagDecodeError },
}

impl From<io::Error> for ChunkLoadError {
    fn from(io_error: io::Error) -> Self {
        ChunkLoadError::ReadError { io_error }
    }
}

impl From<TagDecodeError> for ChunkLoadError {
    fn from(tag_decode_error: TagDecodeError) -> Self {
        ChunkLoadError::TagDecodeError { tag_decode_error }
    }
}

/// Possible errors while saving the chunk.
#[derive(Debug)]
pub enum ChunkSaveError {
    /// Chunk length exceeds 1 MB.
    ///
    /// This should not occur under normal conditions.
    LengthExceedsMaximum {
        /// Chunk length.
        length: u32,
    },
    /// I/O Error which happened while were writing chunk data to region file.
    WriteError { io_error: io::Error },
}

impl From<io::Error> for ChunkSaveError {
    fn from(io_error: io::Error) -> Self {
        ChunkSaveError::WriteError { io_error }
    }
}

pub struct AnvilChunkProvider<'a> {
    /// Folder where region files located.
    folder_path: &'a Path,
}

impl<'a> AnvilChunkProvider<'a> {
    pub fn new(folder: &'a str) -> Self {
        let folder_path = Path::new(folder);

        AnvilChunkProvider { folder_path }
    }

    /// Load chunks from the specified coordinates.
    ///
    /// # Example
    ///
    /// ```
    /// use anvil_region::AnvilChunkProvider;
    ///
    /// let chunk_provider = AnvilChunkProvider::new("test/region");
    ///
    /// let chunk_compound_tag = chunk_provider.load_chunk(4, 2).unwrap();
    /// let level_compound_tag = chunk_compound_tag.get_compound_tag("Level").unwrap();
    ///
    /// assert_eq!(level_compound_tag.get_i32("xPos").unwrap(), 4);
    /// assert_eq!(level_compound_tag.get_i32("zPos").unwrap(), 2);
    /// ```
    pub fn load_chunk(&self, chunk_x: i32, chunk_z: i32) -> Result<CompoundTag, ChunkLoadError> {
        let region_x = chunk_x >> 5;
        let region_z = chunk_z >> 5;

        let region_chunk_x = (chunk_x & 31) as u8;
        let region_chunk_z = (chunk_z & 31) as u8;

        let region_name = format!("r.{}.{}.mca", region_x, region_z);
        let region_path = self.folder_path.join(region_name);

        if !region_path.exists() {
            return Err(ChunkLoadError::RegionNotFound { region_x, region_z });
        }

        // TODO: Cache region files.
        let mut region = AnvilRegion::new(region_path)?;

        region.read_chunk(region_chunk_x, region_chunk_z)
    }

    /// Saves chunk data to the specified coordinates.
    ///
    /// # Example
    ///
    /// ```
    /// use anvil_region::AnvilChunkProvider;
    /// use nbt::CompoundTag;
    ///
    /// let chunk_provider = AnvilChunkProvider::new("test/region");
    /// let mut chunk_compound_tag = CompoundTag::new();
    /// let mut level_compound_tag = CompoundTag::new();
    ///
    /// // To simplify example we add only coordinates.
    /// // Full list of required tags https://minecraft.gamepedia.com/Chunk_format.
    /// level_compound_tag.insert_i32("xPos", 31);
    /// level_compound_tag.insert_i32("zPos", 16);
    ///
    /// chunk_compound_tag.insert_compound_tag("Level", level_compound_tag);
    ///
    /// chunk_provider.save_chunk(31, 16, chunk_compound_tag);
    /// ```
    pub fn save_chunk(
        &self,
        chunk_x: i32,
        chunk_z: i32,
        chunk_compound_tag: CompoundTag,
    ) -> Result<(), ChunkSaveError> {
        if !self.folder_path.exists() {
            fs::create_dir(self.folder_path)?;
        }

        let region_x = chunk_x >> 5;
        let region_z = chunk_z >> 5;

        let region_chunk_x = (chunk_x & 31) as u8;
        let region_chunk_z = (chunk_z & 31) as u8;

        let region_name = format!("r.{}.{}.mca", region_x, region_z);
        let region_path = self.folder_path.join(region_name);

        // TODO: Cache region files.
        let mut region = AnvilRegion::new(region_path)?;

        region.write_chunk(region_chunk_x, region_chunk_z, chunk_compound_tag)
    }
}

/// Region represents a 32x32 group of chunks.
struct AnvilRegion {
    /// File in which region are stored.
    file: File,
    /// Array of chunks metadata.
    chunks_metadata: [AnvilChunkMetadata; REGION_CHUNKS],
    /// Used sectors for chunks data.
    used_sectors: BitVec,
}

/// Chunk metadata are stored in header.
#[derive(Copy, Clone, Default, Debug, Eq, PartialEq)]
struct AnvilChunkMetadata {
    /// Sector index from which starts chunk data.
    sector_index: u32,
    /// Amount of sectors used to store chunk.
    sectors: u8,
    /// Last time in seconds when chunk was modified.
    last_modified_timestamp: u32,
}

impl AnvilChunkMetadata {
    fn new(sector_index: u32, sectors: u8, last_modified_timestamp: u32) -> Self {
        AnvilChunkMetadata {
            sector_index,
            sectors,
            last_modified_timestamp,
        }
    }

    fn update_last_modified_timestamp(&mut self) {
        let system_time = SystemTime::now();
        let time = system_time.duration_since(UNIX_EPOCH).unwrap();

        self.last_modified_timestamp = time.as_secs() as u32
    }

    fn is_empty(&self) -> bool {
        self.sectors == 0
    }
}

impl AnvilRegion {
    fn new<P: AsRef<Path>>(path: P) -> Result<Self, io::Error> {
        let mut file = OpenOptions::new()
            .write(true)
            .read(true)
            .create(true)
            .open(path)?;

        // If necessary, extend the file length to the length of the header.
        if REGION_HEADER_BYTES_LENGTH > file.metadata()?.len() {
            file.set_len(REGION_HEADER_BYTES_LENGTH)?;
        }

        let chunks_metadata = Self::read_header(&mut file)?;
        let total_sectors = file.metadata()?.len() as u32 / REGION_SECTOR_BYTES_LENGTH as u32;
        let used_sectors = Self::used_sectors(total_sectors, &chunks_metadata);

        let region = AnvilRegion {
            file,
            chunks_metadata,
            used_sectors,
        };

        Ok(region)
    }

    /// First 8KB of file are header of 1024 offsets and 1024 timestamps.
    fn read_header(file: &mut File) -> Result<[AnvilChunkMetadata; REGION_CHUNKS], io::Error> {
        let mut chunks_metadata = [Default::default(); REGION_CHUNKS];
        let mut values = [0u32; REGION_CHUNKS_METADATA_LENGTH];

        for index in 0..REGION_CHUNKS_METADATA_LENGTH {
            values[index] = file.read_u32::<BigEndian>()?;
        }

        for index in 0..REGION_CHUNKS {
            let last_modified_timestamp = values[REGION_CHUNKS + index];
            let offset = values[index];

            let sector_index = offset >> 8;
            let sectors = (offset & 0xFF) as u8;

            let metadata = AnvilChunkMetadata::new(sector_index, sectors, last_modified_timestamp);
            chunks_metadata[index] = metadata;
        }

        return Ok(chunks_metadata);
    }

    /// Calculates used sectors.
    fn used_sectors(total_sectors: u32, chunks_metadata: &[AnvilChunkMetadata]) -> BitVec {
        let mut used_sectors = bitvec![0; total_sectors as usize];
        used_sectors.set(0, true);
        used_sectors.set(1, true);
        for metadata in chunks_metadata {
            if metadata.is_empty() {
                continue;
            }

            let start_index = metadata.sector_index as usize;
            let end_index = start_index + metadata.sectors as usize;

            for index in start_index..end_index {
                if index < used_sectors.len() {
                    used_sectors.set(index, true);
                }
            }
        }

        used_sectors
    }

    fn read_chunk(&mut self, chunk_x: u8, chunk_z: u8) -> Result<CompoundTag, ChunkLoadError> {
        let metadata = self.get_metadata(chunk_x, chunk_z);

        if metadata.is_empty() {
            return Err(ChunkLoadError::ChunkNotFound { chunk_x, chunk_z });
        }

        let seek_offset = metadata.sector_index as u64 * REGION_SECTOR_BYTES_LENGTH as u64;
        let maximum_length = (metadata.sectors as u32 * REGION_SECTOR_BYTES_LENGTH as u32)
            .min(CHUNK_MAXIMUM_BYTES_LENGTH);

        self.file.seek(SeekFrom::Start(seek_offset))?;
        let length = self.file.read_u32::<BigEndian>()?;

        if length > maximum_length {
            return Err(ChunkLoadError::LengthExceedsMaximum {
                length,
                maximum_length,
            });
        }

        let compression_scheme = self.file.read_u8()?;
        let mut compressed_buffer = vec![0u8; (length - 1) as usize];
        self.file.read_exact(&mut compressed_buffer)?;

        let mut cursor = Cursor::new(&compressed_buffer);

        match compression_scheme {
            GZIP_COMPRESSION_TYPE => Ok(read_gzip_compound_tag(&mut cursor)?),
            ZLIB_COMPRESSION_TYPE => Ok(read_zlib_compound_tag(&mut cursor)?),
            _ => Err(ChunkLoadError::UnsupportedCompressionScheme { compression_scheme }),
        }
    }

    fn write_chunk(
        &mut self,
        chunk_x: u8,
        chunk_z: u8,
        chunk_compound_tag: CompoundTag,
    ) -> Result<(), ChunkSaveError> {
        let mut buffer = Vec::new();

        buffer.write_u8(ZLIB_COMPRESSION_TYPE)?;
        write_zlib_compound_tag(&mut buffer, chunk_compound_tag)?;

        // 4 bytes for data length.
        let length = (buffer.len() + 4) as u32;

        if length > CHUNK_MAXIMUM_BYTES_LENGTH {
            return Err(ChunkSaveError::LengthExceedsMaximum { length });
        }

        let mut metadata = self.find_place(chunk_x, chunk_z, length)?;
        let seek_offset = metadata.sector_index as u64 * REGION_SECTOR_BYTES_LENGTH as u64;

        self.file.seek(SeekFrom::Start(seek_offset))?;
        self.file.write_u32::<BigEndian>(buffer.len() as u32)?;
        self.file.write_all(&buffer)?;

        // Padding to align sector.
        let padding = REGION_SECTOR_BYTES_LENGTH - length as u16 % REGION_SECTOR_BYTES_LENGTH;

        for _ in 0..padding {
            self.file.write_u8(0)?;
        }

        metadata.update_last_modified_timestamp();
        self.update_metadata(chunk_x, chunk_z, metadata)?;

        Ok(())
    }

    fn metadata_index(chunk_x: u8, chunk_z: u8) -> usize {
        assert!(32 > chunk_x, "Region chunk x coordinate out of bounds");
        assert!(32 > chunk_z, "Region chunk y coordinate out of bounds");

        chunk_x as usize + chunk_z as usize * 32
    }

    /// Returns chunk metadata at specified coordinates.
    fn get_metadata(&self, chunk_x: u8, chunk_z: u8) -> AnvilChunkMetadata {
        self.chunks_metadata[Self::metadata_index(chunk_x, chunk_z)]
    }

    /// Finds a place where chunk data of a given length can be put.
    ///
    /// If cannot find a place to put chunk data will extend file.
    fn find_place(
        &mut self,
        chunk_x: u8,
        chunk_z: u8,
        chunk_length: u32,
    ) -> Result<AnvilChunkMetadata, io::Error> {
        let sectors_required = (chunk_length / REGION_SECTOR_BYTES_LENGTH as u32) as u8 + 1;
        let metadata = self.get_metadata(chunk_x, chunk_z);

        // Can place chunk in the old sectors.
        if metadata.sectors == sectors_required {
            return Ok(metadata);
        }

        // Release used sectors.
        for i in 0..metadata.sectors {
            let sector_index = metadata.sector_index as usize + i as usize;
            if sector_index < self.used_sectors.capacity() {
                self.used_sectors.set(sector_index, false);
            }
        }
        let file_length = self.file.metadata()?.len();
        let total_sectors = file_length / REGION_SECTOR_BYTES_LENGTH as u64;

        // Trying to find enough big gap between sectors to put chunk.
        let mut sectors_free = 0;

        for sector_index in 0..total_sectors {
            if sector_index >= self.used_sectors.capacity() as u64 || self.used_sectors[sector_index as usize] {
                sectors_free = 0;
                continue;
            }

            sectors_free += 1;

            // Can put chunk in gap.
            if sectors_free == sectors_required {
                let put_sector_index = sector_index as u32 - sectors_free as u32;

                // Acquire used sectors.
                for i in 0..sectors_free {
                    if i as usize >= self.used_sectors.capacity(){
                        return Err(io::Error::last_os_error())
                    }
                    let sector_index = put_sector_index as usize + i as usize;
                    self.used_sectors.set(sector_index, true);
                }

                return Ok(AnvilChunkMetadata::new(put_sector_index, sectors_free, 0));
            }
        }

        // Extending file because cannot find a place to put chunk data.
        let extend_sectors = sectors_required - sectors_free;
        let extend_length = (REGION_SECTOR_BYTES_LENGTH * extend_sectors as u16) as u64;
        self.file.set_len(file_length + extend_length)?;

        // Mark new sectors as used.
        for _ in 0..extend_sectors {
            self.used_sectors.push(true);
        }

        return Ok(AnvilChunkMetadata::new(
            total_sectors as u32 - sectors_free as u32,
            sectors_required,
            0,
        ));
    }

    /// Updates chunk metadata.
    fn update_metadata(
        &mut self,
        chunk_x: u8,
        chunk_z: u8,
        metadata: AnvilChunkMetadata,
    ) -> Result<(), io::Error> {
        let metadata_index = Self::metadata_index(chunk_x, chunk_z);
        self.chunks_metadata[metadata_index] = metadata;

        let start_seek_offset = SeekFrom::Start((metadata_index * 4) as u64);
        let offset = (metadata.sector_index << 8) | metadata.sectors as u32;

        self.file.seek(start_seek_offset)?;
        self.file.write_u32::<BigEndian>(offset)?;

        let next_seek_offset = SeekFrom::Current(REGION_SECTOR_BYTES_LENGTH as i64 - 4);
        let last_modified_timestamp = metadata.last_modified_timestamp;

        self.file.seek(next_seek_offset)?;
        self.file.write_u32::<BigEndian>(last_modified_timestamp)?;

        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use crate::{
        AnvilChunkMetadata, AnvilChunkProvider, AnvilRegion, ChunkLoadError,
        REGION_HEADER_BYTES_LENGTH, REGION_SECTOR_BYTES_LENGTH,
    };
    use nbt::CompoundTag;
    use std::io::Read;
    use std::path::Path;
    use tempfile::NamedTempFile;

    #[test]
    fn test_empty_header_write() {
        let file = NamedTempFile::new().unwrap();
        let region = AnvilRegion::new(file.path()).unwrap();
        let file_length = region.file.metadata().unwrap().len();

        assert_eq!(file_length, REGION_HEADER_BYTES_LENGTH);
    }

    #[test]
    fn test_empty_region_init() {
        let mut file = NamedTempFile::new().unwrap();
        AnvilRegion::new(file.path()).unwrap();

        let mut vec = Vec::new();
        file.read_to_end(&mut vec).unwrap();

        assert_eq!(vec, include_bytes!("../test/empty_region.mca").to_vec());
    }

    #[test]
    fn test_header_read() {
        let expected_data = vec![
            AnvilChunkMetadata::new(61, 2, 1570215508),
            AnvilChunkMetadata::new(102, 2, 1570215511),
            AnvilChunkMetadata::new(177, 2, 1570215515),
            AnvilChunkMetadata::new(265, 2, 1570215519),
            AnvilChunkMetadata::new(56, 2, 1570215508),
        ];

        let path = Path::new("test/region/r.0.0.mca");
        assert!(path.exists());

        let region = AnvilRegion::new(path).unwrap();

        for (index, expected_chunk_metadata) in expected_data.iter().enumerate() {
            let chunk_metadata = region.chunks_metadata[256 + index];

            assert_eq!(&chunk_metadata, expected_chunk_metadata);
        }
    }

    #[test]
    fn test_read_chunk_data() {
        let path = Path::new("test/region/r.0.0.mca");
        assert!(path.exists());

        let mut region = AnvilRegion::new(path).unwrap();
        let compound_tag = region.read_chunk(15, 3).unwrap();
        let level_tag = compound_tag.get_compound_tag("Level").unwrap();

        assert_eq!(level_tag.get_i32("xPos").unwrap(), 15);
        assert_eq!(level_tag.get_i32("zPos").unwrap(), 3);
    }

    #[test]
    fn test_read_chunk_empty() {
        let path = Path::new("test/empty_region.mca");
        assert!(path.exists());

        let mut region = AnvilRegion::new(path).unwrap();
        let load_error = region.read_chunk(0, 0).err().unwrap();

        match load_error {
            ChunkLoadError::ChunkNotFound { chunk_x, chunk_z } => {
                assert_eq!(chunk_x, 0);
                assert_eq!(chunk_z, 0);
            }
            _ => panic!("Expected `ChunkNotFound` but got `{:?}`", load_error),
        }
    }

    #[test]
    fn test_load_chunk_no_folder() {
        let chunk_provider = AnvilChunkProvider::new("no-folder");
        let load_error = chunk_provider.load_chunk(4, 4).err().unwrap();

        match load_error {
            ChunkLoadError::RegionNotFound { region_x, region_z } => {
                assert_eq!(region_x, 0);
                assert_eq!(region_z, 0);
            }
            _ => panic!("Expected `RegionNotFound` but got `{:?}", load_error),
        }
    }

    #[test]
    fn test_load_chunk_no_region() {
        let chunk_provider = AnvilChunkProvider::new("test/region");
        let load_error = chunk_provider.load_chunk(100, 100).err().unwrap();

        match load_error {
            ChunkLoadError::RegionNotFound { region_x, region_z } => {
                assert_eq!(region_x, 3);
                assert_eq!(region_z, 3);
            }
            _ => panic!("Expected `RegionNotFound` but got `{:?}", load_error),
        }
    }

    #[test]
    fn test_load_chunk_chunk_not_found() {
        let chunk_provider = AnvilChunkProvider::new("test/region");
        let load_error = chunk_provider.load_chunk(15, 14).err().unwrap();

        match load_error {
            ChunkLoadError::ChunkNotFound { chunk_x, chunk_z } => {
                assert_eq!(chunk_x, 15);
                assert_eq!(chunk_z, 14);
            }
            _ => panic!("Expected `ChunkNotFound` but got `{:?}", load_error),
        }
    }

    #[test]
    fn test_update_metadata() {
        let mut file = NamedTempFile::new().unwrap();
        let mut region = AnvilRegion::new(file.path()).unwrap();

        let mut metadata = AnvilChunkMetadata::new(500, 10, 0);
        metadata.update_last_modified_timestamp();

        region.update_metadata(15, 15, metadata).unwrap();
        let chunks_metadata = AnvilRegion::read_header(file.as_file_mut()).unwrap();
        let metadata_index = AnvilRegion::metadata_index(15, 15);

        // In memory metadata.
        assert_eq!(region.get_metadata(15, 15), metadata);
        // Written to file metadata.
        assert_eq!(chunks_metadata[metadata_index], metadata);
    }

    #[test]
    fn test_write_chunk_with_file_extend() {
        let file = NamedTempFile::new().unwrap();
        let mut region = AnvilRegion::new(file.path()).unwrap();

        let mut write_compound_tag = CompoundTag::new();
        write_compound_tag.insert_bool("test_bool", true);
        write_compound_tag.insert_str("test_str", "test");

        region.write_chunk(15, 15, write_compound_tag).unwrap();

        assert_eq!(
            file.as_file().metadata().unwrap().len(),
            REGION_HEADER_BYTES_LENGTH + REGION_SECTOR_BYTES_LENGTH as u64
        );

        assert_eq!(region.used_sectors.len(), 3);

        let read_compound_tag = region.read_chunk(15, 15).unwrap();

        assert!(read_compound_tag.get_bool("test_bool").unwrap());
        assert_eq!(read_compound_tag.get_str("test_str").unwrap(), "test");
    }

    #[test]
    fn test_write_chunk_same_sector() {
        let file = NamedTempFile::new().unwrap();
        let mut region = AnvilRegion::new(file.path()).unwrap();

        let mut write_compound_tag_1 = CompoundTag::new();
        write_compound_tag_1.insert_bool("test_bool", true);
        write_compound_tag_1.insert_str("test_str", "test");
        write_compound_tag_1.insert_f32("test_f32", 1.23);

        region.write_chunk(15, 15, write_compound_tag_1).unwrap();

        let mut write_compound_tag_2 = CompoundTag::new();
        write_compound_tag_2.insert_bool("test_bool", true);
        write_compound_tag_2.insert_str("test_str", "test");

        region.write_chunk(15, 15, write_compound_tag_2).unwrap();

        assert_eq!(
            file.as_file().metadata().unwrap().len(),
            REGION_HEADER_BYTES_LENGTH + REGION_SECTOR_BYTES_LENGTH as u64
        );

        assert_eq!(region.used_sectors.len(), 3);

        let read_compound_tag = region.read_chunk(15, 15).unwrap();

        assert!(read_compound_tag.get_bool("test_bool").unwrap());
        assert_eq!(read_compound_tag.get_str("test_str").unwrap(), "test");
        assert!(!read_compound_tag.contains_key("test_f32"));
    }

    #[test]
    fn test_write_chunk_same_sector_with_file_expand() {
        let file = NamedTempFile::new().unwrap();
        let mut region = AnvilRegion::new(file.path()).unwrap();

        let mut write_compound_tag_1 = CompoundTag::new();
        write_compound_tag_1.insert_bool("test_bool", true);
        write_compound_tag_1.insert_str("test_str", "test");

        region.write_chunk(15, 15, write_compound_tag_1).unwrap();

        let mut write_compound_tag_2 = CompoundTag::new();
        let mut i32_vec = Vec::new();

        // Extending chunk to second sector.
        // Due compression we need to write more than 1024 ints.
        for i in 0..3000 {
            i32_vec.push(i)
        }

        write_compound_tag_2.insert_i32_vec("test_i32_vec", i32_vec);

        region.write_chunk(15, 15, write_compound_tag_2).unwrap();

        assert_eq!(
            file.as_file().metadata().unwrap().len(),
            REGION_HEADER_BYTES_LENGTH + REGION_SECTOR_BYTES_LENGTH as u64 * 2
        );

        assert_eq!(region.used_sectors.len(), 4);
    }

    #[test]
    fn test_write_chunk_with_insert_in_middle() {
        let file = NamedTempFile::new().unwrap();
        let mut region = AnvilRegion::new(file.path()).unwrap();

        let mut write_compound_tag = CompoundTag::new();
        write_compound_tag.insert_bool("test_bool", true);
        write_compound_tag.insert_str("test_str", "test");

        for _ in 3..6 {
            region.used_sectors.push(true);
        }

        region.used_sectors.set(3, false);

        let length = REGION_HEADER_BYTES_LENGTH + REGION_SECTOR_BYTES_LENGTH as u64 * 3;
        file.as_file().set_len(length).unwrap();

        region.write_chunk(15, 15, write_compound_tag).unwrap();

        assert!(region.used_sectors.get(4).unwrap());
        assert_eq!(file.as_file().metadata().unwrap().len(), length);
        assert_eq!(region.used_sectors.len(), 5);
    }

    #[test]
    fn test_write_chunk_not_enough_gap() {
        let file = NamedTempFile::new().unwrap();
        let mut region = AnvilRegion::new(file.path()).unwrap();

        let mut write_compound_tag_1 = CompoundTag::new();
        write_compound_tag_1.insert_bool("test_bool", true);
        write_compound_tag_1.insert_str("test_str", "test");

        region
            .write_chunk(15, 15, write_compound_tag_1.clone())
            .unwrap();

        region.write_chunk(0, 0, write_compound_tag_1).unwrap();

        let mut write_compound_tag_2 = CompoundTag::new();
        let mut i32_vec = Vec::new();

        // Extending chunk to second sector.
        // Due compression we need to write more than 1024 ints.
        for i in 0..3000 {
            i32_vec.push(i)
        }

        write_compound_tag_2.insert_i32_vec("test_i32_vec", i32_vec);

        region.write_chunk(15, 15, write_compound_tag_2).unwrap();

        assert_eq!(region.used_sectors.clone().into_vec()[0], 0b00111011);
        assert_eq!(region.used_sectors.len(), 6);
        assert_eq!(
            file.as_file().metadata().unwrap().len(),
            REGION_HEADER_BYTES_LENGTH + REGION_SECTOR_BYTES_LENGTH as u64 * 4
        );
    }

    #[test]
    fn test_used_sectors_only_header() {
        let empty_chunks_metadata = Vec::new();
        let used_sectors = AnvilRegion::used_sectors(8, &empty_chunks_metadata);

        // Two sectors are used for header data.
        assert_eq!(used_sectors.into_vec()[0], 0b00000011);
    }

    #[test]
    fn test_used_sectors_all() {
        let chunks_metadata = vec![AnvilChunkMetadata::new(2, 6, 0)];
        let used_sectors = AnvilRegion::used_sectors(8, &chunks_metadata);

        assert_eq!(used_sectors.into_vec()[0], 0b11111111);
    }

    #[test]
    fn test_used_sectors_partially() {
        let chunks_metadata = vec![
            AnvilChunkMetadata::new(3, 3, 0),
            AnvilChunkMetadata::new(8, 1, 0),
        ];

        let used_sectors = AnvilRegion::used_sectors(10, &chunks_metadata);
        let used_vec = used_sectors.into_vec();

        assert_eq!(used_vec[0], 0b100111011);
    }
}
