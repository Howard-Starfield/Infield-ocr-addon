//! Shared test helpers: in-memory pipes for the protocol loop and a tiny PDF builder.
#![allow(dead_code)]

use std::io::{Read, Write};
use std::sync::mpsc::{channel, Receiver, Sender};

/// Read half of a byte pipe. Blocks until bytes arrive; EOF when the sender is dropped.
pub struct PipeReader {
    rx: Receiver<Vec<u8>>,
    buf: Vec<u8>,
    pos: usize,
}

/// Write half of a byte pipe.
pub struct PipeWriter {
    tx: Sender<Vec<u8>>,
}

pub fn pipe() -> (PipeWriter, PipeReader) {
    let (tx, rx) = channel();
    (
        PipeWriter { tx },
        PipeReader {
            rx,
            buf: Vec::new(),
            pos: 0,
        },
    )
}

impl Read for PipeReader {
    fn read(&mut self, out: &mut [u8]) -> std::io::Result<usize> {
        while self.pos >= self.buf.len() {
            match self.rx.recv() {
                Ok(b) => {
                    self.buf = b;
                    self.pos = 0;
                }
                Err(_) => return Ok(0),
            }
        }
        let n = out.len().min(self.buf.len() - self.pos);
        out[..n].copy_from_slice(&self.buf[self.pos..self.pos + n]);
        self.pos += n;
        Ok(n)
    }
}

impl Write for PipeWriter {
    fn write(&mut self, data: &[u8]) -> std::io::Result<usize> {
        self.tx
            .send(data.to_vec())
            .map_err(|_| std::io::Error::new(std::io::ErrorKind::BrokenPipe, "closed"))?;
        Ok(data.len())
    }
    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

/// Build a one-page PDF (US Letter). `content` is the page content stream.
/// Resources: font /F1 (Helvetica) and, when `with_image`, a 2x2 gray image /Im1.
pub fn make_pdf(content: &str, with_image: bool) -> Vec<u8> {
    let mut objs: Vec<Vec<u8>> = Vec::new();
    objs.push(b"<< /Type /Catalog /Pages 2 0 R >>".to_vec());
    objs.push(b"<< /Type /Pages /Kids [3 0 R] /Count 1 >>".to_vec());
    let xobj = if with_image { "/XObject << /Im1 6 0 R >>" } else { "" };
    objs.push(
        format!(
            "<< /Type /Page /Parent 2 0 R /MediaBox [0 0 612 792] /Contents 4 0 R \
             /Resources << /Font << /F1 5 0 R >> {xobj} >> >>"
        )
        .into_bytes(),
    );
    let mut stream = format!("<< /Length {} >>\nstream\n", content.len()).into_bytes();
    stream.extend_from_slice(content.as_bytes());
    stream.extend_from_slice(b"\nendstream");
    objs.push(stream);
    objs.push(b"<< /Type /Font /Subtype /Type1 /BaseFont /Helvetica /Encoding /WinAnsiEncoding >>".to_vec());
    if with_image {
        let mut img = b"<< /Type /XObject /Subtype /Image /Width 2 /Height 2 /ColorSpace /DeviceGray \
            /BitsPerComponent 8 /Length 4 >>\nstream\n"
            .to_vec();
        img.extend_from_slice(&[0, 255, 255, 0]);
        img.extend_from_slice(b"\nendstream");
        objs.push(img);
    }
    let mut out = b"%PDF-1.4\n".to_vec();
    let mut offsets = Vec::new();
    for (i, o) in objs.iter().enumerate() {
        offsets.push(out.len());
        out.extend_from_slice(format!("{} 0 obj\n", i + 1).as_bytes());
        out.extend_from_slice(o);
        out.extend_from_slice(b"\nendobj\n");
    }
    let xref = out.len();
    out.extend_from_slice(format!("xref\n0 {}\n0000000000 65535 f \n", objs.len() + 1).as_bytes());
    for off in offsets {
        out.extend_from_slice(format!("{off:010} 00000 n \n").as_bytes());
    }
    out.extend_from_slice(
        format!("trailer\n<< /Size {} /Root 1 0 R >>\nstartxref\n{xref}\n%%EOF\n", objs.len() + 1).as_bytes(),
    );
    out
}

/// `n` lines of 12 pt Helvetica text starting at y = 720, 16 pt apart.
pub fn text_lines(lines: &[&str]) -> String {
    let mut s = String::from("BT /F1 12 Tf 16 TL 72 720 Td\n");
    for l in lines {
        s.push_str(&format!("({l}) Tj T*\n"));
    }
    s.push_str("ET\n");
    s
}
