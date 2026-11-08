//! The sidecar has no network code: no banned crate may appear in the normal
//! (runtime) dependency tree. `deny.toml` covers build dependencies too.
use std::process::Command;

const BANNED: &[&str] = &["reqwest", "hyper", "curl", "hf-hub", "ureq", "native-tls", "openssl", "rustls", "tokio"];

#[test]
fn the_runtime_dependency_tree_has_no_network_crates() {
    let cargo = std::env::var("CARGO").unwrap_or_else(|_| "cargo".into());
    let out = Command::new(cargo)
        .args(["tree", "-e", "normal", "--prefix", "none", "--manifest-path"])
        .arg(concat!(env!("CARGO_MANIFEST_DIR"), "/Cargo.toml"))
        .output()
        .expect("run cargo tree");
    assert!(out.status.success(), "{}", String::from_utf8_lossy(&out.stderr));
    let tree = String::from_utf8(out.stdout).unwrap();
    let names: std::collections::HashSet<&str> = tree.lines().filter_map(|l| l.split_whitespace().next()).collect();
    for banned in BANNED {
        assert!(!names.contains(banned), "banned crate `{banned}` is in the runtime tree");
    }
    assert!(names.contains("oar-ocr") && names.contains("hayro"), "tree looks wrong:\n{tree}");
}
