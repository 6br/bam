//! BAM file pileup.

use std::rc::Rc;
use std::io;
use std::cmp::min;

use super::{Record, RecordReader};

/// Type of the record sequence, matching a single reference position:
/// * `Deletion` - this position is not present in the record.
/// * `Match` - single base-pair match or mismatch,
/// * `Insertion(len)` - single base-pair match followed by the insertion of `len` base-pairs,
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum AlnType {
    Deletion,
    Match,
    Insertion(u32),
}

/// Single record that covers a reference position. Part of [pileup column](struct.PileupColumn.html).
#[derive(Clone)]
pub struct PileupEntry {
    record: Rc<Record>,
    query_start: u32,
    query_end: u32,
    aln_query_end: u32,

    ref_pos: u32,
    cigar_index: usize,
    cigar_remaining: u32,
}

impl PileupEntry {
    /// Creates a new `PileupEntry` from a mapped `Record`.
    fn new(record: Rc<Record>) -> Self {
        let ref_pos = record.start();
        assert!(ref_pos >= 0, "Pileup record cannot be unmapped");

        let mut cigar_index = 0;
        let mut query_pos = 0;
        let cigar_remaining = loop {
            let (len, op) = record.cigar().at(cigar_index);
            if op.consumes_ref() {
                break len;
            }
            if op.consumes_query() {
                query_pos += len;
            }
            cigar_index += 1;
        };
        assert!(cigar_index < record.cigar().len(), "CIGAR cannot contain only insertions");
        let aln_query_end = record.aligned_query_end();

        let mut res = PileupEntry {
            record,
            query_start: query_pos,
            query_end: query_pos,
            aln_query_end,

            ref_pos: ref_pos as u32,
            cigar_index,
            cigar_remaining,
        };
        res.update_query_end();
        res
    }

    fn update_query_end(&mut self) {
        let op = self.record.cigar().at(self.cigar_index).1;
        self.query_end = if !op.consumes_query() {
            self.query_start
        } else if self.cigar_remaining == 1 {
            let mut query_end = self.query_start + 1;
            let mut i = self.cigar_index + 1;
            while i < self.record.cigar().len() && query_end < self.aln_query_end {
                let (len, op) = self.record.cigar().at(i);
                if op.consumes_ref() {
                    break;
                } else if op.consumes_query() {
                    query_end += len;
                }
                i += 1;
            }
            min(query_end, self.aln_query_end)
        } else {
            self.query_start + 1
        };
    }

    /// Moves the record to the next reference position. Returns false if reached the end of the alignment.
    fn move_forward(&mut self) -> bool {
        let op = self.record.cigar().at(self.cigar_index).1;
        self.cigar_remaining -= 1;
        if op.consumes_ref() {
            self.ref_pos += 1;
        }
        if op.consumes_query() {
            self.query_start += 1;
        }

        while self.cigar_remaining == 0 {
            self.cigar_index += 1;
            if self.cigar_index == self.record.cigar().len() || self.query_start >= self.aln_query_end {
                return false;
            }
            let (len, op) = self.record.cigar().at(self.cigar_index);
            if op.consumes_ref() {
                self.cigar_remaining = len;
            } else if op.consumes_query() {
                self.query_start += len;
            }
        }
        self.update_query_end();
        assert!(self.query_end <= self.aln_query_end);
        true
    }

    /// Returns the smart pointer to the [record](../record/struct.Record.html).
    pub fn record(&self) -> &Rc<Record> {
        &self.record
    }

    /// Returns 0-based index in the record sequence of the first base aligned to the reference position.
    /// If the position is deleted in the record, the function returns the index of the last aligned base before
    /// the reference position.
    pub fn query_start(&self) -> u32 {
        self.query_start
    }

    /// Returns 0-based index in the record sequence of after the last base aligned to the reference position.
    /// If the position is deleted in the record, the function returns the index of the last aligned base before
    /// the reference position (`query_start == query_end`).
    pub fn query_end(&self) -> u32 {
        self.query_end
    }

    /// Returns the size of the record sequence aligned to the reference position
    /// (same as `query_end() - query_start()`).
    pub fn len(&self) -> u32 {
        self.query_end - self.query_start
    }

    /// Returns the type of the region aligned to the reference position (deletion, match or insertion).
    pub fn aln_type(&self) -> AlnType {
        match self.len() {
            0 => AlnType::Deletion,
            1 => AlnType::Match,
            x => AlnType::Insertion(x - 1),
        }
    }

    /// Returns an iterator over nucleotides in the region aligned to the reference position,
    /// if the sequence is present in the record.
    pub fn sequence(&self) -> Option<super::record::sequence::SubseqIter> {
        if self.record.sequence().available() {
            Some(self.record.sequence().subseq(self.query_start as usize..self.query_end as usize))
        } else {
            None
        }
    }

    /// Returns raw qualities (without +33) in the region aligned to the reference position,
    /// if the qualities are present in the record.
    pub fn qualities(&self) -> Option<&[u8]> {
        if self.record.qualities().available() {
            Some(&self.record.qualities().raw()[self.query_start as usize..self.query_end as usize])
        } else {
            None
        }
    }
}

