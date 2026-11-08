//! `emit_crops` on a born-digital page: the text layer is used and no crop is written.
mod common;
use common::{make_pdf, text_lines};
use infield_ocr::pipeline::{Analyzer, OarAnalyzer};
use infield_ocr_protocol::*;
use std::path::PathBuf;

#[test]
fn text_layer_pages_emit_no_crops() {
    let dir = tempfile::tempdir().unwrap();
    let pdf = dir.path().join("born-digital.pdf");
    let content = text_lines(&[
        "The quick brown fox jumps over the lazy dog.",
        "Pack my box with five dozen liquor jugs.",
    ]);
    std::fs::write(&pdf, make_pdf(&content, false)).unwrap();
    let crop_dir = dir.path().join("crops");
    // The models do not exist: a text-layer page must never load them.
    let missing = PathBuf::from("does-not-exist.onnx");
    let req = Analyze {
        job_id: "j".into(),
        input_path: pdf,
        first_page: 1,
        last_page: None,
        mode: AnalyzeMode::Fast,
        models: FastModels {
            layout: missing.clone(),
            det: missing.clone(),
            rec: missing.clone(),
            rec_dict: missing,
            table: None,
            table_dict: None,
            orientation: None,
        },
        threads: 1,
        max_side: 3000,
        det_limit: 1280,
        render_dpi: 200,
        force_ocr: false,
        crop_dir: crop_dir.clone(),
        max_pages: 0,
        emit_crops: true,
    };
    let mut pages = Vec::new();
    let mut analyzer = OarAnalyzer;
    let done = analyzer
        .analyze(&req, &mut || false, &mut |p| {
            pages.push(p);
            Ok(())
        })
        .expect("a text-layer page needs no models");
    assert_eq!((done.pages, done.failed_pages), (1, vec![]));
    assert_eq!(pages[0].source, PageSource::TextLayer);
    assert!(pages[0].blocks.iter().all(|b| b.crop_file.is_none()));
    assert!(!crop_dir.exists() || std::fs::read_dir(&crop_dir).unwrap().next().is_none(), "no crop file was written");
}
