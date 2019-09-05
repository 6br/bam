use std::fs::File;
use std::io::{Read, Seek, Result, Error, Write};
use std::io::ErrorKind::InvalidInput;
use std::path::{Path, PathBuf};
use std::result;

use super::index::{self, Index};
use super::record;
use super::bgzip;
use super::header::Header;

/// Iterator over bam records.
///
/// You can use the single record:
/// ```rust
///let mut record = bam::Record::new();
///loop {
///    // reader: impl BamReader
///    match reader.read_into(&mut record) {
///    // New record is saved into record
///        Ok(()) => {},
///        // NoMoreRecords represents stop iteration
///        Err(bam::Error::NoMoreRecords) => break,
///        Err(e) => panic!("{}", e),
///    }
///    // Do somethind with the record
///}
///```
/// Or you can just iterate over records:
/// ```rust
///for record in reader {
///    let record = record.unwrap();
///    // Do somethind with the record
///}
///```
pub trait BamReader: Iterator<Item = result::Result<record::Record, record::Error>> {
    /// Writes the next record into `record`. It allows to skip excessive memory allocation.
    ///
    /// # Errors
    ///
    /// If there are no more records to iterate over, the function returns
    /// [NoMoreRecords](../record/enum.Error.html#variant.NoMoreRecords) error.
    ///
    /// If the record was corrupted, the function returns
    /// [Corrupted](../record/enum.Error.html#variant.Corrupted) error.
    /// If the record was truncated or the reading failed for a different reason, the function
    /// returns [Truncated](../record/enum.Error.html#variant.Truncated) error.
    fn read_into(&mut self, record: &mut record::Record) -> result::Result<(), record::Error>;
}

/// Iterator over records in a specific region. Implements [BamReader](trait.BamReader.html) trait.
///
/// If possible, create a single record using [Record::new](../record/struct.Record.html#method.new)
/// and then use [read_into](trait.BamReader.html#method.read_into) instead of iterating,
/// as it saves time on allocation.
pub struct RegionViewer<'a, R: Read + Seek> {
    chunks_reader: bgzip::ChunksReader<'a, R>,
    start: i32,
    end: i32,
    predicate: Box<Fn(&record::Record) -> bool>,
}

impl<'a, R: Read + Seek> BamReader for RegionViewer<'a, R> {
    fn read_into(&mut self, record: &mut record::Record) -> result::Result<(), record::Error> {
        loop {
            record.fill_from(&mut self.chunks_reader)?;
            if !record.is_mapped() {
                continue;
            }
            // Reads are sorted, so no more reads would be in the region.
            if record.start() >= self.end {
                return Err(record::Error::NoMoreRecords);
            }
            if !(self.predicate)(&record) {
                continue;
            }
            if record.bin() > index::MAX_BIN {
                return Err(record::Error::Corrupted(
                    "Read has BAI bin bigger than max possible value"));
            }
            let (min_start, max_end) = index::bin_to_region(record.bin());
            if min_start >= self.start && max_end <= self.end {
                return Ok(());
            }

            let record_end = record.calculate_end();
            if record_end != -1 && record_end < record.start() {
                return Err(record::Error::Corrupted("aln_end < aln_start"));
            }
            if record_end > self.start {
                return Ok(());
            }
        }
    }
}

/// Iterator over records.
///
/// # Errors
///
/// If the record was corrupted, the function returns
/// [Corrupted](../record/enum.Error.html#variant.Corrupted) error.
/// If the record was truncated or the reading failed for a different reason, the function
/// returns [Truncated](../record/enum.Error.html#variant.Truncated) error.
impl<'a, R: Seek + Read> Iterator for RegionViewer<'a, R> {
    type Item = result::Result<record::Record, record::Error>;

    fn next(&mut self) -> Option<Self::Item> {
        let mut record = record::Record::new();
        match self.read_into(&mut record) {
            Ok(()) => Some(Ok(record)),
            Err(record::Error::NoMoreRecords) => None,
            Err(e) => Some(Err(e)),
        }
    }
}

/// Defines how to react to a BAI index being younger than BAM file.
///
/// # Variants
/// * `Error` - [IndexedReader](struct.IndexedReader.html) will not be constructed if the BAI
/// index is was modified earlier than the BAM file. `io::Error` will be raised.
/// * `Ignore` - does nothing if the index is younger than the BAM file.
/// * `Warn` - calls a function `Fn(&str)` and continues constructing
/// [IndexedReader](struct.IndexedReader.html);
pub enum ModificationTime {
    Error,
    Ignore,
    Warn(Box<Fn(&str)>),
}

