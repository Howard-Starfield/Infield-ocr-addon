use infield_ocr_protocol::*;
use std::io::Write;
use std::process::{Command, Stdio};

const EXE: &str = env!("CARGO_BIN_EXE_infield-ocr");

fn spawn() -> std::process::Child {
    Command::new(EXE)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .unwrap()
}

#[test]
fn version_flag_prints_the_crate_version() {
    let out = Command::new(EXE).arg("--version").output().unwrap();
    assert!(out.status.success());
    let s = String::from_utf8(out.stdout).unwrap();
    assert!(s.contains(env!("CARGO_PKG_VERSION")), "{s:?}");
}

#[test]
fn the_real_exe_answers_hello_and_exits_on_shutdown() {
    let mut child = spawn();
    let mut stdin = child.stdin.take().unwrap();
    let mut stdout = child.stdout.take().unwrap();
    write_frame(&mut stdin, FrameKind::Hello, 0, br#"{"protocol":2,"app_version":"t"}"#, &[]).unwrap();
    let f = read_frame(&mut stdout, 0).unwrap();
    assert_eq!(f.kind, FrameKind::HelloOk);
    let ok: HelloOk = serde_json::from_slice(&f.header).unwrap();
    assert_eq!(ok.sidecar_version, env!("CARGO_PKG_VERSION"));
    assert!(!ok.ort_version.is_empty());
    write_frame(&mut stdin, FrameKind::Shutdown, 1, b"{}", &[]).unwrap();
    stdin.flush().unwrap();
    assert!(child.wait().unwrap().success());
}

#[test]
fn a_missing_input_is_a_bad_input_error_not_a_crash() {
    let mut child = spawn();
    let mut stdin = child.stdin.take().unwrap();
    let mut stdout = child.stdout.take().unwrap();
    write_frame(&mut stdin, FrameKind::Hello, 0, br#"{"protocol":2,"app_version":"t"}"#, &[]).unwrap();
    read_frame(&mut stdout, 0).unwrap();
    let req = serde_json::json!({
        "job_id": "j", "input_path": "definitely-not-here.pdf", "first_page": 1, "last_page": null,
        "mode": "fast",
        "models": {"layout":"l","det":"d","rec":"r","rec_dict":"t","table":null,"table_dict":null,"orientation":null},
        "threads": 1, "max_side": 3000, "det_limit": 1280, "render_dpi": 200, "force_ocr": false,
        "crop_dir": "crops", "max_pages": 10
    });
    write_frame(&mut stdin, FrameKind::Analyze, 1, req.to_string().as_bytes(), &[]).unwrap();
    let f = read_frame(&mut stdout, 1).unwrap();
    assert_eq!(f.kind, FrameKind::Error);
    let e: ErrorFrame = serde_json::from_slice(&f.header).unwrap();
    assert_eq!(e.code, ErrorCode::BadInput);
    drop(stdin);
    assert!(child.wait().unwrap().success());
}

#[cfg(debug_assertions)]
#[test]
fn a_panic_logs_only_its_location_never_its_message() {
    let out = Command::new(EXE).arg("--panic-test").output().unwrap();
    assert!(!out.status.success());
    let err = String::from_utf8_lossy(&out.stderr);
    assert!(err.contains("panic at"), "{err}");
    assert!(!err.contains("SECRET-DOCUMENT-TEXT"), "{err}");
}

fn crc32(data: &[u8]) -> u32 {
    let mut c = 0xFFFF_FFFFu32;
    for b in data {
        c ^= *b as u32;
        for _ in 0..8 {
            c = if c & 1 != 0 { 0xEDB8_8320 ^ (c >> 1) } else { c >> 1 };
        }
    }
    !c
}

/// A PNG that is only a signature, a valid IHDR claiming a huge size and an empty IDAT.
fn png_header_only(w: u32, h: u32) -> Vec<u8> {
    let mut ihdr = Vec::new();
    ihdr.extend_from_slice(&w.to_be_bytes());
    ihdr.extend_from_slice(&h.to_be_bytes());
    ihdr.extend_from_slice(&[8, 2, 0, 0, 0]); // 8-bit RGB
    let mut chunk = b"IHDR".to_vec();
    chunk.extend_from_slice(&ihdr);
    let mut out = vec![0x89, b'P', b'N', b'G', 0x0D, 0x0A, 0x1A, 0x0A];
    out.extend_from_slice(&(ihdr.len() as u32).to_be_bytes());
    out.extend_from_slice(&chunk);
    out.extend_from_slice(&crc32(&chunk).to_be_bytes());
    // The decoder reads up to the first IDAT before it reports the size.
    out.extend_from_slice(&0u32.to_be_bytes());
    out.extend_from_slice(b"IDAT");
    out.extend_from_slice(&crc32(b"IDAT").to_be_bytes());
    out
}

#[test]
fn an_oversized_image_is_refused_from_its_header_even_with_max_side_zero() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("huge.png");
    std::fs::write(&path, png_header_only(30_000, 30_000)).unwrap();
    let mut child = spawn();
    let mut stdin = child.stdin.take().unwrap();
    let mut stdout = child.stdout.take().unwrap();
    write_frame(&mut stdin, FrameKind::Hello, 0, br#"{"protocol":2,"app_version":"t"}"#, &[]).unwrap();
    read_frame(&mut stdout, 0).unwrap();
    let req = serde_json::json!({
        "job_id": "j", "input_path": path, "first_page": 1, "last_page": null, "mode": "fast",
        "models": {"layout":"l","det":"d","rec":"r","rec_dict":"t","table":null,"table_dict":null,"orientation":null},
        "threads": 1, "max_side": 0, "det_limit": 1280, "render_dpi": 200, "force_ocr": false,
        "crop_dir": dir.path().join("crops"), "max_pages": 10
    });
    write_frame(&mut stdin, FrameKind::Analyze, 1, req.to_string().as_bytes(), &[]).unwrap();
    let f = read_frame(&mut stdout, 1).unwrap();
    assert_eq!(f.kind, FrameKind::Error);
    let e: ErrorFrame = serde_json::from_slice(&f.header).unwrap();
    assert_eq!(e.code, ErrorCode::UnsupportedInput);
    assert!(e.message.contains("too large"), "{}", e.message);
    drop(stdin);
    assert!(child.wait().unwrap().success());
}
