# Contributing

Issues and pull requests are welcome. nfscope only reads kernel interfaces, so most of it can be
developed and tested on any OS against fixture data; only the netlink code needs Linux.

## Build and test

```sh
cargo test                                  # parsers, rate maths, insta UI snapshots
cargo clippy --all-targets
cargo run -- --fixture tests/fixtures       # the TUI on synthetic data (works on macOS)
```

On macOS the Linux-only code (`src/sockdiag.rs`, the netlink socket) is not compiled. To run its
tests, including the real kernel round trips, use a Linux container:

```sh
docker run --rm -v "$PWD":/work -w /work -e CARGO_TARGET_DIR=/tmp/target rust:1-slim cargo test
```

UI changes show up as insta snapshot diffs: review them with `cargo insta review` (or accept
the `.snap.new` files once you have checked them).

## Looking at a frame without a terminal

```sh
cargo run --example peek -- tests/fixtures 10.20.1.11   # print one rendered frame
KEYS=i cargo run --example peek -- tests/fixtures       # …after pressing keys (in-flight panel)
scripts/screenshots.sh                                  # regenerate docs/*.png and docs/demo.gif (needs Chrome, gifski)
```

`peek` also takes `TICKS`, `COLS`, `ROWS`, `NAME` and `FORMAT=html`.

## Testing against a real server

`tests/fixtures/` is synthetic: `example.org` hosts and made-up paths. To test with real data,
capture into the gitignored `tests/fixtures/local/`:

```sh
mkdir -p tests/fixtures/local
ssh root@SERVER 'bash -s' < scripts/capture-fixture.sh | tar -C tests/fixtures/local -xf -
cargo test local_capture                    # parser sanity checks on the real capture
cargo run -- --fixture tests/fixtures/local
```

The capture script replaces every file and directory name in `states` with a pseudonym **on the
server** and aborts if any original name would survive. Export paths and client host names are
kept, which is why the directory stays out of git. With `NFSCOPE=/path/to/nfscope` set on the
server it also saves a few in-flight snapshots (client addresses, XIDs and op numbers only).

Two hidden flags help when checking a kernel:

- `nfscope --dump-inflight` prints the RPCs nfsd is working on now (fixture format).
- `nfscope --dump-sockets` prints the NFS connections as nfscope reads them, in `ss -tniH`
  format, to compare with `ss` or save as `ss/001.txt` in a fixture.

## Deploying a test build

The server needs no Rust toolchain. `.cargo/config.toml` links the musl target with Rust's
bundled `rust-lld`, so cross-compiling needs no extra linker or Docker:

```sh
rustup target add x86_64-unknown-linux-musl   # or aarch64-unknown-linux-musl
cargo build --release --target x86_64-unknown-linux-musl
scp target/x86_64-unknown-linux-musl/release/nfscope root@SERVER:/tmp/nfscope-test
```

## Releasing

`.github/workflows/release.yml` builds static `x86_64` and `aarch64` musl binaries, runs the
tests, and publishes a GitHub release with `SHA256SUMS` when a `v*` tag is pushed:

```sh
# bump `version` in Cargo.toml, then
cargo build && git commit -am "release v0.1.8"
git tag v0.1.8 && git push origin master v0.1.8
```

The tag must match the `Cargo.toml` version. "Run workflow" on the Release action does a dry
run: it builds and tests, and attaches the binaries to the run instead of publishing.

## Code layout

| Path | What |
|---|---|
| `src/parse/` | parsers: `ss` text (fixtures), nfsd `info`/`states`, mountinfo, `/proc/net/rpc/nfsd`, in-flight RPCs |
| `src/sockdiag.rs` | TCP stats over sock_diag netlink (Linux) |
| `src/netlink.rs` | minimal generic-netlink client for the nfsd family |
| `src/inflight.rs` | in-flight averages |
| `src/model.rs` | rates, history, TCP flag thresholds |
| `src/files.rs` | open-state aggregation and the full-path search |
| `src/source.rs` | live system vs fixture directory |
| `src/workers.rs` | sampler, states, find and DNS threads |
| `src/app.rs` | state and key handling |
| `src/ui/` | ratatui views |
