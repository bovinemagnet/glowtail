# Repository Guidelines

## Project Structure & Module Organization

This is a Rust workspace for `glowtail`, a multi-log viewer with a UI-neutral core.

- `crates/glowtail-core/` contains parsing, indexing, filtering, tailing, viewport logic, and shared domain models. Keep it free of terminal, GPU, or UI framework dependencies.
- `crates/glowtail-tui/` contains the Ratatui/crossterm terminal UI and maps core `RowPresentation` data into terminal styling.
- `crates/glowtail-cli/` contains command-line argument parsing and executable entry points.
- `crates/glowtail-core/tests/` holds integration and architecture tests.
- `samples/` provides example log files for manual runs.
- `docs/prd/` contains product requirements and planning notes.

## Build, Test, and Development Commands

- `make fmt` or `cargo fmt --all`: format the workspace.
- `make clippy` or `cargo clippy --all-targets --all-features -- -D warnings`: run lint checks with warnings treated as errors.
- `make test` or `cargo test`: run all unit and integration tests.
- `make run-sample`: run the CLI against `samples/mixed.log`.
- `cargo run -p glowtail-cli -- view samples/plain.log`: manually view a sample file.
- `cargo run -p glowtail-cli -- tail samples/mixed.log --level warn --no-follow --from-start`: test tail/filter behavior from sample input.

## Coding Style & Naming Conventions

Use Rust 2024 edition and standard `rustfmt` output. Keep core types and logic in `glowtail-core`, UI rendering in `glowtail-tui`, and argument parsing or process wiring in `glowtail-cli`. Use `snake_case` for functions, modules, and tests; `PascalCase` for structs, enums, and traits. Keep comments short and only where they clarify non-obvious behavior.

## Testing Guidelines

Use Rust's built-in test framework. Add focused unit tests next to implementation code and integration or boundary tests under `crates/<crate>/tests/`. Name tests by behavior, for example `viewport_returns_filtered_semantic_rows_without_ui_types`. Preserve the architecture rule that `glowtail-core` must not depend on UI libraries such as `ratatui`, `crossterm`, `egui`, `gpui`, or `wgpu`.

## Commit & Pull Request Guidelines

Recent history uses concise, imperative commit subjects, sometimes with a PR reference, for example `Scaffold Phase-1 Rust architecture for glowtail with UI-neutral core engine and terminal MVP (#1)`. Keep commits focused and describe the user-visible or architectural change.

Pull requests should include a short summary, tests run, linked issues when relevant, and terminal screenshots or recordings for TUI changes. Call out any changes to crate boundaries, dependencies, or CLI behavior.

## Agent-Specific Instructions

Before editing, inspect the relevant crate and avoid unrelated refactors. Do not introduce UI dependencies into `glowtail-core`. Run `make fmt`, `make clippy`, and `make test` before handing off when practical.