impl ModificationTime {
    fn check<T: AsRef<Path>, U: AsRef<Path>>(&self, bam_path: T, bai_path: U) -> Result<()> {
        let bam_modified = bam_path.as_ref().metadata().and_then(|metadata| metadata.modified());
        let bai_modified = bai_path.as_ref().metadata().and_then(|metadata| metadata.modified());
        let bam_younger = match (bam_modified, bai_modified) {
            (Ok(bam_time), Ok(bai_time)) => bai_time < bam_time,
            _ => false, // Modification time not available.
        };
        if !bam_younger {
            return Ok(());
        }

        match &self {
            ModificationTime::Ignore => {},
            ModificationTime::Error => return Err(Error::new(InvalidInput,
                "the BAM file is younger than the BAI index")),
            ModificationTime::Warn(box_fun) =>
                box_fun("the BAM file is younger than the BAI index"),
        }
        Ok(())
    }

    /// Create a warning strategy `ModificationTime::Warn`.
    pub fn warn<F: Fn(&str) + 'static>(warning: F) -> Self {
        ModificationTime::Warn(Box::new(warning))
    }
}

/// [IndexedReader](struct.IndexedReader.html) builder. Allows to specify paths to BAM and BAI
/// files, as well as LRU cache size and an option to ignore or warn BAI modification time check.
pub struct IndexedReaderBuilder {
    cache_capacity: Option<usize>,
    bai_path: Option<PathBuf>,
    modification_time: ModificationTime,
}

impl IndexedReaderBuilder {
    /// Creates a new indexed reader builder.
    pub fn new() -> Self {
        Self {
            cache_capacity: None,
            bai_path: None,
            modification_time: ModificationTime::Error,
        }
    }

    /// Sets a path to a BAI index. By default, it is `{bam_path}.bai`.
    /// Overwrites the last value, if any.
    pub fn bai_path<P: AsRef<Path>>(&mut self, path: P) -> &mut Self {
        self.bai_path = Some(path.as_ref().to_path_buf());
        self
    }

    /// By default, [IndexedReader::new](struct.IndexedReader.html#method.new) and
    /// [IndexedReaderBuilder::from_path](struct.IndexedReaderBuilder.html#method.from_path)
    /// return an `io::Error` if the modification time of the BAI index is earlier
    /// than the modification time of the BAM file.
    /// 
    /// Enum [ModificationTime](enum.ModificationTime.html) contains options to skip
    /// this check or raise a warning instead of returning an error.
    pub fn modification_time(&mut self, modification_time: ModificationTime) -> &mut Self {
        self.modification_time = modification_time;
        self
    }

    /// Sets new LRU cache capacity. See
    /// [cache_capacity](../bgzip/struct.SeekReaderBuilder.html#method.cache_capacity)
    /// for more details.
    pub fn cache_capacity(&mut self, cache_capacity: usize) -> &mut Self {
        assert!(cache_capacity > 0, "Cache size must be non-zero");
        self.cache_capacity = Some(cache_capacity);
        self
    }

    /// Creates a new [IndexedReader](struct.IndexedReader.html) from `bam_path`.
    /// If BAI path was not specified, the functions tries to open `{bam_path}.bai`.
    pub fn from_path<P: AsRef<Path>>(&self, bam_path: P) -> Result<IndexedReader<File>> {
        let bam_path = bam_path.as_ref();
        let bai_path = self.bai_path.as_ref().map(PathBuf::clone)
            .unwrap_or_else(|| PathBuf::from(format!("{}.bai", bam_path.display())));
        self.modification_time.check(&bam_path, &bai_path)?;

        let mut reader_builder = bgzip::SeekReader::build();
        if let Some(cache_capacity) = self.cache_capacity {
            reader_builder.cache_capacity(cache_capacity);
        }
        let reader = reader_builder.from_path(bam_path)
            .map_err(|e| Error::new(e.kind(), format!("Failed to open BAM file: {}", e)))?;

        let index = Index::from_path(bai_path)
            .map_err(|e| Error::new(e.kind(), format!("Failed to open BAI index: {}", e)))?;
        IndexedReader::new(reader, index)
    }

    /// Creates a new [IndexedReader](struct.IndexedReader.html) from two streams.
    /// BAM stream should support random access, while BAI stream does not need to.
    /// `check_time` and `bai_path` values are ignored.
    pub fn from_streams<R: Seek + Read, T: Read>(&self, bam_stream: R, bai_stream: T)
            -> Result<IndexedReader<R>> {
        let mut reader_builder = bgzip::SeekReader::build();
        if let Some(cache_capacity) = self.cache_capacity {
            reader_builder.cache_capacity(cache_capacity);
        }
        let reader = reader_builder.from_stream(bam_stream);

        let index = Index::from_stream(bai_stream)
            .map_err(|e| Error::new(e.kind(), format!("Failed to open BAI index: {}", e)))?;
        IndexedReader::new(reader, index)
    }
}

