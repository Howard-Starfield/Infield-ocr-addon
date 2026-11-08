use infield_ocr_protocol::*;

fn roundtrip(kind: FrameKind, header: &[u8], payload: &[u8]) -> Frame {
    let mut buf = Vec::new();
    write_frame(&mut buf, kind, 0, header, payload).unwrap();
    read_frame(&mut buf.as_slice(), 0).unwrap()
}

#[test]
fn a_frame_roundtrips() {
    let f = roundtrip(FrameKind::PageResult, br#"{"page":1}"#, &[1, 2, 3]);
    assert_eq!(f.kind, FrameKind::PageResult);
    assert_eq!(f.header, br#"{"page":1}"#);
    assert_eq!(f.payload, vec![1, 2, 3]);
}

#[test]
fn bad_magic_is_rejected() {
    let mut buf = Vec::new();
    write_frame(&mut buf, FrameKind::Hello, 0, b"{}", &[]).unwrap();
    buf[0] = b'X';
    assert!(matches!(
        read_frame(&mut buf.as_slice(), 0),
        Err(FrameError::BadMagic)
    ));
}

fn prefix(kind: FrameKind, header_len: u32, payload_len: u64) -> Vec<u8> {
    let mut b = Vec::new();
    b.extend_from_slice(&MAGIC);
    b.push(PROTOCOL_VERSION);
    b.push(kind as u8);
    b.extend_from_slice(&0u32.to_le_bytes());
    b.extend_from_slice(&header_len.to_le_bytes());
    b.extend_from_slice(&payload_len.to_le_bytes());
    b
}

#[test]
fn oversized_header_is_rejected_before_allocation() {
    let buf = prefix(FrameKind::Hello, MAX_HEADER_BYTES + 1, 0);
    assert!(matches!(
        read_frame(&mut buf.as_slice(), 0),
        Err(FrameError::HeaderTooLarge(_))
    ));
}

#[test]
fn oversized_payload_is_rejected_before_allocation() {
    let buf = prefix(FrameKind::PageResult, 2, MAX_PAYLOAD_BYTES + 1);
    assert!(matches!(
        read_frame(&mut buf.as_slice(), 0),
        Err(FrameError::PayloadTooLarge(_))
    ));
}

#[test]
fn a_sequence_gap_is_rejected() {
    let mut buf = Vec::new();
    write_frame(&mut buf, FrameKind::Done, 5, b"{}", &[]).unwrap();
    assert!(matches!(
        read_frame(&mut buf.as_slice(), 4),
        Err(FrameError::Sequence {
            got: 5,
            expected: 4
        })
    ));
}

#[test]
fn eof_at_a_boundary_differs_from_truncation() {
    let empty: &[u8] = &[];
    assert!(matches!(read_frame(&mut &*empty, 0), Err(FrameError::Eof)));
    let mut buf = Vec::new();
    write_frame(&mut buf, FrameKind::Done, 0, b"{\"pages\":1}", &[]).unwrap();
    buf.truncate(buf.len() - 3);
    assert!(matches!(
        read_frame(&mut buf.as_slice(), 0),
        Err(FrameError::Truncated)
    ));
}

#[test]
fn headers_are_snake_case_and_strict() {
    assert_eq!(
        serde_json::to_value(PageSource::Failed {
            code: ErrorCode::Render
        })
        .unwrap(),
        serde_json::json!({"failed": {"code": "render"}})
    );
    assert!(serde_json::from_str::<Done>(r#"{"pages":1,"failed_pages":[],"extra":1}"#).is_err());
}

#[test]
fn version_and_kind_are_validated() {
    let mut buf = prefix(FrameKind::Hello, 0, 0);
    // A frame from the previous protocol version (1) is refused by version.
    buf[4] = PROTOCOL_VERSION - 1;
    assert!(matches!(
        read_frame(&mut buf.as_slice(), 0),
        Err(FrameError::Version(v)) if v == PROTOCOL_VERSION - 1
    ));
    let mut buf = prefix(FrameKind::Hello, 0, 0);
    buf[5] = 99;
    assert!(matches!(
        read_frame(&mut buf.as_slice(), 0),
        Err(FrameError::Kind(99))
    ));
}

#[test]
fn prefix_is_22_bytes_and_write_enforces_caps() {
    let mut buf = Vec::new();
    write_frame(&mut buf, FrameKind::Hello, 0, b"{}", &[]).unwrap();
    assert_eq!(buf.len(), 22 + 2);
    let big = vec![0u8; MAX_HEADER_BYTES as usize + 1];
    assert!(matches!(
        write_frame(&mut Vec::new(), FrameKind::Hello, 0, &big, &[]),
        Err(FrameError::HeaderTooLarge(_))
    ));
}

#[test]
fn analyze_roundtrips_through_json() {
    let json = r#"{"job_id":"j","input_path":"a.pdf","first_page":1,"last_page":null,"mode":"fast",
      "models":{"layout":"l","det":"d","rec":"r","rec_dict":"rd","table":null,"table_dict":null,"orientation":null},
      "threads":4,"max_side":1600,"det_limit":960,"render_dpi":150,"force_ocr":false,"crop_dir":"c","max_pages":10}"#;
    let a: Analyze = serde_json::from_str(json).unwrap();
    assert_eq!(a.mode, AnalyzeMode::Fast);
    let back: Analyze = serde_json::from_value(serde_json::to_value(&a).unwrap()).unwrap();
    assert_eq!(back, a);
}

struct OneByteReader<'a>(&'a [u8]);

impl std::io::Read for OneByteReader<'_> {
    fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
        if self.0.is_empty() || buf.is_empty() {
            return Ok(0);
        }
        buf[0] = self.0[0];
        self.0 = &self.0[1..];
        Ok(1)
    }
}

