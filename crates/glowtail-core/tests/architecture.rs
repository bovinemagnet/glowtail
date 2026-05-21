use glowtail_core::filter::FilterExpr;
use glowtail_core::model::ViewportRequest;
use glowtail_core::viewport::Engine;

#[test]
fn viewport_returns_filtered_semantic_rows_without_ui_types() {
    let mut engine = Engine::default();
    for i in 0..1000 {
        let level = if i % 2 == 0 {
            glowtail_core::model::LogLevel::Error
        } else {
            glowtail_core::model::LogLevel::Info
        };
        let row = glowtail_core::model::LogRow {
            row_id: glowtail_core::model::RowId(i),
            source_id: glowtail_core::model::SourceId(1),
            byte_range: glowtail_core::model::ByteRange {
                start: i,
                end: i + 1,
            },
            timestamp: None,
            level: Some(level),
            raw: std::sync::Arc::from("line"),
            message: std::sync::Arc::from("line"),
            fields: glowtail_core::model::ParsedFields::default(),
        };
        engine.append_row(row);
    }

    engine
        .set_filter(FilterExpr::LevelEquals(
            glowtail_core::model::LogLevel::Error,
        ))
        .unwrap();
    let snapshot = engine.viewport(ViewportRequest {
        first_row: 20,
        row_count: 20,
    });

    assert_eq!(snapshot.rows.len(), 20);
    assert_eq!(snapshot.rows[0].row_id.0, 40);
}

/// Catches *direct* UI dependencies in `glowtail-core/Cargo.toml`. A
/// transitive UI dep (e.g. a future internal helper crate that re-exports
/// ratatui types) would slip past this check; if the dep graph ever grows
/// past the four UI crates, replace this with a `cargo metadata` walk.
#[test]
fn core_crate_has_no_ui_dependencies() {
    let manifest =
        std::fs::read_to_string(concat!(env!("CARGO_MANIFEST_DIR"), "/Cargo.toml")).unwrap();
    for forbidden in ["ratatui", "crossterm", "egui", "gpui", "wgpu"] {
        assert!(
            !manifest.contains(forbidden),
            "forbidden dependency found: {forbidden}"
        );
    }
}
