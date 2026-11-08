//! Infield OCR add-on sidecar.
//!
//! The binary speaks the framed stdio protocol of `infield-ocr-protocol`. The
//! library holds the pieces so they can be tested without models: the protocol
//! loop ([`server`]), the analyzer seam and its stub ([`pipeline`]), the PDF
//! text-layer check ([`text_layer`]) and the table grid derivation ([`table`]).
//!
//! Logs go to stderr and never contain document text.

pub mod pipeline;
pub mod platform;
pub mod server;
pub mod table;
pub mod text_layer;

/// Crate version, reported by `--version` and in `HelloOk`.
pub const VERSION: &str = env!("CARGO_PKG_VERSION");

/// ONNX Runtime version statically linked by `ort-sys` rc.13 (its `dist.tsv`).
/// Update together with the `oar-ocr` pin.
pub const ORT_VERSION: &str = "1.28.0";
