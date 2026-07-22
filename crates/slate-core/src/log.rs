//! log

use crate::chain::Chain;
use crate::config::*;
use crate::error::Error;
use crate::record::RecordHeader;
use slate_hal::Flash;

/// Commit marker fields.
pub struct CmFields {
    /// Magic byte.
    pub magic: u8,
    /// Max sequence number.
    pub seq_max: u64,
    /// Epoch number.
    pub epoch: u64,
    /// Number of covered data pages by the preceding XOR page.
    pub xor_pages: u16,
    /// Chain hash.
    pub chi: [u8; 32],
    /// MAC over marker fields.
    pub tau_cm: [u8; 32],
}

/// Crypto seam
pub trait Sealer {
    /// Seals a record into out buffer.
    fn seal_record(&mut self, hdr: &[u8; REC_HDR_LEN], plain_kv: &[u8], ct_tag_out: &mut [u8]);
    /// Opens a record and returns plain KV.
    fn open_record(
        &mut self,
        hdr: &[u8; REC_HDR_LEN],
        ct_tag: &[u8],
        plain_out: &mut [u8],
    ) -> Result<(), Error>;

    /// Generates a commit marker.
    fn commit_marker(
        &mut self,
        seq_max: u64,
        epoch: u64,
        xor_pages: u16,
        chi: &[u8; 32],
    ) -> [u8; CM_LEN];
    /// Verifies a commit marker.
    fn verify_marker(&self, cm: &[u8; CM_LEN]) -> Result<CmFields, Error>;
    /// Seals a checkpoint in-place.
    fn seal_checkpoint(&mut self, epoch: u64, slot: u8, ad: &[u8], in_out: &mut [u8]) -> [u8; 16];
    /// Opens a checkpoint in-place.
    fn open_checkpoint(
        &mut self,
        epoch: u64,
        slot: u8,
        ad: &[u8],
        in_out: &mut [u8],
        tag: &[u8; 16],
    ) -> Result<(), Error>;
    /// Rotates the epoch key.
    fn roll_epoch(&mut self, e: u64);
}

/// Head state for the append log.
pub struct HeadState {
    /// Segment allocation number.
    pub seg_seq: u64,
    /// Write offset.
    pub write_offset: u32,
    /// Open segment id.
    pub block_idx: u32,
}

/// Buffer for batches.
pub struct BatchBuf<'a> {
    buf: &'a mut [u8],
    offset: usize,
}

impl<'a> BatchBuf<'a> {
    /// Creates a new BatchBuf.
    pub fn new(buf: &'a mut [u8]) -> Self {
        Self { buf, offset: 0 }
    }

    /// Allocates space in batch.
    pub fn alloc(&mut self, size: usize) -> Result<&mut [u8], Error> {
        if self.offset + size > self.buf.len() {
            return Err(Error::BatchFull);
        }
        let start = self.offset;
        self.offset += size;
        Ok(&mut self.buf[start..self.offset])
    }

    /// Checks if batch is empty.
    pub fn is_empty(&self) -> bool {
        self.offset == 0
    }

    /// Clears batch.
    pub fn clear(&mut self) {
        self.offset = 0;
    }

    /// Returns written data.
    pub fn data(&self) -> &[u8] {
        &self.buf[0..self.offset]
    }
}

/// The log instance.
pub struct Log<'a, F: Flash> {
    /// Head state.
    pub head: HeadState,
    /// Batch buffer.
    pub batch: BatchBuf<'a>,
    _flash: core::marker::PhantomData<F>,
}

impl<'a, F: Flash> Log<'a, F> {
    /// Creates a new Log.
    pub fn new(buf: &'a mut [u8], head: HeadState) -> Self {
        Self {
            head,
            batch: BatchBuf::new(buf),
            _flash: core::marker::PhantomData,
        }
    }