/// BAM file reader. In contrast to [Reader](struct.Reader.html) the `IndexedReader`
/// allows to fetch records from arbitrary positions,
/// but does not allow to read all records consecutively.
///
/// The following code would load BAM file `test.bam` and its index `test.bam.bai`, take all records
/// from `2:100001-200000` and print them on the stdout.
///
/// ```rust
/// extern crate bam;
///
/// fn main() {
///     let mut reader = bam::IndexedReader::from_path("test.bam").unwrap();
///
///     // We need to clone the header to have access to reference names as the
///     // reader will be blocked during fetch.
///     let header = reader.header().clone();
///     let mut stdout = std::io::BufWriter::new(std::io::stdout());
///
///     for record in reader.fetch(1, 100_000, 200_000).unwrap() {
///         record.unwrap().write_sam(&mut stdout, &header).unwrap();
///     }
/// }
/// ```
///
/// Additionally, you can use `read_into(&mut record)` to save time on record allocation:
/// ```rust
/// extern crate bam;
///
/// // You need to import BamReader trait
/// use bam::BamReader;
///
/// fn main() {
///     let mut reader = bam::IndexedReader::from_path("test.bam").unwrap();
///
///     let header = reader.header().clone();
///     let mut stdout = std::io::BufWriter::new(std::io::stdout());
///
///     let mut viewer = reader.fetch(1, 100_000, 200_000).unwrap();
///     let mut record = bam::Record::new();
///     loop {
///         match viewer.read_into(&mut record) {
///             Ok(()) => {},
///             Err(bam::Error::NoMoreRecords) => break,
///             Err(e) => panic!("{}", e),
///         }
///         record.write_sam(&mut stdout, &header).unwrap();
///     }
/// }
/// ```
///
/// If only records with specific MAPQ or FLAGs are needed, you can use `fetch_by`. For example,
/// ```rust
/// reader.fetch_by(1, 100_000, 200_000,
///     |record| record.mapq() >= 30 && !record.is_secondary())
/// ```
/// to load only records with MAPQ at least 30 and skip all secondary alignments. In some cases it
/// helps to save time by not calculating the right-most aligned read position, as well as
/// remove additional allocations.
///
/// You can also use [IndexedReaderBuilder](struct.IndexedReaderBuilder.html),
/// which gives more control over loading
/// [IndexedReader](struct.IndexedReader.html).
/// For example you can create a reader using a different BAI path, and a different cache capacity:
/// ```rust
/// let mut reader = bam::IndexedReader::build()
///     .bai_path("other_dir/test.bai")
///     .cache_capacity(10000)
///     .from_path("test.bam").unwrap();
/// ```
///
/// By default, during construction of the `IndexedReader`, we compare modification times of
/// the BAI index and the BAM file. If the index is older, the function returns an error. This can
/// be changed:
/// ```rust
/// use bam::bam_reader::ModificationTime;
/// let mut reader = bam::IndexedReader::build()
///     .modification_time(ModificationTime::warn(|e| eprintln!("{}", e)))
///     .from_path("test.bam").unwrap();
/// ```
/// You can also ignore the error completely: `.modification_time(ModificationTime::Ignore)`.

pub struct IndexedReader<R: Read + Seek> {
    reader: bgzip::SeekReader<R>,
    header: Header,
    index: Index,
    buffer: Vec<u8>,
}

impl IndexedReader<File> {
    /// Creates [IndexedReaderBuilder](struct.IndexedReaderBuilder.html).
    pub fn build() -> IndexedReaderBuilder {
        IndexedReaderBuilder::new()
    }

    /// Opens bam file from `path`. Bai index will be loaded from `{path}.bai`.
    ///
    /// Same as `Self::build().from_path(path)`.
    pub fn from_path<P: AsRef<Path>>(path: P) -> Result<Self> {
        Self::build().from_path(path)
    }
}

impl<R: Read + Seek> IndexedReader<R> {
    fn new(mut reader: bgzip::SeekReader<R>, index: Index) -> Result<Self> {
        let mut buffer = Vec::with_capacity(bgzip::MAX_BLOCK_SIZE);
        let header = {
            let mut header_reader = bgzip::ChunksReader::without_boundaries(
                &mut reader, &mut buffer);
            Header::from_bam(&mut header_reader)?
        };
        Ok(Self {
            reader, header, index, buffer,
        })
    }

