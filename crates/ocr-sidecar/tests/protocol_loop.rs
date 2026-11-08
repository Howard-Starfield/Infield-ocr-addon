mod common;
use common::{pipe, PipeReader, PipeWriter};
use infield_ocr::pipeline::StubAnalyzer;
use infield_ocr::server::serve;
use infield_ocr_protocol::*;
use std::path::PathBuf;
use std::thread::JoinHandle;

struct Client {
    tx: PipeWriter,
    rx: PipeReader,
    out_seq: u32,
    in_seq: u32,
    server: Option<JoinHandle<()>>,
}

impl Client {
    fn start(analyzer: StubAnalyzer) -> Client {
        let (to_server, server_in) = pipe();
        let (server_out, from_server) = pipe();
        let server = std::thread::spawn(move || {
            let mut a = analyzer;
            let _ = serve(server_in, server_out, &mut a);
        });
        Client { tx: to_server, rx: from_server, out_seq: 0, in_seq: 0, server: Some(server) }
    }
    fn send<T: serde::Serialize>(&mut self, kind: FrameKind, h: &T) {
        write_frame(&mut self.tx, kind, self.out_seq, &serde_json::to_vec(h).unwrap(), &[]).unwrap();
        self.out_seq += 1;
    }
    fn hello(&mut self) -> HelloOk {
        self.send(FrameKind::Hello, &Hello { protocol: PROTOCOL_VERSION, app_version: "0.0.0-test".into() });
        let f = self.recv();
        assert_eq!(f.kind, FrameKind::HelloOk);
        serde_json::from_slice(&f.header).unwrap()
    }
    fn recv(&mut self) -> Frame {
        let f = read_frame(&mut self.rx, self.in_seq).expect("frame from sidecar");
        self.in_seq += 1;
        f
    }
    fn analyze(&mut self, first: u32, last: Option<u32>) {
        self.send(FrameKind::Analyze, &analyze_req(first, last));
    }
    fn finish(mut self) {
        drop(self.tx);
        self.server.take().unwrap().join().unwrap();
    }
}

fn analyze_req(first: u32, last: Option<u32>) -> Analyze {
    Analyze {
        job_id: "job-1".into(),
        input_path: PathBuf::from("stub.pdf"),
        first_page: first,
        last_page: last,
        mode: AnalyzeMode::Fast,
        models: FastModels {
            layout: "l.onnx".into(),
            det: "d.onnx".into(),
            rec: "r.onnx".into(),
            rec_dict: "r.txt".into(),
            table: None,
            table_dict: None,
            orientation: None,
        },
        threads: 1,
        max_side: 3000,
        det_limit: 1280,
        render_dpi: 200,
        force_ocr: false,
        crop_dir: PathBuf::from("crops"),
        max_pages: 100,
        emit_crops: false,
    }
}

#[test]
fn hello_then_analyze_streams_pages_then_done() {
    let mut c = Client::start(StubAnalyzer::new(5));
    let ok = c.hello();
    assert_eq!(ok.protocol, PROTOCOL_VERSION);
    assert_eq!(ok.sidecar_version, env!("CARGO_PKG_VERSION"));
    c.analyze(2, Some(4));
    for want in 2..=4u32 {
        let f = c.recv();
        assert_eq!(f.kind, FrameKind::PageResult);
        let p = decode_page_result(&f).unwrap();
        assert_eq!(p.page, want);
        assert_eq!(p.source, PageSource::Ocr);
    }
    let f = c.recv();
    assert_eq!(f.kind, FrameKind::Done);
    let d: Done = serde_json::from_slice(&f.header).unwrap();
    assert_eq!((d.pages, d.failed_pages), (3, vec![]));
    c.send(FrameKind::Shutdown, &serde_json::json!({}));
    c.finish();
}

#[test]
fn hello_ok_lists_emit_crops() {
    let mut c = Client::start(StubAnalyzer::new(1));
    let ok = c.hello();
    assert_eq!(ok.protocol, PROTOCOL_VERSION, "the version stays 2: the capability is additive");
    assert_eq!(ok.capabilities, vec![CAP_EMIT_CROPS.to_string()]);
    c.finish();
}

#[test]
fn two_analyze_chunks_in_one_session_both_complete() {
    // The app reads a document in chunks of pages through one session.
    let mut c = Client::start(StubAnalyzer::new(8));
    c.hello();
    for (first, last) in [(1u32, 4u32), (5, 8)] {
        c.analyze(first, Some(last));
        for want in first..=last {
            let f = c.recv();
            assert_eq!(f.kind, FrameKind::PageResult);
            assert_eq!(decode_page_result(&f).unwrap().page, want);
        }
        let f = c.recv();
        assert_eq!(f.kind, FrameKind::Done);
        let d: Done = serde_json::from_slice(&f.header).unwrap();
        assert_eq!((d.pages, d.failed_pages), (4, vec![]));
    }
    c.send(FrameKind::Shutdown, &serde_json::json!({}));
    c.finish();
}

#[test]
fn an_analyze_asking_for_crops_is_served_like_any_other() {
    let mut c = Client::start(StubAnalyzer::new(2));
    c.hello();
    let mut req = analyze_req(1, Some(2));
    req.emit_crops = true;
    c.send(FrameKind::Analyze, &req);
    for want in 1..=2u32 {
        assert_eq!(decode_page_result(&c.recv()).unwrap().page, want);
    }
    assert_eq!(c.recv().kind, FrameKind::Done);
    c.send(FrameKind::Shutdown, &serde_json::json!({}));
    c.finish();
}

