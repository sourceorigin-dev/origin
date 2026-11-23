# origin

Version control for repositories that many people, agents, CI runners and
review bots read and write at the same time, written in Rust.
`docs/design.md` describes the model; this repository holds its prototype.

## Layout

- `crates/origin-shared`: the object model, wire formats, and API contracts
  shared by every component.
- `crates/origin-platform`: everything that differs between platforms
  (durable writes, file locks, copy-on-write clones, positioned reads,
  refusing network file systems), behind traits. It is the only place whose
  code differs by target OS. `std::os::unix`, which every supported platform
  has, may be used anywhere.
- `crates/origin-client`: the SDK, a typed client for origin servers. Each
  endpoint is one line of its `endpoints!` macro, over the `origin-shared`
  types; no reqwest type is exposed.
- `apps/origin-src`: the `origin` binary, both the command line and the
  server. `OriginService` (`service.rs`) is the one API over a repository:
  the HTTP routes (`http.rs`) and the command line's commands that the API
  offers only translate into its calls, so neither duplicates the other.
  `sync.rs` shares records with a remote (`remote.rs`), which answers the
  same calls over the SDK or, for a local path, in-process.
- `tools/xtask`: development tasks, run with `cargo xtask`, such as timing
  origin against git (`just bench-git`).

## Testing & Benchmarks

Performance is a primary goal, so benchmarks matter as much as tests. A change
to a hot path (hashing, storage, snapshots, transfer) should come with a
benchmark, and its numbers in the pull request.

- `just bench`: the divan micro-benchmarks (encoding, store writes,
  snapshots, input digests and promotion, landing, reading events, and the
  server's request latency and long-poll wakeups).
- `just bench-git`: times origin against git on a 20,000-file tree with
  hyperfine, and rewrites `docs/benchmarks.md`.

End-to-end tests run the `origin` binary with `ORIGIN_CONFIG_DIR` set to a
temporary directory, which holds the user's `config.toml` and node key.
The HTTP tests drive every SDK method against a real server. One fails
while `docs/openapi.json` is stale: `just openapi` rewrites it.

## Validation

`just check-changed` runs CI's quick checks on what a change reaches;
`just validate` runs them all. `just standard` checks the engineering
standard's rules (the `sourceorigin-standard` crate's README lists them);
the `standard` skill says how to fix a violation, and `standard.toml` holds
origin's deviations.

## Issues & Pull Requests

Issues live in GitHub issues. Pull requests follow
`.github/pull_request_template.md`, and `standard / pr-checks` checks their
titles and descriptions. `docs/design.md` is the documentation page.
