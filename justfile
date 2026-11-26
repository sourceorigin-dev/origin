# Validation formats Rust sources and runs every check CI runs.
default: validate

validate: fmt lint standard test doc doc-test audit unused

build:
    @cargo build --workspace --locked

run *ARGS:
    @cargo run --locked --package origin-src --bin origin -- {{ARGS}}

fmt:
    @cargo fmt --all

# CI checks formatting without modifying sources; local validation uses fmt.
fmt-check:
    @cargo fmt --all -- --check

# ARGS, such as `--package origin-src`, replace `--workspace`.
lint *ARGS='--workspace':
    @cargo clippy --locked --all-targets --all-features {{ARGS}} -- -D warnings

# ARGS, such as `--package origin-src`, replace `--workspace`.
test *ARGS='--workspace':
    @cargo nextest run --locked --all-features --no-tests=pass {{ARGS}}

# The API docs, private items included, as most of the code is internal.
doc:
    @cargo doc --locked --workspace --all-features --no-deps --document-private-items

# ARGS, such as `--package origin-src`, replace `--workspace`.
doc-test *ARGS='--workspace':
    @cargo test --locked --doc --all-features {{ARGS}}

audit:
    @cargo deny check

# Dependencies a crate declares but never names.
unused:
    @cargo machete

# The engineering standard's rules; `cargo xtask standard rules` lists them.
standard:
    @cargo xtask standard

# CI's quick checks, for only what changed since the merge base with a branch,
# `origin/main` by default.
[positional-arguments]
check-changed *ARGS:
    @cargo xtask check-changed "$@"

# Rewrite docs/openapi.json after changing the API. `just test` fails while
# it is stale.
openapi:
    @UPDATE_OPENAPI=1 cargo nextest run --locked --package origin-src --test http openapi

# Serve repositories over HTTP (see `origin serve --help`).
serve *ARGS:
    @cargo run --locked --package origin-src --bin origin -- serve {{ARGS}}

# Micro-benchmarks of the hot paths (hashing, storage, snapshots).
bench *ARGS:
    @cargo bench --locked --workspace -- {{ARGS}}

# Times origin against git with hyperfine, which must be installed, and
# rewrites docs/benchmarks.md.
# Pass `--tree PATH` to benchmark a copy of a real working tree.
bench-git *ARGS:
    @cargo xtask bench-git {{ARGS}}
