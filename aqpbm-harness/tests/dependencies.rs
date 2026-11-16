//! The dependency rule: the harness stays engine-neutral and light.

/// `cargo tree -p aqpbm-harness -e normal` names no sketch library
/// (`asap_sketchlib`), no `polars`, no `sketch-bench`, no `clap` and no
/// ASAPPlanner crate (`asap-`).
#[cfg(target_os = "linux")]
#[test]
fn harness_depends_on_no_sketch_library_polars_clap_or_planner() {
    let workspace = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .unwrap();
    let output = std::process::Command::new(env!("CARGO"))
        .args([
            "tree",
            "-p",
            "aqpbm-harness",
            "-e",
            "normal",
            "--prefix",
            "none",
        ])
        .current_dir(workspace)
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "cargo tree failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    let tree = String::from_utf8(output.stdout).unwrap();
    assert!(tree.contains("aqpbm-harness"), "{tree}");
    for forbidden in ["asap_sketchlib", "polars", "sketch-bench", "clap", "asap-"] {
        assert!(!tree.contains(forbidden), "{forbidden} in:\n{tree}");
    }
}