pub struct Pileup<'a, I: Iterator<Item = io::Result<Record>>> {
    record_iter: &'a mut I,
    read_filter: Box<dyn Fn(&Record) -> bool>,
    entries: Vec<PileupEntry>,
    error: Option<io::Error>,

    last_ref_id: u32,
    last_ref_pos: u32,
}

impl<'a, I: Iterator<Item = io::Result<Record>>> Pileup<'a, I> {
    pub fn new(record_iter: &'a mut I) -> Self {
        Self::with_filter(record_iter, |_| true)
    }

    pub fn with_filter<F: 'static + Fn(&Record) -> bool>(record_iter: &'a mut I, read_filter: F) -> Self {
        let mut res = Pileup {
            record_iter,
            read_filter: Box::new(read_filter),
            entries: Vec::new(),
            error: None,
            last_ref_id: 0,
            last_ref_pos: 0,
        };
        res.read_next();
        res
    }

    fn record_passes(&self, record: &Record) -> bool {
        if !record.flag().is_mapped() {
            return false;
        }
        assert!(record.ref_id() >= 0 && record.start() >= 0);
        (self.read_filter)(record)
    }

    fn read_next(&mut self) {
        if self.last_ref_id == std::u32::MAX || self.error.is_some() {
            return;
        }
        loop {
            match self.record_iter.next() {
                None => self.last_ref_id = std::u32::MAX,
                Some(Ok(record)) => {
                    if !self.record_passes(&record) {
                        continue;
                    }
                    let rec_ref_id = record.ref_id() as u32;
                    let rec_start = record.start() as u32;
                    if rec_ref_id < self.last_ref_id
                            || (rec_ref_id == self.last_ref_id && rec_start < self.last_ref_pos) {
                        self.error = Some(io::Error::new(io::ErrorKind::InvalidData, "Input file is unsorted"));
                        self.last_ref_id = std::u32::MAX;
                    }
                    self.last_ref_id = rec_ref_id;
                    self.last_ref_pos = rec_start;
                    self.entries.push(PileupEntry::new(Rc::new(record)));
                },
                Some(Err(e)) => {
                    self.error = Some(e);
                    self.last_ref_id = std::u32::MAX;
                },
            }
            return;
        }
    }
}

impl<'a, R: RecordReader> Iterator for Pileup<'a, R> {
    type Item = io::Result<PileupColumn>;

    fn next(&mut self) -> Option<Self::Item> {
        if self.error.is_some() {
            self.entries.clear();
            self.last_ref_id = std::u32::MAX;
            return Some(Err(self.error.take().unwrap()));
        }

        let mut new_ref_id = std::u32::MAX;
        let mut new_ref_pos = std::u32::MAX;
        while new_ref_id == std::u32::MAX && (!self.entries.is_empty() || self.last_ref_id < std::u32::MAX) {
            for entry in self.entries.iter() {
                let rec_ref_id = entry.record.ref_id() as u32;
                if rec_ref_id < new_ref_id {
                    new_ref_id = rec_ref_id;
                    new_ref_pos = entry.ref_pos;
                } else if rec_ref_id == new_ref_id {
                    new_ref_pos = min(new_ref_pos, entry.ref_pos);
                }
            }

            while self.last_ref_id < std::u32::MAX
                    // new_ref_id == last_ref_id or new_ref_id == u32::MAX, same with pos.
                    && self.last_ref_id <= new_ref_id && self.last_ref_pos <= new_ref_pos {
                self.read_next();
            }
            if self.error.is_some() {
                self.entries.clear();
                self.last_ref_id = std::u32::MAX;
                return Some(Err(self.error.take().unwrap()));
            }
        }

        let mut entries = Vec::new();
        for i in (0..self.entries.len()).rev() {
            let entry = &mut self.entries[i];
            let rec_ref_id = entry.record.ref_id() as u32;
            if rec_ref_id == new_ref_id && entry.ref_pos == new_ref_pos {
                entries.push(entry.clone());
                if !entry.move_forward() {
                    std::mem::drop(entry);
                    self.entries.swap_remove(i);
                }
            } else {
                assert!(rec_ref_id > new_ref_id || entry.ref_pos > new_ref_pos,
                    "Record is to the left of the new pileup position");
            }
        }

        if entries.is_empty() {
            None
        } else {
            Some(Ok(PileupColumn {
                entries,
                ref_id: new_ref_id,
                ref_pos: new_ref_pos,
            }))
        }
    }
}

#[derive(Clone)]
pub struct PileupColumn {
    entries: Vec<PileupEntry>,
    ref_id: u32,
    ref_pos: u32,
}

impl PileupColumn {
    /// Returns [pileup entries](struct.PileupEntry.html), corresponding to this reference position.
    pub fn entries(&self) -> &[PileupEntry] {
        &self.entries
    }

    /// Sort [pileup entries](struct.PileupEntry.html) by the start of the alignment, and then by the record names.
    pub fn sort(&mut self) {
        self.entries.sort_by(|a, b| (a.record.start(), a.record.name()).cmp(&(b.record.start(), b.record.name())))
    }

    /// Returns 0-based reference id.
    pub fn ref_id(&self) -> u32 {
        self.ref_id
    }

    /// Returns 0-based reference position.
    pub fn ref_pos(&self) -> u32 {
        self.ref_pos
    }
}
