## What

<!-- One sentence: what does this change? -->

## Why

<!-- Why is this necessary? Link to issue if applicable. -->

## How

<!-- Brief description of approach. -->

## Checklist

- [ ] `cargo test --lib` passes (21 tests)
- [ ] `cargo clippy -- -D warnings` clean
- [ ] `no_std` protocol core still compiles: `cargo build --target thumbv7em-none-eabi --no-default-features --features alloc`
- [ ] Benchmarks included if performance claim is made