    /// Returns an iterator over records aligned to the reference `ref_id` (0-based),
    /// and intersecting half-open interval `[start-end)`.
    pub fn fetch<'a>(&'a mut self, ref_id: u32, start: u32, end: u32)
            -> Result<RegionViewer<'a, R>> {
        self.fetch_by(ref_id, start, end, |_| true)
    }

    /// Returns an iterator over records aligned to the reference `ref_id` (0-based),
    /// and intersecting half-open interval `[start-end)`.
    ///
    /// Records will be filtered by `predicate`. It helps to slightly reduce fetching time,
    /// as some records will be removed without allocating new memory and without calculating
    /// alignment length.
    pub fn fetch_by<'a, F>(&'a mut self, ref_id: u32, start: u32, end: u32, predicate: F)
        -> Result<RegionViewer<'a, R>>
    where F: 'static + Fn(&record::Record) -> bool
    {
        if start > end {
            return Err(Error::new(InvalidInput,
                format!("Failed to fetch records: start > end ({} > {})", start, end)));
        }
        match self.header.reference_len(ref_id as usize) {
            None => return Err(Error::new(InvalidInput,
                format!("Failed to fetch records: out of bounds reference {}", ref_id))),
            Some(len) if len < end => return Err(Error::new(InvalidInput,
                format!("Failed to fetch records: end > reference length ({} > {})", end, len))),
            _ => {},
        }

        let chunks = self.index.fetch_chunks(ref_id, start as i32, end as i32);
        Ok(RegionViewer {
            chunks_reader: bgzip::ChunksReader::new(&mut self.reader, chunks, &mut self.buffer),
            start: start as i32,
            end: end as i32,
            predicate: Box::new(predicate),
        })
    }

    /// Returns BAM header.
    pub fn header(&self) -> &Header {
        &self.header
    }

    /// Writes record in sam format.
    /// Same as [Record::write_sam](../record/struct.Record.html#method.write_sam).
    pub fn write_record_as_sam<W: Write>(&self, writer: &mut W, record: &record::Record)
            -> Result<()> {
        record.write_sam(writer, self.header())
    }
}

/// BAM file reader. In contrast to [IndexedReader](struct.IndexedReader.html) the `Reader`
/// allows to read all records consecutively, but does not allow random access.
///
/// Implements [BamReader](struct.BamReader.html) trait.
///
/// ```rust
/// extern crate bam;
///
/// fn main() {
///     let reader = bam::Reader::from_path("test.bam").unwrap();
///
///     let header = reader.header().clone();
///     let mut stdout = std::io::BufWriter::new(std::io::stdout());
///
///     for record in reader {
///         record.unwrap().write_sam(&mut stdout, &header).unwrap();
///     }
/// }
/// ```
///
/// You can skip excessive record allocation using `read_into`:
/// ```rust
/// extern crate bam;
///
/// // You need to import BamReader trait
/// use bam::BamReader;
///
/// fn main() {
///     let mut reader = bam::Reader::from_path("test.bam").unwrap();
///
///     let header = reader.header().clone();
///     let mut stdout = std::io::BufWriter::new(std::io::stdout());
///
///     let mut record = bam::Record::new();
///     loop {
///         match reader.read_into(&mut record) {
///             Ok(()) => {},
///             Err(bam::Error::NoMoreRecords) => break,
///             Err(e) => panic!("{}", e),
///         }
///         record.write_sam(&mut stdout, &header).unwrap();
///     }
/// }
/// ```
pub struct Reader<R: Read> {
    bgzip_reader: bgzip::ConsecutiveReader<R>,
    header: Header,
}

impl Reader<File> {
    /// Creates BAM file reader from `path`.
    pub fn from_path<P: AsRef<Path>>(path: P) -> Result<Self> {
        let stream = File::open(path)
            .map_err(|e| Error::new(e.kind(), format!("Failed to open BAM file: {}", e)))?;
        Self::from_stream(stream)
    }
}

impl<R: Read> Reader<R> {
    /// Creates BAM file reader from `stream`. The stream does not have to support random access.
    pub fn from_stream(stream: R) -> Result<Self> {
        let mut bgzip_reader = bgzip::ConsecutiveReader::from_stream(stream)?;
        let header = Header::from_bam(&mut bgzip_reader)?;
        Ok(Self {
            bgzip_reader, header,
        })
    }
}

impl<R: Read> Reader<R> {
    /// Returns BAM header.
    pub fn header(&self) -> &Header {
        &self.header
    }
}

impl<R: Read> BamReader for Reader<R> {
    fn read_into(&mut self, record: &mut record::Record) -> result::Result<(), record::Error> {
        record.fill_from(&mut self.bgzip_reader)
    }
}

/// Iterator over records.
///
/// # Errors
///
/// If the record was corrupted, the function returns
/// [Corrupted](../record/enum.Error.html#variant.Corrupted) error.
/// If the record was truncated or the reading failed for a different reason, the function
/// returns [Truncated](../record/enum.Error.html#variant.Truncated) error.
impl<R: Read> Iterator for Reader<R> {
    type Item = result::Result<record::Record, record::Error>;

    fn next(&mut self) -> Option<Self::Item> {
        let mut record = record::Record::new();
        match self.read_into(&mut record) {
            Ok(()) => Some(Ok(record)),
            Err(record::Error::NoMoreRecords) => None,
            Err(e) => Some(Err(e)),
        }
    }
}
