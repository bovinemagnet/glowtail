fmt:
	cargo fmt --all

clippy:
	cargo clippy --all-targets --all-features -- -D warnings

test:
	cargo test

run-sample:
	cargo run -p glowtail-cli -- view samples/mixed.log

run-gui:
	cargo run -p glowtail-gui -- samples/mixed.log

run-gpui:
	cargo run -p glowtail-gpui -- samples/mixed.log

run-iced:
	cargo run -p glowtail-iced -- samples/mixed.log

run-makepad:
	cargo run -p glowtail-makepad -- samples/mixed.log

bench-ui:
	cargo test --release -p glowtail-gui     --test render_perf -- --ignored --nocapture
	cargo test --release -p glowtail-iced    --test render_perf -- --ignored --nocapture
	cargo test --release -p glowtail-gpui    --test render_perf -- --ignored --nocapture
	cargo test --release -p glowtail-makepad --test render_perf -- --ignored --nocapture
