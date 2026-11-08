//! `infield-ocr`: the OCR add-on sidecar. One process serves one job over
//! stdin/stdout using the framed protocol of `infield-ocr-protocol`.

use infield_ocr::pipeline::OarAnalyzer;
use infield_ocr::{platform, server, VERSION};

fn main() {
    // First: a panic message must never reach the logs (it may hold document text).
    platform::install_panic_hook();
    // Before anything else: later runtime loads see only the app dir and System32.
    platform::harden_dll_search();

    if std::env::args().skip(1).any(|a| a == "--version" || a == "-V") {
        println!("infield-ocr {VERSION}");
        return;
    }

    // Test hook, debug builds only: panics with text that must not appear in the logs.
    #[cfg(debug_assertions)]
    if std::env::args().skip(1).any(|a| a == "--panic-test") {
        panic!("SECRET-DOCUMENT-TEXT");
    }

    let mut analyzer = OarAnalyzer;
    let code = match server::serve(std::io::stdin(), std::io::stdout().lock(), &mut analyzer) {
        Ok(()) => 0,
        Err(e) => {
            // Protocol-level failure; never contains document text.
            eprintln!("infield-ocr: {e}");
            1
        }
    };
    std::process::exit(code);
}
