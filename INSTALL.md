# Build & Run `pq` from source

## 1. Install Rust toolchain (one-time)

```bash
# macOS / Linux
curl --proto '=https' --tlsv1.2 -sSf https://sh.rustup.rs | sh
source "$HOME/.cargo/env"
```

Verify:
```bash
cargo --version    # should print 1.70 or newer
```

## 2. First build

```bash
cd /Users/hwang/Cursor/cdl_cursor_rules/pq

# Validate code without compiling DuckDB (~30 sec)
cargo check

# Full debug build — bundled DuckDB compiles from source on first run.
# Expect 5–10 min the first time, ~10 sec on subsequent builds.
cargo build

# Optimized release binary (~50 MB single binary)
cargo build --release
```

After `cargo build --release` the binary lives at `./target/release/pq`.

## 3. Run unit tests

```bash
cargo test       # parser tests (don't depend on DuckDB)
```

## 4. Generate a sample parquet & run a demo

```bash
# Need pandas + pyarrow once
pip install pandas pyarrow

python3 scripts/make_sample.py             # writes ./sample.parquet

# === Demo time ===
./target/release/pq sample.parquet
./target/release/pq sample.parquet '.email'
./target/release/pq sample.parquet '.email, .country where .country == "US"'
./target/release/pq schema sample.parquet
./target/release/pq stats  sample.parquet
./target/release/pq count  sample.parquet
./target/release/pq sample sample.parquet -n 3

# Pipe friendliness — auto-switches to NDJSON when stdout isn't a TTY
./target/release/pq sample.parquet '.email' | head -2
./target/release/pq sample.parquet -o csv > /tmp/out.csv && cat /tmp/out.csv

# Cloud (anyone with valid creds in env can try):
#   export GCS_HMAC_KEY_ID=... GCS_HMAC_SECRET=...      # for gs://
#   export AWS_ACCESS_KEY_ID=... AWS_SECRET_ACCESS_KEY=...  # for s3://
./target/release/pq gs://my-bucket/file.parquet '.country, count(*) group by .country'

# Debug the parser
./target/release/pq sample.parquet '.email where .country == "US"' --explain
# Prints: SELECT email FROM read_parquet('sample.parquet') WHERE country = "US"
```

## 5. Install locally as `pq`

```bash
# Install into ~/.cargo/bin (must be on PATH)
cargo install --path .

# Now `pq` works anywhere
pq sample.parquet '.email'
```

## 6. Cross-compile binaries for release

```bash
# macOS Intel
cargo build --release --target x86_64-apple-darwin
# macOS ARM
cargo build --release --target aarch64-apple-darwin
# Linux x86_64 (musl, fully static)
cargo build --release --target x86_64-unknown-linux-musl

# Each binary is ~50 MB (DuckDB bundled). Strip + zstd compress for releases:
strip target/release/pq
zstd -19 target/release/pq -o pq-v0.1.0-darwin-arm64.zst
```

## Troubleshooting

| Symptom | Cause | Fix |
|---|---|---|
| `cargo` not found | Rust not installed | Step 1 above |
| `linking with cc failed: cannot find -lduckdb_static` | macOS xcode CLI tools missing | `xcode-select --install` |
| 10 min build hangs at `Compiling duckdb-sys` | Normal first-build behavior | Wait. Subsequent builds are 10 sec. |
| `cargo build` runs OOM on Linux | DuckDB uses ~4 GB peak RAM during link | Add 8 GB swap or use `cargo build --jobs=2` |
| `pq gs://...` errors with `no host` | DuckDB httpfs not loaded | Run `pq schema gs://...` once — error message will tell you which extension to install |

## Project layout

```
pq/
├── Cargo.toml           # crate metadata + deps
├── README.md            # public marketing pitch
├── INSTALL.md           # this file
├── LICENSE              # MIT
├── scripts/
│   └── make_sample.py   # tiny test parquet generator
└── src/
    ├── main.rs          # CLI entrypoint (clap)
    ├── parser.rs        # DSL → DuckDB SQL compiler (with unit tests)
    └── output.rs        # table / json / ndjson / csv output
```
