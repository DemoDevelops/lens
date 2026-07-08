# Deployment

Guide to deploying the lens binary.

## Installation

Build and install to `~/.cargo/bin/lens`:

```bash
cargo build --release
cp target/release/lens ~/.cargo/bin/lens.tmp
mv ~/.cargo/bin/lens.tmp ~/.cargo/bin/lens
```

## Verification

After installation, verify with:

```bash
lens --version
lens map tests/fixtures/md/
```

See [home](./index.md) for more documentation.
