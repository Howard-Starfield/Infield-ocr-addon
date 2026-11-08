//! Framed stdio protocol between the Infield app and the OCR sidecar.
//!
//! Frame: magic(4) version(1) kind(1) seq(u32 LE) header_len(u32 LE)
//! payload_len(u64 LE), then the UTF-8 JSON header and the raw payload.

use serde::{Deserialize, Serialize};
use std::io::{Read, Write};
use std::path::PathBuf;

pub const MAGIC: [u8; 4] = *b"IOCR";
/// Version 2: `doc_page_count` is required in the page header.
pub const PROTOCOL_VERSION: u8 = 2;
pub const MAX_HEADER_BYTES: u32 = 64 * 1024;
pub const MAX_PAYLOAD_BYTES: u64 = 64 * 1024 * 1024;
const PREFIX_LEN: usize = 22;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[repr(u8)]
pub enum FrameKind {
    Hello = 1,
    Analyze = 2,
    Cancel = 3,
    Shutdown = 4,
    HelloOk = 101,
    PageResult = 102,
    Done = 104,
    Error = 105,
}

impl FrameKind {
    fn from_u8(v: u8) -> Option<Self> {
        Some(match v {
            1 => Self::Hello,
            2 => Self::Analyze,
            3 => Self::Cancel,
            4 => Self::Shutdown,
            101 => Self::HelloOk,
            102 => Self::PageResult,
            104 => Self::Done,
            105 => Self::Error,
            _ => return None,
        })
    }
}

#[derive(Debug)]
pub struct Frame {
    pub kind: FrameKind,
    pub seq: u32,
    pub header: Vec<u8>,
    pub payload: Vec<u8>,
}

