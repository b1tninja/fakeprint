# Contributing

fakeprint is a lab printer. A change should keep Windows Add-printer and inbox capture behaving the same way every time, and should arrive with a test when it changes that behavior.

## Setup

Stable Rust with rustfmt and clippy (`rust-toolchain.toml` selects them).

```bash
cargo test -p fakeprint
pip install -e "./python[dev]"
pytest python/tests -m "not live"
```

`pytest python/tests -m live` needs a fakeprint process on `FAKEPRINT_BASE_URL` (default `http://127.0.0.1:3921`).

## Before a pull request

```bash
cargo fmt --all
cargo clippy -p fakeprint --all-targets -- -D warnings
cargo test -p fakeprint
pytest python/tests -m "not live"
```

CI runs the same fmt, clippy, and test gates on Ubuntu and Windows, plus the offline Python tests. See `.github/workflows/ci.yml`.

Do not commit captured jobs, `.env`, or tokens. `inbox/` stays empty except `.gitkeep`.

## License

MIT OR Apache-2.0. Contributions are dual-licensed the same way ([LICENSE-MIT](LICENSE-MIT), [LICENSE-APACHE](LICENSE-APACHE)).