    /// Appends operation to batch.
    pub fn append(
        &mut self,
        seq: u64,
        op: u8,
        key: &[u8],
        val: &[u8],
        s: &mut impl Sealer,
        chain: &mut Chain,
    ) -> Result<(u64, u32), Error> {
        if key.len() > MAX_KEY_LEN || val.len() > MAX_VAL_LEN {
            return Err(Error::FormatError);
        }

        let mut hdr = RecordHeader {
            magic: MAGIC_REC,
            seq,
            op,
            fp: crate::index::fingerprint(key) as u16,
            klen: key.len() as u16,
            vlen: val.len() as u16,
            nonce: [0; 12],
        };
        hdr.nonce[0..8].copy_from_slice(&seq.to_le_bytes());

        let total_len = crate::config::REC_OVERHEAD + key.len() + val.len();
        let offset = self.head.write_offset + self.batch.offset as u32;
        let rec = self.batch.alloc(total_len)?;

        let mut hdr_bytes = [0u8; REC_HDR_LEN];
        hdr.encode(&mut hdr_bytes);
        rec[..REC_HDR_LEN].copy_from_slice(&hdr_bytes);

        let mut kv_idx = 0;
        let mut kv = [0u8; MAX_KEY_LEN + MAX_VAL_LEN];
        kv[..key.len()].copy_from_slice(key);
        kv_idx += key.len();
        kv[kv_idx..kv_idx + val.len()].copy_from_slice(val);
        kv_idx += val.len();

        s.seal_record(&hdr_bytes, &kv[..kv_idx], &mut rec[REC_HDR_LEN..]);
        chain.fold(&rec[..total_len]);

        Ok((seq, offset))
    }

    fn program_batch_pages(&mut self, flash: &mut F) -> Result<(), Error> {
        let data = self.batch.data();
        let page_size = flash.page_size();

        let mut addr = self.head.write_offset;
        let num_pages = data.len().div_ceil(page_size);
        let mut page_buf = [0xFF; MAX_PAGE_SIZE];

        for i in 0..num_pages {
            let chunk_start = i * page_size;
            let chunk_end = core::cmp::min(chunk_start + page_size, data.len());
            let chunk_len = chunk_end - chunk_start;

            page_buf[..page_size].fill(0xFF);
            page_buf[..chunk_len].copy_from_slice(&data[chunk_start..chunk_end]);

            flash
                .program(addr, &page_buf[..page_size])
                .map_err(|_| Error::Io)?;
            addr += page_size as u32;
        }

        self.head.write_offset = addr;
        Ok(())
    }

    fn program_xor_parity(&mut self, flash: &mut F) -> Result<u16, Error> {
        let data = self.batch.data();
        if data.is_empty() {
            return Ok(0);
        }
        let page_size = flash.page_size();
        let num_pages = data.len().div_ceil(page_size);

        let mut xor_page = [0u8; MAX_PAGE_SIZE];
        let mut page_buf = [0xFF; MAX_PAGE_SIZE];

        for i in 0..num_pages {
            let chunk_start = i * page_size;
            let chunk_end = core::cmp::min(chunk_start + page_size, data.len());
            let chunk_len = chunk_end - chunk_start;

            page_buf[..page_size].fill(0xFF);
            page_buf[..chunk_len].copy_from_slice(&data[chunk_start..chunk_end]);

            for j in 0..page_size {
                xor_page[j] ^= page_buf[j];
            }
        }

        flash
            .program(self.head.write_offset, &xor_page[..page_size])
            .map_err(|_| Error::Io)?;
        self.head.write_offset += page_size as u32;
        Ok(num_pages as u16)
    }

    fn program_page(&mut self, flash: &mut F, data: &[u8]) -> Result<(), Error> {
        let page_size = flash.page_size();
        let mut page_buf = [0xFF; MAX_PAGE_SIZE];
        let len = core::cmp::min(data.len(), page_size);
        page_buf[..len].copy_from_slice(&data[..len]);
        flash
            .program(self.head.write_offset, &page_buf[..page_size])
            .map_err(|_| Error::Io)?;
        self.head.write_offset += page_size as u32;
        Ok(())
    }

    /// Commits batched records to flash.
    pub fn commit(
        &mut self,
        flash: &mut F,
        s: &mut impl Sealer,
        chain: &Chain,
        epoch: u64,
        seq_max: u64,
    ) -> Result<(), Error> {
        if self.batch.is_empty() {
            return Ok(());
        }
        self.program_batch_pages(flash)?;
        let xor_pages = self.program_xor_parity(flash)?;
        let cm = s.commit_marker(seq_max, epoch, xor_pages, &chain.chi);
        self.program_page(flash, &cm)?;
        self.program_page(flash, &cm)?;
        self.batch.clear();
        Ok(())
    }
}