#[derive(Debug, thiserror::Error)]
pub enum FrameError {
    #[error("bad magic")]
    BadMagic,
    #[error("unsupported protocol version {0}")]
    Version(u8),
    #[error("unknown frame kind {0}")]
    Kind(u8),
    #[error("header too large: {0}")]
    HeaderTooLarge(u32),
    #[error("payload too large: {0}")]
    PayloadTooLarge(u64),
    #[error("sequence {got}, expected {expected}")]
    Sequence { got: u32, expected: u32 },
    #[error("clean end of stream")]
    Eof,
    #[error("truncated frame")]
    Truncated,
    #[error("malformed page result: {0}")]
    Malformed(&'static str),
    #[error("io: {0}")]
    Io(#[from] std::io::Error),
}

pub fn write_frame<W: Write>(
    w: &mut W,
    kind: FrameKind,
    seq: u32,
    header: &[u8],
    payload: &[u8],
) -> Result<(), FrameError> {
    if header.len() > MAX_HEADER_BYTES as usize {
        return Err(FrameError::HeaderTooLarge(
            u32::try_from(header.len()).unwrap_or(u32::MAX),
        ));
    }
    if payload.len() as u64 > MAX_PAYLOAD_BYTES {
        return Err(FrameError::PayloadTooLarge(payload.len() as u64));
    }
    let mut prefix = [0u8; PREFIX_LEN];
    prefix[0..4].copy_from_slice(&MAGIC);
    prefix[4] = PROTOCOL_VERSION;
    prefix[5] = kind as u8;
    prefix[6..10].copy_from_slice(&seq.to_le_bytes());
    prefix[10..14].copy_from_slice(&(header.len() as u32).to_le_bytes());
    prefix[14..22].copy_from_slice(&(payload.len() as u64).to_le_bytes());
    w.write_all(&prefix)?;
    w.write_all(header)?;
    w.write_all(payload)?;
    w.flush()?;
    Ok(())
}

/// Fill `buf` fully. `Ok(false)` means EOF before the first byte.
fn read_full<R: Read>(r: &mut R, buf: &mut [u8]) -> Result<bool, FrameError> {
    let mut filled = 0;
    while filled < buf.len() {
        match r.read(&mut buf[filled..]) {
            Ok(0) => {
                return if filled == 0 {
                    Ok(false)
                } else {
                    Err(FrameError::Truncated)
                };
            }
            Ok(n) => filled += n,
            Err(e) if e.kind() == std::io::ErrorKind::Interrupted => {}
            Err(e) if e.kind() == std::io::ErrorKind::UnexpectedEof => return Err(FrameError::Truncated),
            Err(e) => return Err(e.into()),
        }
    }
    Ok(true)
}

/// Read exactly `len` bytes without trusting `len` for the up-front allocation.
fn read_body<R: Read>(r: &mut R, len: usize) -> Result<Vec<u8>, FrameError> {
    let mut v = Vec::new();
    match r.take(len as u64).read_to_end(&mut v) {
        Ok(n) if n == len => Ok(v),
        Ok(_) => Err(FrameError::Truncated),
        Err(e) if e.kind() == std::io::ErrorKind::UnexpectedEof => Err(FrameError::Truncated),
        Err(e) => Err(e.into()),
    }
}

pub fn read_frame<R: Read>(r: &mut R, expected_seq: u32) -> Result<Frame, FrameError> {
    let mut p = [0u8; PREFIX_LEN];
    if !read_full(r, &mut p)? {
        return Err(FrameError::Eof);
    }
    if p[0..4] != MAGIC {
        return Err(FrameError::BadMagic);
    }
    if p[4] != PROTOCOL_VERSION {
        return Err(FrameError::Version(p[4]));
    }
    let kind = FrameKind::from_u8(p[5]).ok_or(FrameError::Kind(p[5]))?;
    let seq = u32::from_le_bytes(p[6..10].try_into().unwrap());
    let header_len = u32::from_le_bytes(p[10..14].try_into().unwrap());
    let payload_len = u64::from_le_bytes(p[14..22].try_into().unwrap());
    if header_len > MAX_HEADER_BYTES {
        return Err(FrameError::HeaderTooLarge(header_len));
    }
    if payload_len > MAX_PAYLOAD_BYTES {
        return Err(FrameError::PayloadTooLarge(payload_len));
    }
    if seq != expected_seq {
        return Err(FrameError::Sequence {
            got: seq,
            expected: expected_seq,
        });
    }
    let header = read_body(r, header_len as usize)?;
    let payload = read_body(r, payload_len as usize)?;
    Ok(Frame {
        kind,
        seq,
        header,
        payload,
    })
}

/// Write a `PageResult` frame: a small [`PageHeader`] plus the blocks as a JSON
/// array payload. `PayloadTooLarge` means the blocks alone exceed the 64 MiB cap.
pub fn write_page_result<W: Write>(w: &mut W, seq: u32, page: &PageResult) -> Result<(), FrameError> {
    let header = PageHeader {
        page: page.page,
        width: page.width,
        height: page.height,
        source: page.source,
        block_count: u32::try_from(page.blocks.len()).unwrap_or(u32::MAX),
        render_ms: page.render_ms,
        analyze_ms: page.analyze_ms,
        peak_working_set_mb: page.peak_working_set_mb,
        doc_page_count: page.doc_page_count,
    };
    let header = serde_json::to_vec(&header).map_err(|_| FrameError::Malformed("page header"))?;
    let payload = serde_json::to_vec(&page.blocks).map_err(|_| FrameError::Malformed("page blocks"))?;
    write_frame(w, FrameKind::PageResult, seq, &header, &payload)
}

/// Decode a `PageResult` frame written by [`write_page_result`]. The block count in the
/// header must match the payload.
pub fn decode_page_result(frame: &Frame) -> Result<PageResult, FrameError> {
    if frame.kind != FrameKind::PageResult {
        return Err(FrameError::Malformed("not a page result frame"));
    }
    let h: PageHeader = serde_json::from_slice(&frame.header).map_err(|_| FrameError::Malformed("page header"))?;
    let blocks: Vec<Block> =
        serde_json::from_slice(&frame.payload).map_err(|_| FrameError::Malformed("page blocks"))?;
    if blocks.len() != h.block_count as usize {
        return Err(FrameError::Malformed("block count does not match the payload"));
    }
    Ok(PageResult {
        page: h.page,
        width: h.width,
        height: h.height,
        source: h.source,
        blocks,
        render_ms: h.render_ms,
        analyze_ms: h.analyze_ms,
        peak_working_set_mb: h.peak_working_set_mb,
        doc_page_count: h.doc_page_count,
    })
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case", deny_unknown_fields)]
pub struct Hello {
    pub protocol: u8,
    pub app_version: String,
}

/// A capability a sidecar may advertise in [`HelloOk::capabilities`]: it writes one raw RGB crop
/// per text-like block when [`Analyze::emit_crops`] is set (accurate reading).
pub const CAP_EMIT_CROPS: &str = "emit_crops";

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case", deny_unknown_fields)]
pub struct HelloOk {
    pub protocol: u8,
    pub sidecar_version: String,
    pub ort_version: String,
    /// Optional features of this sidecar. Absent (an older sidecar) reads as an empty list, and
    /// an empty list is not written, so the reply stays byte-identical to the one before.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub capabilities: Vec<String>,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case", deny_unknown_fields)]
pub struct Analyze {
    pub job_id: String,
    pub input_path: PathBuf,
    pub first_page: u32,
    pub last_page: Option<u32>,
    pub mode: AnalyzeMode,
    pub models: FastModels,
    pub threads: u16,
    pub max_side: u32,
    pub det_limit: u32,
    pub render_dpi: u32,
    pub force_ocr: bool,
    pub crop_dir: PathBuf,
    pub max_pages: u32,
    /// Also write one raw RGB crop per text-like block ([`write_rgb_crop`] layout). The app sends
    /// this only when the hello reply advertised [`CAP_EMIT_CROPS`]; it is not serialized when
    /// false, so an older sidecar (which denies unknown fields) never sees it.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub emit_crops: bool,
}

/// Accurate reading adds `AccurateCrops`.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum AnalyzeMode {
    Fast,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case", deny_unknown_fields)]
pub struct FastModels {
    pub layout: PathBuf,
    pub det: PathBuf,
    pub rec: PathBuf,
    pub rec_dict: PathBuf,
    pub table: Option<PathBuf>,
    pub table_dict: Option<PathBuf>,
    pub orientation: Option<PathBuf>,
}

/// Header of a `PageResult` frame. The blocks do not travel in the header (64 KiB
/// cap, which a dense page exceeds); they are a JSON array in the frame payload
/// (64 MiB cap). Use [`write_page_result`] and [`decode_page_result`].
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case", deny_unknown_fields)]
pub struct PageHeader {
    pub page: u32,
    pub width: u32,
    pub height: u32,
    pub source: PageSource,
    pub block_count: u32,
    pub render_ms: u32,
    pub analyze_ms: u32,
    pub peak_working_set_mb: u32,
    /// Pages in the opened document (1 for an image). The same on every page
    /// result of a document, so the supervisor learns the real end of the
    /// document without guessing from errors.
    pub doc_page_count: u32,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case", deny_unknown_fields)]
pub struct PageResult {
    pub page: u32,
    pub width: u32,
    pub height: u32,
    pub source: PageSource,
    pub blocks: Vec<Block>,
    pub render_ms: u32,
    pub analyze_ms: u32,
    pub peak_working_set_mb: u32,
    /// Pages in the opened document; see [`PageHeader::doc_page_count`].
    pub doc_page_count: u32,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case", deny_unknown_fields)]
pub enum PageSource {
    TextLayer,
    Ocr,
    Failed { code: ErrorCode },
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case", deny_unknown_fields)]
pub struct Block {
    pub id: u32,
    pub kind: BlockKind,
    pub bbox: [f32; 4],
    pub order: Option<u32>,
    pub text: Option<String>,
    pub cells: Option<TableCells>,
    pub confidence: f32,
    pub crop_file: Option<String>,
    /// The model that read this block, set by the app after recognition. The sidecar never sets it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub model: Option<String>,
    /// The accurate reader failed on this block and the fast text was kept. Set by the app only.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub vlm_fallback: bool,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum BlockKind {
    DocTitle,
    ParagraphTitle,
    Text,
    List,
    Table,
    Figure,
    Chart,
    Formula,
    Caption,
    Header,
    Footer,
    PageNumber,
    Seal,
    Other,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case", deny_unknown_fields)]
pub struct TableCells {
    pub rows: u16,
    pub cols: u16,
    pub cells: Vec<Cell>,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case", deny_unknown_fields)]
pub struct Cell {
    pub row: u16,
    pub col: u16,
    pub row_span: u16,
    pub col_span: u16,
    pub text: String,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case", deny_unknown_fields)]
pub struct Done {
    pub pages: u32,
    pub failed_pages: Vec<u32>,
}

/// `message` must never contain document text.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case", deny_unknown_fields)]
pub struct ErrorFrame {
    pub code: ErrorCode,
    pub message: String,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ErrorCode {
    BadInput,
    UnsupportedInput,
    ModelLoad,
    Render,
    Analyze,
    Cancelled,
    Internal,
}

// ---- raw RGB crops ----------------------------------------------------------------------------

/// A raw RGB crop file: 8 bytes (width, height as `u32` little endian) then `width * height * 3`
/// bytes of RGB8, at most [`MAX_RGB_CROP_BYTES`] in all. The app encodes the PNG a vision model
/// gets, so the model server only ever decodes images the app produced.
pub const RGB_CROP_HEADER_LEN: usize = 8;
pub const MAX_RGB_CROP_BYTES: usize = 64 * 1024 * 1024;

#[derive(Clone, Debug, PartialEq, Eq, thiserror::Error)]
pub enum RgbCropError {
    #[error("the crop is shorter than its header")]
    Short,
    #[error("the crop is larger than {MAX_RGB_CROP_BYTES} bytes")]
    Oversize,
    #[error("the crop has a zero side, or its size does not match its header")]
    SizeMismatch,
    #[error("the crop is not a regular file")]
    NotAFile,
    #[error("the crop could not be read: {0}")]
    Io(std::io::ErrorKind),
}

/// The bytes of a raw RGB crop file for `rgb` (`width * height * 3` bytes of RGB8).
pub fn encode_rgb_crop(width: u32, height: u32, rgb: &[u8]) -> Result<Vec<u8>, RgbCropError> {
    let expected = (width as usize).checked_mul(height as usize).and_then(|p| p.checked_mul(3));
    if width == 0 || height == 0 || expected != Some(rgb.len()) {
        return Err(RgbCropError::SizeMismatch);
    }
    if RGB_CROP_HEADER_LEN + rgb.len() > MAX_RGB_CROP_BYTES {
        return Err(RgbCropError::Oversize);
    }
    let mut out = Vec::with_capacity(RGB_CROP_HEADER_LEN + rgb.len());
    out.extend_from_slice(&width.to_le_bytes());
    out.extend_from_slice(&height.to_le_bytes());
    out.extend_from_slice(rgb);
    Ok(out)
}

/// `(width, height, rgb)` of a raw RGB crop file's bytes, checked against its header.
pub fn decode_rgb_crop(bytes: &[u8]) -> Result<(u32, u32, &[u8]), RgbCropError> {
    if bytes.len() > MAX_RGB_CROP_BYTES {
        return Err(RgbCropError::Oversize);
    }
    if bytes.len() < RGB_CROP_HEADER_LEN {
        return Err(RgbCropError::Short);
    }
    let width = u32::from_le_bytes(bytes[0..4].try_into().expect("4 bytes"));
    let height = u32::from_le_bytes(bytes[4..8].try_into().expect("4 bytes"));
    let rgb = &bytes[RGB_CROP_HEADER_LEN..];
    let expected = (width as usize).checked_mul(height as usize).and_then(|p| p.checked_mul(3));
    if width == 0 || height == 0 || expected != Some(rgb.len()) {
        return Err(RgbCropError::SizeMismatch);
    }
    Ok((width, height, rgb))
}

// ---- crop names and the crop list (shared by the sidecar and the app) -------------------------

/// Block kinds that get a raw RGB crop when `emit_crops` is set: the ones a vision model reads.
/// Charts, figures and seals stay figures (PNG crops); headers, footers and page numbers stay with
/// the fast text.
pub fn wants_rgb_crop(kind: BlockKind) -> bool {
    matches!(
        kind,
        BlockKind::DocTitle
            | BlockKind::ParagraphTitle
            | BlockKind::Text
            | BlockKind::List
            | BlockKind::Table
            | BlockKind::Formula
            | BlockKind::Caption
            | BlockKind::Other
    )
}

/// A page with fewer crop-list blocks than this (counted before anything is filtered) gets one
/// full-page crop, and the app reads that page whole in one request (page mode).
pub const MIN_VLM_REGIONS: usize = 2;

/// The raw crop of block `block_id` on page `page`: `p0003-b007.rgb`.
pub fn region_crop_name(page: u32, block_id: u32) -> String {
    format!("p{page:04}-b{block_id:03}.rgb")
}

/// The full-page crop of page `page`: `p0003-page.rgb`. No region crop can have this name.
pub fn page_crop_name(page: u32) -> String {
    format!("p{page:04}-page.rgb")
}

/// What a canonical raw crop name names.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum RgbCropName {
    Region { page: u32, block_id: u32 },
    Page { page: u32 },
}

/// The crop a name names, only for the exact names [`region_crop_name`] and [`page_crop_name`]
/// write (ASCII digits, no other spelling, no path).
pub fn parse_rgb_crop_name(name: &str) -> Option<RgbCropName> {
    // The longest canonical name is `p4294967295-b4294967295.rgb` (27 bytes).
    if name.len() > 40 {
        return None;
    }
    let digits = |s: &str| -> Option<u32> {
        if s.is_empty() || !s.bytes().all(|b| b.is_ascii_digit()) {
            return None;
        }
        s.parse().ok()
    };
    let (page, rest) = name.strip_prefix('p')?.strip_suffix(".rgb")?.split_once('-')?;
    let page = digits(page)?;
    let parsed = match rest {
        "page" => RgbCropName::Page { page },
        _ => RgbCropName::Region { page, block_id: digits(rest.strip_prefix('b')?)? },
    };
    // Only the exact spelling the writers use: no extra zeros, no other width.
    let canonical = match parsed {
        RgbCropName::Region { page, block_id } => region_crop_name(page, block_id),
        RgbCropName::Page { page } => page_crop_name(page),
    };
    (canonical == name).then_some(parsed)
}

/// True only for a canonical raw crop name (see [`parse_rgb_crop_name`]): never a path.
pub fn safe_rgb_crop_name(name: &str) -> bool {
    parse_rgb_crop_name(name).is_some()
}

/// A raw RGB crop read from disk.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct RgbCrop {
    pub width: u32,
    pub height: u32,
    /// `width * height * 3` bytes of RGB8.
    pub rgb: Vec<u8>,
}

/// Reads a raw RGB crop file, bounded: anything but a regular file is refused, and a file over
/// [`MAX_RGB_CROP_BYTES`] is refused from its length before a byte is read.
pub fn read_rgb_crop(path: &std::path::Path) -> Result<RgbCrop, RgbCropError> {
    let io = |e: std::io::Error| RgbCropError::Io(e.kind());
    let meta = std::fs::symlink_metadata(path).map_err(io)?;
    if !meta.file_type().is_file() {
        return Err(RgbCropError::NotAFile);
    }
    if meta.len() > MAX_RGB_CROP_BYTES as u64 {
        return Err(RgbCropError::Oversize);
    }
    if meta.len() < RGB_CROP_HEADER_LEN as u64 {
        return Err(RgbCropError::Short);
    }
    let mut bytes = Vec::with_capacity(meta.len() as usize);
    // Bounded again while reading: the file may have grown since the length was checked.
    std::fs::File::open(path)
        .map_err(io)?
        .take(MAX_RGB_CROP_BYTES as u64 + 1)
        .read_to_end(&mut bytes)
        .map_err(io)?;
    let (width, height, _) = decode_rgb_crop(&bytes)?;
    // In place: the pixels are not copied into a second buffer.
    bytes.drain(..RGB_CROP_HEADER_LEN);
    Ok(RgbCrop { width, height, rgb: bytes })
}
