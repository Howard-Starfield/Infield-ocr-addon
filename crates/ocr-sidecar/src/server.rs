//! The protocol loop: Hello, then Analyze / Cancel / Shutdown until EOF.
//!
//! A reader thread parses frames into a channel so that `Cancel` and `Shutdown`
//! can arrive while a page is being analyzed. The analyzer polls for them
//! between pages (a page in flight cannot be interrupted; the parent's Job
//! Object and page timeout are the hard limits).
//!
//! Replies: `HelloOk`; per page a `PageResult` (small header, blocks in the payload),
//! then `Done`; or a single
//! `Error` frame that ends the request. A cancelled request ends with
//! `Error { code: cancelled }` and the process keeps serving until `Shutdown`
//! or a clean EOF. Error messages never contain document text.

use crate::pipeline::{AnalyzeError, Analyzer};
use crate::{ORT_VERSION, VERSION};
use infield_ocr_protocol::{
    read_frame, write_frame, write_page_result, Analyze, ErrorCode, ErrorFrame, Frame, FrameError, FrameKind, Hello,
    HelloOk, PageResult, PageSource, CAP_EMIT_CROPS, PROTOCOL_VERSION,
};
use serde::Serialize;
use std::collections::VecDeque;
use std::io::{Read, Write};
use std::sync::mpsc::{channel, Receiver, TryRecvError};

#[derive(Debug)]
pub enum ServeError {
    Frame(FrameError),
    Protocol(&'static str),
}

impl std::fmt::Display for ServeError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Frame(e) => write!(f, "{e}"),
            Self::Protocol(m) => write!(f, "{m}"),
        }
    }
}

impl std::error::Error for ServeError {}

impl From<FrameError> for ServeError {
    fn from(e: FrameError) -> Self {
        Self::Frame(e)
    }
}

type Incoming = Result<Frame, FrameError>;

struct Out<W: Write> {
    w: W,
    seq: u32,
}

impl<W: Write> Out<W> {
    fn send<T: Serialize>(&mut self, kind: FrameKind, header: &T) -> Result<(), FrameError> {
        let bytes = serde_json::to_vec(header).expect("header serializes");
        write_frame(&mut self.w, kind, self.seq, &bytes, &[])?;
        self.seq += 1;
        Ok(())
    }
    fn error(&mut self, code: ErrorCode, message: &str) -> Result<(), FrameError> {
        self.send(FrameKind::Error, &ErrorFrame { code, message: message.to_string() })
    }
}

fn spawn_reader<R: Read + Send + 'static>(mut r: R) -> Receiver<Incoming> {
    let (tx, rx) = channel();
    std::thread::spawn(move || {
        let mut seq = 0u32;
        loop {
            let frame = read_frame(&mut r, seq);
            let stop = frame.is_err();
            seq = seq.wrapping_add(1);
            if tx.send(frame).is_err() || stop {
                break;
            }
        }
    });
    rx
}

enum Next {
    Frame(Frame),
    End,
    Bad(FrameError),
}

struct Inbox {
    rx: Receiver<Incoming>,
    pending: VecDeque<Incoming>,
}

impl Inbox {
    fn next(&mut self) -> Next {
        let item = match self.pending.pop_front() {
            Some(i) => i,
            None => self.rx.recv().unwrap_or(Err(FrameError::Eof)),
        };
        match item {
            Ok(f) => Next::Frame(f),
            Err(FrameError::Eof) => Next::End,
            Err(e) => Next::Bad(e),
        }
    }
}