fn encoded(kind: FrameKind, header: &[u8], payload: &[u8]) -> Vec<u8> {
    let mut buf = Vec::new();
    write_frame(&mut buf, kind, 0, header, payload).unwrap();
    buf
}

#[test]
fn a_frame_roundtrips_through_a_one_byte_reader() {
    let buf = encoded(FrameKind::PageResult, br#"{"page":1}"#, &[9, 8, 7, 6]);
    let f = read_frame(&mut OneByteReader(&buf), 0).unwrap();
    assert_eq!(f.kind, FrameKind::PageResult);
    assert_eq!(f.header, br#"{"page":1}"#);
    assert_eq!(f.payload, vec![9, 8, 7, 6]);
}

#[test]
fn eof_inside_the_prefix_is_truncated() {
    let buf = encoded(FrameKind::Hello, b"{}", &[]);
    for cut in [1, 10, 21] {
        assert!(
            matches!(read_frame(&mut &buf[..cut], 0), Err(FrameError::Truncated)),
            "cut at {cut}"
        );
    }
    assert!(matches!(
        read_frame(&mut &buf[..0], 0),
        Err(FrameError::Eof)
    ));
}

#[test]
fn eof_inside_the_header_or_payload_is_truncated() {
    let buf = encoded(FrameKind::PageResult, b"{}", &[1, 2, 3, 4]);
    // inside the header
    assert!(matches!(
        read_frame(&mut &buf[..23], 0),
        Err(FrameError::Truncated)
    ));
    // inside the payload
    assert!(matches!(
        read_frame(&mut &buf[..buf.len() - 2], 0),
        Err(FrameError::Truncated)
    ));
    // a declared payload with nothing after the header
    assert!(matches!(
        read_frame(&mut &buf[..24], 0),
        Err(FrameError::Truncated)
    ));
}

#[test]
fn an_oversized_payload_fails_before_any_byte_is_written() {
    let big = vec![0u8; MAX_PAYLOAD_BYTES as usize + 1];
    let mut out = Vec::new();
    assert!(matches!(
        write_frame(&mut out, FrameKind::PageResult, 0, b"{}", &big),
        Err(FrameError::PayloadTooLarge(_))
    ));
    assert!(out.is_empty());
}

#[test]
fn page_source_rejects_unknown_fields() {
    assert!(
        serde_json::from_str::<PageSource>(r#"{"failed":{"code":"render","extra":1}}"#).is_err()
    );
    assert!(serde_json::from_str::<PageSource>(r#"{"failed":{"code":"render"}}"#).is_ok());
}

fn dense_page(blocks: usize) -> PageResult {
    PageResult {
        page: 3,
        width: 1654,
        height: 2339,
        source: PageSource::Ocr,
        blocks: (0..blocks as u32)
            .map(|id| Block {
                id,
                kind: BlockKind::Text,
                bbox: [1.0, 2.0, 3.0, 4.0],
                order: Some(id),
                text: Some("x".repeat(200)),
                cells: None,
                confidence: 0.9,
                crop_file: None,
                model: None,
                vlm_fallback: false,
            })
            .collect(),
        render_ms: 10,
        analyze_ms: 20,
        peak_working_set_mb: 800,
        doc_page_count: 12,
    }
}

#[test]
fn a_page_result_well_over_64_kib_roundtrips_with_a_small_header() {
    let page = dense_page(1000); // about 250 KB of blocks
    let mut buf = Vec::new();
    write_page_result(&mut buf, 0, &page).unwrap();
    assert!(buf.len() > 64 * 1024 * 3);
    let f = read_frame(&mut buf.as_slice(), 0).unwrap();
    assert_eq!(f.kind, FrameKind::PageResult);
    assert!(f.header.len() < 512, "header is {} bytes", f.header.len());
    assert_eq!(decode_page_result(&f).unwrap(), page);
}

#[test]
fn a_block_count_mismatch_or_wrong_kind_is_malformed() {
    let page = dense_page(2);
    let mut buf = Vec::new();
    write_page_result(&mut buf, 0, &page).unwrap();
    let mut f = read_frame(&mut buf.as_slice(), 0).unwrap();
    f.payload = serde_json::to_vec(&page.blocks[..1]).unwrap();
    assert!(matches!(decode_page_result(&f), Err(FrameError::Malformed(_))));
    f.kind = FrameKind::Done;
    assert!(matches!(decode_page_result(&f), Err(FrameError::Malformed(_))));
}

#[test]
fn a_page_whose_blocks_exceed_the_payload_cap_is_refused_not_truncated() {
    let mut page = dense_page(1);
    page.blocks[0].text = Some("y".repeat(MAX_PAYLOAD_BYTES as usize + 1));
    assert!(matches!(
        write_page_result(&mut Vec::new(), 0, &page),
        Err(FrameError::PayloadTooLarge(_))
    ));
}

/// A reader that yields some bytes, then fails with the given error kind.
struct FailingReader<'a> {
    data: &'a [u8],
    kind: std::io::ErrorKind,
}

impl std::io::Read for FailingReader<'_> {
    fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
        if self.data.is_empty() {
            return Err(std::io::Error::new(self.kind, "boom"));
        }
        let n = buf.len().min(self.data.len());
        buf[..n].copy_from_slice(&self.data[..n]);
        self.data = &self.data[n..];
        Ok(n)
    }
}

#[test]
fn unexpected_eof_from_the_reader_is_truncated_and_broken_pipe_is_io() {
    let buf = encoded(FrameKind::Done, br#"{"pages":1,"failed_pages":[]}"#, &[]);
    for cut in [5, 22, buf.len() - 1] {
        let mut r = FailingReader { data: &buf[..cut], kind: std::io::ErrorKind::UnexpectedEof };
        assert!(matches!(read_frame(&mut r, 0), Err(FrameError::Truncated)), "cut {cut}");
        let mut r = FailingReader { data: &buf[..cut], kind: std::io::ErrorKind::BrokenPipe };
        assert!(matches!(read_frame(&mut r, 0), Err(FrameError::Io(_))), "cut {cut}");
    }
}

#[test]
fn the_document_page_count_travels_in_the_page_header() {
    let page = dense_page(2);
    let mut wire = Vec::new();
    write_page_result(&mut wire, 0, &page).unwrap();
    let frame = read_frame(&mut wire.as_slice(), 0).unwrap();
    let header: serde_json::Value = serde_json::from_slice(&frame.header).unwrap();
    assert_eq!(header["doc_page_count"], 12);
    assert_eq!(decode_page_result(&frame).unwrap().doc_page_count, 12);
    // A header without it is not a page result any more.
    let mut stripped = header.clone();
    stripped.as_object_mut().unwrap().remove("doc_page_count");
    let frame = Frame { header: serde_json::to_vec(&stripped).unwrap(), ..frame };
    assert!(matches!(decode_page_result(&frame), Err(FrameError::Malformed(_))));
}

// ---- additions for accurate reading: additive, the protocol version stays 2 ---------------------

/// The exact JSON of a v2 `Analyze` request as the app sent it before `emit_crops` existed.
const V2_ANALYZE: &str = r#"{"job_id":"j","input_path":"a.pdf","first_page":1,"last_page":null,"mode":"fast","models":{"layout":"l","det":"d","rec":"r","rec_dict":"rd","table":null,"table_dict":null,"orientation":null},"threads":4,"max_side":1600,"det_limit":960,"render_dpi":150,"force_ocr":false,"crop_dir":"c","max_pages":10}"#;

#[test]
fn hello_ok_without_capabilities_decodes_as_empty() {
    let ok: HelloOk = serde_json::from_str(r#"{"protocol":2,"sidecar_version":"0.1.0","ort_version":"1.28"}"#).unwrap();
    assert!(ok.capabilities.is_empty());
    // And an empty list is not written: the reply is byte-identical to the one before.
    assert_eq!(
        serde_json::to_string(&ok).unwrap(),
        r#"{"protocol":2,"sidecar_version":"0.1.0","ort_version":"1.28"}"#
    );
    let new: HelloOk =
        serde_json::from_str(r#"{"protocol":2,"sidecar_version":"0.2.0","ort_version":"1.28","capabilities":["emit_crops"]}"#)
            .unwrap();
    assert_eq!(new.capabilities, vec![CAP_EMIT_CROPS.to_string()]);
}

#[test]
fn analyze_without_emit_crops_serializes_exactly_as_before() {
    let a: Analyze = serde_json::from_str(V2_ANALYZE).unwrap();
    assert!(!a.emit_crops);
    // Byte-compare with the v2 fixture: an older sidecar (deny_unknown_fields) never sees the field.
    assert_eq!(serde_json::to_string(&a).unwrap(), V2_ANALYZE);
    let mut on = a.clone();
    on.emit_crops = true;
    assert!(serde_json::to_string(&on).unwrap().contains(r#""emit_crops":true"#));
    assert_eq!(serde_json::from_str::<Analyze>(&serde_json::to_string(&on).unwrap()).unwrap(), on);
}

#[test]
fn block_without_model_and_vlm_fallback_decodes() {
    let b: Block = serde_json::from_str(
        r#"{"id":1,"kind":"text","bbox":[0.0,0.0,1.0,1.0],"order":1,"text":"t","cells":null,"confidence":0.9,"crop_file":null}"#,
    )
    .unwrap();
    assert_eq!((b.model.as_deref(), b.vlm_fallback), (None, false));
    // Not written when unset, so a page file from before decodes and one from now matches it.
    let json = serde_json::to_string(&b).unwrap();
    assert!(!json.contains("model") && !json.contains("vlm_fallback"), "{json}");
    let set = Block { model: Some("glm-ocr".into()), vlm_fallback: true, ..b };
    let json = serde_json::to_string(&set).unwrap();
    assert!(json.contains(r#""model":"glm-ocr""#) && json.contains(r#""vlm_fallback":true"#), "{json}");
    assert_eq!(serde_json::from_str::<Block>(&json).unwrap(), set);
}

#[test]
fn rgb_crop_header_roundtrips_and_rejects_short_and_oversize_files() {
    let rgb: Vec<u8> = (0..2 * 3 * 3).map(|i| i as u8).collect();
    let bytes = encode_rgb_crop(2, 3, &rgb).unwrap();
    assert_eq!(bytes.len(), RGB_CROP_HEADER_LEN + rgb.len());
    assert_eq!(&bytes[..8], &[2, 0, 0, 0, 3, 0, 0, 0]);
    assert_eq!(decode_rgb_crop(&bytes).unwrap(), (2, 3, rgb.as_slice()));
    // Shorter than the header, or than the header promises.
    assert_eq!(decode_rgb_crop(&bytes[..5]), Err(RgbCropError::Short));
    assert_eq!(decode_rgb_crop(&bytes[..bytes.len() - 1]), Err(RgbCropError::SizeMismatch));
    // A zero side, or data that is not width * height * 3.
    assert_eq!(encode_rgb_crop(0, 3, &[]), Err(RgbCropError::SizeMismatch));
    assert_eq!(encode_rgb_crop(2, 3, &rgb[..10]), Err(RgbCropError::SizeMismatch));
    // Larger than the cap: refused to write, refused to read.
    let side = 3000u32; // 3000 * 3000 * 3 = 27 MB: fine
    assert!(encode_rgb_crop(side, side, &vec![0u8; (side * side * 3) as usize]).is_ok());
    let huge = 5000u32; // 5000 * 5000 * 3 = 75 MB: over the 64 MiB cap
    assert_eq!(
        encode_rgb_crop(huge, huge, &vec![0u8; (huge * huge * 3) as usize]),
        Err(RgbCropError::Oversize)
    );
    let mut oversize = vec![0u8; MAX_RGB_CROP_BYTES + 1];
    oversize[..4].copy_from_slice(&1u32.to_le_bytes());
    assert_eq!(decode_rgb_crop(&oversize), Err(RgbCropError::Oversize));
}

// ---- crop names, the bounded crop read and the crop list ---------------------------------------

/// A fresh, empty folder under the target's temp dir (no dev-dependency).
fn scratch(name: &str) -> std::path::PathBuf {
    let dir = std::path::Path::new(env!("CARGO_TARGET_TMPDIR")).join(format!("codec-{}-{name}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    dir
}

#[test]
fn crop_names_are_canonical_and_parse_back() {
    assert_eq!(region_crop_name(3, 7), "p0003-b007.rgb");
    assert_eq!(page_crop_name(3), "p0003-page.rgb");
    assert_eq!(region_crop_name(12345, 1234), "p12345-b1234.rgb");
    assert_eq!(parse_rgb_crop_name("p0003-b007.rgb"), Some(RgbCropName::Region { page: 3, block_id: 7 }));
    assert_eq!(parse_rgb_crop_name("p0003-page.rgb"), Some(RgbCropName::Page { page: 3 }));
    assert_eq!(parse_rgb_crop_name("p12345-b1234.rgb"), Some(RgbCropName::Region { page: 12345, block_id: 1234 }));
    for (page, id) in [(0, 0), (1, 999), (9999, 1000), (u32::MAX, u32::MAX)] {
        assert_eq!(parse_rgb_crop_name(&region_crop_name(page, id)), Some(RgbCropName::Region { page, block_id: id }));
        assert_eq!(parse_rgb_crop_name(&page_crop_name(page)), Some(RgbCropName::Page { page }));
    }
}

#[test]
fn only_canonical_crop_names_are_safe() {
    let long = format!("p{}-b001.rgb", "0".repeat(190));
    let refused = [
        "",
        "../p0001-b001.rgb",
        "p0001-b001.rgb/x",
        "P0001-B001.RGB",
        "p1-b1.rgb",
        "p00001-b001.rgb",
        "p0001-b0001.rgb",
        "p0001-b001.png",
        "p0001-b001.rgb.tmp",
        "p0001-b001.rgb ",
        " p0001-b001.rgb",
        "p0001-b001\0.rgb",
        "p\u{0661}\u{0662}\u{0663}\u{0664}-b001.rgb",
        "p0001-b-01.rgb",
        "p+001-b001.rgb",
        "p0001-page.png",
        "p0001-pagex.rgb",
        "p0001-b99999999999.rgb",
        "p0001b001.rgb",
        long.as_str(),
    ];
    for name in refused {
        assert!(!safe_rgb_crop_name(name), "{name:?} must be refused");
        assert_eq!(parse_rgb_crop_name(name), None, "{name:?}");
    }
    assert!(safe_rgb_crop_name("p0001-b001.rgb"));
    assert!(safe_rgb_crop_name("p0001-page.rgb"));
}

#[test]
fn read_rgb_crop_returns_what_encode_rgb_crop_wrote() {
    let dir = scratch("roundtrip");
    let rgb: Vec<u8> = (0..4 * 5 * 3).map(|i| (i * 7) as u8).collect();
    let path = dir.join("p0001-b000.rgb");
    std::fs::write(&path, encode_rgb_crop(4, 5, &rgb).unwrap()).unwrap();
    let crop = read_rgb_crop(&path).unwrap();
    assert_eq!((crop.width, crop.height), (4, 5));
    assert_eq!(crop.rgb, rgb);
}

#[test]
fn read_rgb_crop_refuses_short_mismatched_oversize_and_non_files() {
    let dir = scratch("refuses");
    let short = dir.join("short.rgb");
    std::fs::write(&short, [1u8, 2, 3, 4, 5]).unwrap();
    assert_eq!(read_rgb_crop(&short).unwrap_err(), RgbCropError::Short);

    let mismatched = dir.join("mismatched.rgb");
    let bytes = encode_rgb_crop(2, 3, &[9u8; 18]).unwrap();
    std::fs::write(&mismatched, &bytes[..bytes.len() - 1]).unwrap();
    assert_eq!(read_rgb_crop(&mismatched).unwrap_err(), RgbCropError::SizeMismatch);

    // A sparse file one byte over the cap: refused from its length, before a byte is read.
    let oversize = dir.join("oversize.rgb");
    std::fs::File::create(&oversize).unwrap().set_len(MAX_RGB_CROP_BYTES as u64 + 1).unwrap();
    assert_eq!(read_rgb_crop(&oversize).unwrap_err(), RgbCropError::Oversize);
    let _ = std::fs::remove_file(&oversize);

    let folder = dir.join("a-folder.rgb");
    std::fs::create_dir_all(&folder).unwrap();
    assert_eq!(read_rgb_crop(&folder).unwrap_err(), RgbCropError::NotAFile);

    assert_eq!(read_rgb_crop(&dir.join("missing.rgb")).unwrap_err(), RgbCropError::Io(std::io::ErrorKind::NotFound));
}

#[test]
fn the_crop_list_is_the_plan_list() {
    use BlockKind::*;
    for kind in [DocTitle, ParagraphTitle, Text, List, Table, Formula, Caption, Other] {
        assert!(wants_rgb_crop(kind), "{kind:?}");
    }
    for kind in [Figure, Chart, Seal, Header, Footer, PageNumber] {
        assert!(!wants_rgb_crop(kind), "{kind:?}");
    }
    assert_eq!(MIN_VLM_REGIONS, 2);
}