#[test]
fn a_failed_page_is_reported_and_listed_in_done() {
    let mut a = StubAnalyzer::new(3);
    a.fail_pages = vec![2];
    let mut c = Client::start(a);
    c.hello();
    c.analyze(1, None);
    let mut sources = Vec::new();
    loop {
        let f = c.recv();
        if f.kind == FrameKind::Done {
            let d: Done = serde_json::from_slice(&f.header).unwrap();
            assert_eq!(d.failed_pages, vec![2]);
            break;
        }
        sources.push(decode_page_result(&f).unwrap().source);
    }
    assert_eq!(sources[1], PageSource::Failed { code: ErrorCode::Render });
    c.finish();
}

#[test]
fn cancel_during_analyze_stops_with_a_cancelled_error() {
    let mut a = StubAnalyzer::new(50);
    a.hold_after = Some(1);
    let mut c = Client::start(a);
    c.hello();
    c.analyze(1, None);
    assert_eq!(c.recv().kind, FrameKind::PageResult);
    c.send(FrameKind::Cancel, &serde_json::json!({}));
    let f = c.recv();
    assert_eq!(f.kind, FrameKind::Error);
    let e: ErrorFrame = serde_json::from_slice(&f.header).unwrap();
    assert_eq!(e.code, ErrorCode::Cancelled);
    // The process is still alive and exits on a clean EOF.
    c.finish();
}

#[test]
fn shutdown_during_analyze_cancels_and_exits() {
    let mut a = StubAnalyzer::new(50);
    a.hold_after = Some(0);
    let mut c = Client::start(a);
    c.hello();
    c.analyze(1, None);
    c.send(FrameKind::Shutdown, &serde_json::json!({}));
    let f = c.recv();
    assert_eq!(f.kind, FrameKind::Error);
    c.server.take().unwrap().join().unwrap();
}

#[test]
fn clean_eof_after_hello_exits_without_an_error_frame() {
    let mut c = Client::start(StubAnalyzer::new(1));
    c.hello();
    drop(c.tx);
    c.server.take().unwrap().join().unwrap();
    assert!(matches!(read_frame(&mut c.rx, c.in_seq), Err(FrameError::Eof)));
}

#[test]
fn analyze_before_hello_is_refused() {
    let mut c = Client::start(StubAnalyzer::new(1));
    c.analyze(1, None);
    let f = c.recv();
    assert_eq!(f.kind, FrameKind::Error);
    let e: ErrorFrame = serde_json::from_slice(&f.header).unwrap();
    assert_eq!(e.code, ErrorCode::BadInput);
    c.server.take().unwrap().join().unwrap();
}

#[test]
fn a_protocol_mismatch_in_hello_is_refused() {
    let mut c = Client::start(StubAnalyzer::new(1));
    c.send(FrameKind::Hello, &Hello { protocol: 99, app_version: "x".into() });
    assert_eq!(c.recv().kind, FrameKind::Error);
    c.server.take().unwrap().join().unwrap();
}

#[test]
fn a_garbage_header_gets_an_error_and_the_loop_keeps_serving() {
    let mut c = Client::start(StubAnalyzer::new(2));
    c.hello();
    write_frame(&mut c.tx, FrameKind::Analyze, c.out_seq, b"{\"nope\":1}", &[]).unwrap();
    c.out_seq += 1;
    let f = c.recv();
    assert_eq!(f.kind, FrameKind::Error);
    let e: ErrorFrame = serde_json::from_slice(&f.header).unwrap();
    assert_eq!(e.code, ErrorCode::BadInput);
    c.analyze(1, None);
    assert_eq!(c.recv().kind, FrameKind::PageResult);
    c.finish();
}

#[test]
fn a_bad_frame_on_the_wire_ends_the_loop() {
    let (mut to_server, server_in) = pipe();
    let (server_out, mut from_server) = pipe();
    let h = std::thread::spawn(move || {
        let mut a = StubAnalyzer::new(1);
        serve(server_in, server_out, &mut a)
    });
    std::io::Write::write_all(&mut to_server, &[b'X'; 22]).unwrap();
    let f = read_frame(&mut from_server, 0).unwrap();
    assert_eq!(f.kind, FrameKind::Error);
    assert!(h.join().unwrap().is_err());
}

#[test]
fn a_dense_page_over_64_kib_streams_with_a_small_header() {
    let mut a = StubAnalyzer::new(2);
    a.dense = Some((800, 200)); // about 180 KB of blocks per page
    let mut c = Client::start(a);
    c.hello();
    c.analyze(1, None);
    for want in 1..=2 {
        let f = c.recv();
        assert_eq!(f.kind, FrameKind::PageResult);
        assert!(f.header.len() < 512 && f.payload.len() > 64 * 1024);
        let p = decode_page_result(&f).unwrap();
        assert_eq!((p.page, p.blocks.len()), (want, 800));
    }
    assert_eq!(c.recv().kind, FrameKind::Done);
    c.finish();
}

#[test]
fn a_page_too_big_for_any_frame_becomes_a_failed_page_and_the_job_continues() {
    let mut a = StubAnalyzer::new(2);
    a.dense = Some((1, MAX_PAYLOAD_BYTES as usize + 1));
    let mut c = Client::start(a);
    c.hello();
    c.analyze(1, None);
    for want in 1..=2 {
        let p = decode_page_result(&c.recv()).unwrap();
        assert_eq!(p.page, want);
        assert_eq!(p.source, PageSource::Failed { code: ErrorCode::Internal });
        assert!(p.blocks.is_empty());
    }
    let d: Done = serde_json::from_slice(&c.recv().header).unwrap();
    assert_eq!(d.failed_pages, vec![1, 2]);
    c.finish();
}