/// Serve one session. Returns `Ok` on a clean EOF or `Shutdown`.
pub fn serve<R, W, A>(input: R, output: W, analyzer: &mut A) -> Result<(), ServeError>
where
    R: Read + Send + 'static,
    W: Write,
    A: Analyzer,
{
    let mut inbox = Inbox { rx: spawn_reader(input), pending: VecDeque::new() };
    let mut out = Out { w: output, seq: 0 };

    match inbox.next() {
        Next::End => return Ok(()),
        Next::Bad(e) => {
            let _ = out.error(ErrorCode::Internal, &e.to_string());
            return Err(e.into());
        }
        Next::Frame(f) if f.kind == FrameKind::Hello => {
            match serde_json::from_slice::<Hello>(&f.header) {
                Ok(h) if h.protocol == PROTOCOL_VERSION => {}
                _ => {
                    out.error(ErrorCode::BadInput, "unsupported protocol or malformed hello")?;
                    return Err(ServeError::Protocol("bad hello"));
                }
            }
            out.send(
                FrameKind::HelloOk,
                &HelloOk {
                    protocol: PROTOCOL_VERSION,
                    sidecar_version: VERSION.to_string(),
                    ort_version: ORT_VERSION.to_string(),
                    capabilities: vec![CAP_EMIT_CROPS.to_string()],
                },
            )?;
        }
        Next::Frame(_) => {
            out.error(ErrorCode::BadInput, "hello must be the first frame")?;
            return Err(ServeError::Protocol("no hello"));
        }
    }

    loop {
        match inbox.next() {
            Next::End => return Ok(()),
            Next::Bad(e) => {
                let _ = out.error(ErrorCode::Internal, &e.to_string());
                return Err(e.into());
            }
            Next::Frame(f) => match f.kind {
                FrameKind::Shutdown => return Ok(()),
                FrameKind::Cancel => {} // nothing running
                FrameKind::Analyze => {
                    if !run_analyze(&f, &mut inbox, &mut out, analyzer)? {
                        return Ok(());
                    }
                }
                _ => out.error(ErrorCode::BadInput, "unexpected frame kind")?,
            },
        }
    }
}

/// Returns `Ok(false)` when the session must end (Shutdown or EOF seen mid-run).
fn run_analyze<W: Write, A: Analyzer>(
    frame: &Frame,
    inbox: &mut Inbox,
    out: &mut Out<W>,
    analyzer: &mut A,
) -> Result<bool, ServeError> {
    let req: Analyze = match serde_json::from_slice(&frame.header) {
        Ok(r) => r,
        Err(_) => {
            out.error(ErrorCode::BadInput, "malformed analyze request")?;
            return Ok(true);
        }
    };
    let (mut cancel, mut end) = (false, false);
    let mut write_failed: Option<FrameError> = None;
    // Pages whose blocks were too big for a frame; sent as failed pages instead.
    let mut oversized: Vec<u32> = Vec::new();
    let result = {
        let rx = &inbox.rx;
        let pending = &mut inbox.pending;
        let mut cancelled = || {
            loop {
                match rx.try_recv() {
                    Ok(Ok(f)) => match f.kind {
                        FrameKind::Cancel => cancel = true,
                        FrameKind::Shutdown => {
                            cancel = true;
                            end = true;
                        }
                        _ => pending.push_back(Ok(f)),
                    },
                    Ok(Err(FrameError::Eof)) | Err(TryRecvError::Disconnected) => {
                        cancel = true;
                        end = true;
                        break;
                    }
                    Ok(Err(e)) => {
                        cancel = true;
                        pending.push_back(Err(e));
                        break;
                    }
                    Err(TryRecvError::Empty) => break,
                }
            }
            cancel
        };
        let mut sink = |page: PageResult| {
            // The header stays small; the blocks travel in the payload (64 MiB cap).
            let sent = match write_page_result(&mut out.w, out.seq, &page) {
                Err(FrameError::PayloadTooLarge(_)) | Err(FrameError::HeaderTooLarge(_)) => {
                    oversized.push(page.page);
                    let failed = PageResult {
                        source: PageSource::Failed { code: ErrorCode::Internal },
                        blocks: Vec::new(),
                        ..page
                    };
                    write_page_result(&mut out.w, out.seq, &failed)
                }
                other => other,
            };
            match sent {
                Ok(()) => {
                    out.seq += 1;
                    Ok(())
                }
                Err(e) => {
                    write_failed = Some(e);
                    Err(AnalyzeError::new(ErrorCode::Internal, "the parent closed the pipe"))
                }
            }
        };
        analyzer.analyze(&req, &mut cancelled, &mut sink)
    };
    if let Some(e) = write_failed {
        return Err(e.into());
    }
    match result {
        Ok(mut done) => {
            for p in oversized {
                if !done.failed_pages.contains(&p) {
                    done.failed_pages.push(p);
                }
            }
            done.failed_pages.sort_unstable();
            out.send(FrameKind::Done, &done)?
        }
        Err(e) => out.error(e.code, &e.message)?,
    }
    Ok(!end)
}
