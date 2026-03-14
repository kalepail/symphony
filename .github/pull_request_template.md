#### Context

<!-- Why is this change needed? Length <= 240 chars -->

#### TL;DR

*<!-- A short description of what we are changing. Use simple language. Assume reader is not familiar with this code. Length <= 120 chars -->*

#### Summary

- <!-- Details of the changes in bullet points -->
- <!-- Keep them high level -->
- <!-- Each item <= 120 chars -->

#### Alternatives

- <!-- What alternatives have been considered? Why not? -->

#### Test Plan

- [ ] `cargo fmt --manifest-path rust-todoist/Cargo.toml --check && cargo clippy --manifest-path rust-todoist/Cargo.toml --all-targets --all-features -- -D warnings && cargo test --manifest-path rust-todoist/Cargo.toml`
- [ ] `make -C elixir all` (if `elixir/` changed)
- [ ] <!-- Additional targeted checks (list below) -->
