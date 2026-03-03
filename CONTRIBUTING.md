# Contributing to chirp

Thanks for your interest. chirp is a focused protocol library — contributions that make it faster, more correct, or more portable are welcome.

## Ground rules

- **No `#[allow(dead_code)]`** — delete dead code instead
- **Zero warnings** after `cargo clippy`
- **All 21 unit tests must pass** — `cargo test --lib`
- **`no_std` protocol core must stay `no_std`** — `protocol/` modules must not import `std`
- **No `std::time::Instant` in protocol core** — timing uses `fugit`

## What's in scope

- Performance improvements to CC, FEC, or NACK
- Additional impairment scenarios in `scripts/netem_suite.sh`
- Real hardware benchmarks (drone modem, satellite link, LTE modem)
- `no_std` improvements or new embedded targets
- Bug fixes with a reproduction case

## What's out of scope

- New protocol features without benchmarks proving they help
- Additional CLI flags that don't improve correctness or performance
- Dependencies that break `no_std` compatibility

## Workflow

1. Fork and create a branch
2. Make your change
3. Run `cargo test --lib && cargo clippy -- -D warnings`
4. Submit a PR with a clear description of what changed and why

## Reporting bugs

Open an issue with:
- OS and Rust version
- Network conditions (loss %, RTT, link type)
- File size
- Full sender and receiver log output (set `RUST_LOG=debug`)
- MD5 of source file and received file if it's a corruption bug

## License

By contributing, you agree your changes are licensed under Apache 2.0.
