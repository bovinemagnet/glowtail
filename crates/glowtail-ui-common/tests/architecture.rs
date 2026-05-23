//! Mirror of `glowtail-core`'s architecture guard. `glowtail-ui-common` is
//! consumed by every UI front-end, so it must stay UI-framework-free —
//! otherwise the shared crate would silently couple, say, the CLI to
//! `gpui` and defeat the whole point of having a UI-neutral layer.

#[test]
fn ui_common_crate_has_no_ui_dependencies() {
    let manifest =
        std::fs::read_to_string(concat!(env!("CARGO_MANIFEST_DIR"), "/Cargo.toml")).unwrap();
    for forbidden in ["ratatui", "crossterm", "egui", "eframe", "gpui", "wgpu"] {
        assert!(
            !manifest.contains(forbidden),
            "forbidden dependency found: {forbidden}"
        );
    }
}
