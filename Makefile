fmt:
cargo fmt --all

clippy:
cargo clippy --all-targets --all-features -- -D warnings

test:
cargo test

run-sample:
cargo run -p glowtail-cli -- view samples/mixed.log
