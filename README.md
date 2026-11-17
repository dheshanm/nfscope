# nfscope

A live, `top`-style terminal UI for an NFS server: which clients are moving the most data
right now, and what each one has open. It runs **on the NFS server itself** (as root);
no SSH round-trips, no 30-second sampling windows.

```
┌ nfscope · nfs-server ──────────────────────────────── READ server→client · WRITE client→server ┐
│Clients 31 (7 active)        READ    51.1 MiB/s                     ▁                           │
│Interval 2s  updated 10:47:28                  ··················▄█▄▄▄▄▅▆▆▆▇▇▆▆▅▅▄▄▄▄▅▆▆▆▇│
│                             WRITE   19.0 MiB/s                    █                            │
│TOTAL 70.1 MiB/s                               ··················█▄▁▁▁▁▁▂▂▂▂▃▃▃▃▃▃▃▂▂▂▂▁▁▁▁▁▂│
└──────────────────────────────────────────────────── ◀ last 1m16s · full height = 122.9 MiB/s ┘
┌ Clients (31) ──────────────────────────────────────────────────────────────────────────────────┐
│CLIENT            READ/s      WRITE/s    TOTAL/s ▼ TREND          READ TOT  WRITE TOT  CONN HOST │
│10.20.1.11    50.0 MiB/s   16.5 MiB/s   66.5 MiB/s           █▇    2.4 TiB    2.4 TiB    16 client…│
```

**Colours:** blue always means READ (server → client) and pink always means WRITE
(client → server). This applies to the header graphs, the table columns, the drill-down chart
and connection totals, and the `r`/`w` access flags in the open-files tree.

The header graph shows **server-wide** throughput: the sum over all clients, with READ on top
and WRITE below. Both graphs use one scale, so their heights compare directly. One column is
one sample, and the newest sample is at the right edge. The bottom border shows how much time
the graph covers and what full height means. Dots mark samples not collected yet, or idle periods.

* **READ** = server → client (the client is reading); **WRITE** = client → server.
* Rates come from per-socket TCP counters (`ss -tni`: `bytes_acked` / `bytes_received`)
  on port 2049. They are diffed every tick and summed per client IP, so nconnect is handled.
* Host names come from reverse DNS (on a background pool) plus the NFSv4 client name from
  `/proc/fs/nfsd/clients/*/info`.
* **Enter** on a client opens a drill-down with:
  * read/write history chart
  * NFSv4 client record (version, lease, callback, kernel)
  * per-connection TCP stats (RTT, retransmits, cwnd)
  * tree of the client's open state from `/proc/fs/nfsd/clients/<id>/states`, grouped
    by export mount point and then directory

## Keys

| Key | Action |
|---|---|
| `↑↓` `j k` `PgUp PgDn` `g G` | move |
| `Enter` | drill into client / fold tree node |
| `Esc` / `Backspace` | back (or clear filter) |
| `s` / `S` | cycle sort column / reverse |
| `/` | fuzzy filter (hosts in the list, file paths in the drill-down) |
| `p` | pause the display (sampling continues, so rates are right on resume) |
| `+` / `-` | sampling interval (1–60 s) |
| `← →` `e` `c` | drill-down: collapse/expand, expand all, collapse all |
| `d` | drill-down: hide files the client only holds a cached **delegation** for |
| `r` | drill-down: reload open state |
| `l` | log pane |
| `?` | help |
| `q` / `Ctrl-C` | quit |

**Mouse**

| Action | Effect |
|---|---|
| Click a column header | Sort by that column; click it again to reverse. |
| Click a row | Select the client. |
| Double-click a row | Open the client. |
| Click an export or directory in the tree | Expand or collapse it. |
| Click a file in the tree | Select the file. |
| Scroll wheel | Moves the selection in the list and scrolls the tree. |
| Right-click, or click `◀ back` | Go back to the list. |
| Click a key hint in the footer | Same as pressing that key. |

Run with `--no-mouse` to keep your terminal's own text selection. Most terminals also allow
Shift+drag to select text while mouse capture is on.

## Reading the open-files tree

* Each file shows its access (`r-`, `-w`, `rw`), its state types and its name.
  * `open` means a file the client has open right now.
  * `deleg` means a delegation: the server let the client cache the file, and it may
    no longer be open. Busy clients can hold thousands of these.
  * Press `d` to see only what is really open.
* The kernel reports only the **last two path components** (`dir/file`). The export
  comes from mapping the state's `superblock` device to `/proc/self/mountinfo`.
* NFSv3 clients have no per-client state in `/proc`, so their drill-down shows rates and
  connections only.

## Build & deploy

The server needs no Rust toolchain. Cross-compile a static binary on any machine.
`.cargo/config.toml` points the musl target at Rust's bundled `rust-lld`, so no extra
linker or Docker is needed.

```sh
rustup target add x86_64-unknown-linux-musl
cargo build --release --target x86_64-unknown-linux-musl
scp target/x86_64-unknown-linux-musl/release/nfscope root@SERVER:/usr/local/sbin/
ssh -t root@SERVER nfscope
```

```
nfscope [-i SECS] [--port 2049] [--no-dns] [--log /tmp/nfscope.log] [--fixture DIR]
```

Root is needed to read `/proc/fs/nfsd/clients/*`. Without it, rates still work from
`ss` and a banner explains what's missing. Logs go to `--log` (filter with `RUST_LOG`,
e.g. `RUST_LOG=debug`), and are also visible in the `l` pane.

## Development

```sh
cargo test                                  # parsers, rate maths, insta UI snapshots
cargo run -- --fixture tests/fixtures       # run the TUI on synthetic data (works on macOS)
cargo run --example peek -- tests/fixtures 10.20.1.11   # print one rendered frame
```

`tests/fixtures/` is synthetic: `example.org` hosts and made-up paths. To test against a
real server, capture into the gitignored `tests/fixtures/local/`:

```sh
mkdir -p tests/fixtures/local
ssh root@SERVER 'bash -s' < scripts/capture-fixture.sh | tar -C tests/fixtures/local -xf -
cargo test local_capture                    # parser sanity checks on the real capture
cargo run -- --fixture tests/fixtures/local
```

The capture script replaces every file and directory name in `states` with a pseudonym
**on the server**. It aborts if any original name would survive. Export paths (in
`mountinfo`) and client host names (in `hosts`) are kept, so the directory stays out of git.

Layout: `src/parse/` (ss / nfsd / mountinfo parsers), `src/model.rs` (rates, history),
`src/files.rs` (open-state aggregation), `src/source.rs` (live vs fixture), `src/workers.rs`
(sampler / states / DNS threads), `src/app.rs` (state + keys), `src/ui/` (ratatui views).
