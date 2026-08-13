# grab — the EDOS package manager

`grab` fetches signed software from `https://edos.edgl.dev/pkg` and installs it.
It exists for three reasons, in order of how much they matter:

1. The amount of software in this tree is growing past what belongs on a boot
   image. Programs need to be classifiable as *shipped with EDOS* or *installed
   afterwards*, and that classification needs a mechanism behind it, not a
   convention.
2. It is the first thing that drives the TCP/HTTP path for a real purpose,
   against a real server on the public internet, rather than a test program
   pointed at a loopback socket.
3. Nothing in EDOS can currently reach anything published on the internet.
   `wget` and `http` refuse `https://` for want of TLS, and `tar` reads only
   uncompressed archives. Closing that is worth more than the package manager
   itself.

The name is a verb because the common case has no subcommand: `grab snake`
installs snake.

## Shape

Five pieces, each independently useful:

| Piece | What it is |
| --- | --- |
| `programs/edos_http/` | HTTP/1.1 client with TLS. Linked by `wget`, `http`, `grab`, `edos-grab`. |
| `programs/gzip/`, `tar -z` | inflate/deflate, exposed as programs and as a `tar` flag. |
| `tools/grab-repo/` | Host tool. Builds the index, signs it, publishes to the server. |
| `programs/grab/` | The CLI, and a lib crate the GUI links. |
| `programs/edos-grab/` | The GUI. |

TLS and HTTP go in their own crate rather than into `edos_lib`, because every
program in the tree links `edos_lib` and rustls has no business inside `true`
and `yes`.

`grab` is a lib plus a bin, and `edos-grab` depends on the lib by path. The GUI
does not shell out to the CLI: it needs progress and structured errors, and a
subprocess gives it neither.

## Transport

### TLS

rustls 0.23 with the RustCrypto provider and a bundled root store. Measured on
this target: a binary carrying rustls, `rustls-rustcrypto`, all of
`webpki-roots`, flate2 and std is **712 KB stripped** (966 KB release). Against
a 16 MB `filesystem/bin` under a 64 MiB live-root floor, TLS is free.

Three things are load-bearing in the dependency declaration, each of which fails
the build if got wrong:

```toml
rustls = { version = "0.23", default-features = false, features = ["std", "tls12"] }
rustls-rustcrypto = { version = "0.0.2-alpha" }   # NOT default-features = false
webpki-roots = "1"
getrandom = { version = "0.2", features = ["custom"] }
```

- **rustls must lose its default features.** The default pulls `aws_lc_rs`,
  which needs a C toolchain and NASM. Dropping to `std` + `tls12` leaves a pure
  Rust build.
- **`rustls-rustcrypto` must keep its default features**, which is the opposite
  of the reflex every other crypto crate in this tree wants.
  `default-features = false` strips its `alloc` feature and it answers with
  `compile_error!("Rustls currently does not support alloc-less environments")`.
- **`getrandom` 0.2 needs the `custom` feature.** It arrives via `rand_core`
  0.6 under `elliptic-curve` and `rsa`, and on an unrecognised target its
  unmodified form is a `compile_error!`. The `custom` feature replaces that with
  a registration hook:

```rust
fn edos_rng(buf: &mut [u8]) -> Result<(), getrandom::Error> {
    edos_lib::getrandom(buf);
    Ok(())
}
getrandom::register_custom_getrandom!(edos_rng);
```

which routes it to `SYS_GETRANDOM`, the same source sshd already uses.

TLS 1.2 is kept alongside 1.3. It costs a feature flag and removes a class of
"works against nginx, fails against something else" problem later.

### The clock

Certificate validity is checked against the system clock, which makes a wrong
clock present as a certificate error — the single most confusing failure this
design can produce. So `grab` reads `SYS_CLOCK_GETTIME` before its first
connection, and if the clock reads earlier than 2020, it performs an SNTP sync
and retries once. If the clock is still implausible, or a handshake fails on
validity period, the error names the real cause:

```
grab: certificate rejected: system clock reads 1970-01-01T00:00:12Z
```

This wants `sntp`'s query extracted into `edos_lib::time` so both programs use
one implementation rather than two.

### HTTP

HTTP/1.1, `Host`, `Connection: close`. Three deliberate choices:

- **`Accept-Encoding: identity`.** Package files are gzip *content*; asking for
  gzip *transfer* encoding on top buys nothing and adds a decode path that only
  fires against some servers, which is how a bug hides.
- **Chunked transfer decoding is implemented anyway**, because a CDN will use it
  whether or not the origin does.
- **A body-size cap**, checked against the index's declared size before the
  download starts. Without it a hostile or broken server can exhaust guest
  memory, and this program exists to talk to the internet.

Redirects are followed to a depth of 5, including `http://` → `https://`, with
the certificate re-verified at each hop.

Known limitation, inherited: `sys_connect` blocks until the handshake completes
regardless of `O_NONBLOCK`, so `grab` against an unreachable host waits out the
kernel's TCP timeout with no way to bound it. That is the existing non-blocking
connect gap, not a new one.

## Compression

flate2 1.1.9 on `miniz_oxide` 0.8.9 is already in `programs/Cargo.lock` and
already ships, pulled in by resvg for the PNGs an SVG can embed. So inflate is
wiring, not implementation:

- `gzip` / `gunzip` as programs, since a system that can download a `.gz` and
  not open it is worse than one that can do neither.
- `tar -z`, so `tar -xzf` behaves the way every user expects.

`grab` uses the library directly rather than shelling out to either.

## The repository

Static files. No dynamic API, nothing to run server-side, and the whole thing is
`rsync`-able and mirror-able.

```
https://edos.edgl.dev/pkg/index                    # the catalogue
https://edos.edgl.dev/pkg/index.sig                # ed25519 signature, 64 raw bytes
https://edos.edgl.dev/pkg/p/<name>-<version>.tar.gz
https://edos.edgl.dev/pkg/icons/<name>.svg
```

### The index

RFC822-style stanzas, one blank line between them. Not JSON: it needs no
dependency in the guest, it is greppable from a shell that already has `grep`,
and it reads like the rest of `/etc`.

```
Repo: edos
Serial: 7
Generated: 2026-08-13T18:00:00Z

Package: edos-edit
Version: 0.1.0
Summary: Graphical text editor with a file tree, tabs and syntax highlighting
Category: editors
Size: 812345
SHA256: 9f86d081884c7d659a2feaa0c55ad015a3bf4f1b2b0b822cd15d6c15b0f00a08
File: p/edos-edit-0.1.0.tar.gz
Icon: icons/edos-edit.svg
Installs: bin/edos-edit
```

`Serial` is monotonic and is an anti-rollback guard: `grab` refuses an index
whose serial is lower than the one it already holds. A signed *old* index is
otherwise a perfectly valid way to hide the existence of a fixed version, and
the check costs one comparison.

The website's catalogue page parses this same file at build time, so there is
one artifact rather than a machine format and a human format that can disagree.

### What authenticates a package

An ed25519 detached signature over the exact bytes of `index`. The index carries
`SHA256` and `Size` for every package, so package integrity chains from the one
signature. The public key is a 32-byte constant compiled into `grab`.

The reason this is not "TLS is enough": **Cloudflare terminates TLS for this
domain.** The certificate a guest validates is Cloudflare's, not ours. Under TLS
alone, the CDN — and anyone who reaches the origin — is trusted to decide what
binaries the OS runs. Signing makes the transport untrusted, which is also what
makes mirroring possible later.

The private key lives at `~/.config/edos/grab-repo.key`, mode 0600, on the
publishing machine, and is never committed. Rotation means rebuilding `grab`,
which is an honest limit to accept rather than engineer around at this size.

TLS is still used, because signing does not give confidentiality and there is no
reason to publish what each machine installs.

### Unsigned installs

`grab install --allow-unsigned ./edos-edit-0.1.0.tar.gz` installs a local file
with no signature check, printing what it is about to do. It exists because
testing a package before publishing it is otherwise impossible. It is the only
path that skips verification, it is never reachable without the flag, and it
never applies to a URL.

## Packages

A package is a gzipped tar of paths relative to `/`:

```
bin/edos-edit
share/icons/edos-edit.svg
```

Two rules, both enforced by `grab` rather than by trust in the archive:

- **Path whitelist.** Only `bin/`, `lib/`, `share/` and `opt/` may be written.
  Absolute paths, `..` components and everything else are refused. `etc/` is
  excluded deliberately: settings belong to the machine, not to a package.
- **No overwriting what a package does not own.** `grab` refuses to write a path
  that exists and is not recorded in some installed package's file list. This is
  what makes the shipped and packaged namespaces disjoint mechanically, so a
  package cannot replace `/bin/sh` by being named carefully.

Installed state, all of it plain text:

```
/var/lib/grab/db/<name>/meta      the package's index stanza, verbatim
/var/lib/grab/db/<name>/files     one installed path per line
/var/cache/grab/index             last verified index
/var/cache/grab/index.sig
/etc/grab/repo                    repo base URL, via edos_lib::config
```

`files` is what makes removal exact: `grab remove` deletes what it recorded and
nothing else.

There is **no dependency resolution**, and this is not a simplification to
revisit. EDOS has no dynamic linker — PT_INTERP is unimplemented — so every
binary is statically linked and a shared-library dependency cannot exist here.
There is nothing to resolve.

Versions are compared as opaque strings: `upgrade` acts when the repo's version
differs from the installed one. Ordering semantics would be a guess at what a
version *means*, and the `Serial` guard already covers whole-index rollback.

## The CLI

```
grab update                       fetch and verify the index
grab <name>                       install (the bare form)
grab install <name>...
grab remove <name>...
grab upgrade [<name>...]
grab search <term>
grab show <name>
grab list [--installed]
```

## The GUI

`edos-grab`: a search field, a scrolling list of packages showing SVG icon, name,
version and one-line summary, a detail pane, and Install/Remove/Update with a
progress line.

It is the fourth consumer of `edos_render`'s widgets, and `edos-edit` is the
model to copy: geometry as pure functions that both drawing and hit-testing
call, layout rebuilt every frame as derived state, `text_width` for measuring
anything outside a monospaced pane. Icons render through the `svg` feature
`edos_render` already carries.

The three input rules that have each cost a bug apply unchanged: window key
events carry `pc_keyboard` KeyCodes and never Character events; a modifier press
must not return early from the event loop; and the key that opens a field must
not also be typed into it.

## Server side

nginx, inside the existing `edos.edgl.dev` block:

```nginx
location /pkg/ {
    alias /srv/edos-pkg/;
    autoindex off;
}

# Version-stamped package files are immutable and cache well. The index is not.
location = /pkg/index     { alias /srv/edos-pkg/index;     add_header Cache-Control "no-cache"; }
location = /pkg/index.sig { alias /srv/edos-pkg/index.sig; add_header Cache-Control "no-cache"; }
```

Package files are served from `/srv/edos-pkg`, outside the site's git checkout,
because they are binaries and binaries do not belong in that repo. `tools/grab-repo
publish` writes there.

Two things about the existing config matter. The media regex already matches
`gz`, so `.tar.gz` inherits `expires 1d` — correct, because package filenames
carry their version and are therefore immutable. The index has no extension and
would inherit no expiry at all, which is why it gets an explicit `no-cache`:
a cached index is an invisible rollback.

The human-facing catalogue lives at `/software/`, not `/pkg/`, since `/pkg/` is
an nginx alias and cannot also be an Astro page. It renders from the same index
file at build time.

## Shipped versus packaged

Additive: the ISO keeps everything it ships today, and the repo carries new
software. Moving binaries out saves nothing at present — `filesystem/bin` is
16 MB and `live-root.img` has a 64 MiB floor, so the floor binds, not the
binaries — and a live session with no network has to stay usable.

**`edos-edit` is the exception and the first package**, which makes it the test
that the whole path works end to end. `edos-vi` stays on the image, so the
no-network case still has an editor.

The mechanism has to account for `--artifact-dir=../filesystem/bin/` copying
*every* workspace binary into the image. Excluding one is therefore a post-build
move, not a build-list change: a `PACKAGED` list in `programs/Makefile` moves
those binaries to a staging directory after `build`, and `tools/grab-repo` packs
from there.

Metadata lives in a `pkg.toml` beside the program's `Cargo.toml`:

```toml
summary  = "Graphical text editor with a file tree, tabs and syntax highlighting"
category = "editors"
icon     = "assets/edos-edit.svg"
shipped  = false          # false = packaged, absent = shipped
```

so the shipped/packaged classification is one field, next to the thing it
classifies.

## Order of work

Each step is independently useful and independently committable.

1. **`edos_http`** — HTTP/1.1 plus TLS; `wget` and `http` gain `https://`.
   Done when a real fetch from `https://edos.edgl.dev` succeeds inside the guest.
2. **gzip/gunzip and `tar -z`.**
3. **Repo format, `tools/grab-repo`, signing, nginx, publish.** Done when the
   guest can `wget` a verified index.
4. **`grab` CLI.**
5. **`edos-grab` GUI.**
6. **`edos-edit` leaves the image and becomes the first package**, plus the
   `/software/` page on the website.

## Risks

- **The probe proves linking, not a handshake.** rustls, the RustCrypto provider
  and webpki-roots build and link for `x86_64-unknown-edos`; nothing yet proves
  a completed TLS session against Cloudflare. Step 1 is where that is settled,
  and it is the step most likely to surface work not listed here.
- **Cloudflare may not like an unusual client.** A bare Rust HTTP client with no
  JavaScript can attract bot protection, which would present as an HTML
  challenge page where a tarball was expected. Mitigations in order: a plain
  `User-Agent: grab/0.1 (EDOS)`, then a cache/security rule for `/pkg/*`, then
  moving the repo to a DNS-only subdomain that bypasses the proxy entirely.
- **`rustls-rustcrypto` is 0.0.2-alpha.** If it rots, the fallbacks are a
  hand-written provider over p256/p384/rsa behind rustls's `custom-provider`
  feature, or a hand-rolled TLS 1.3 client. The second is expensive mostly
  because of X.509, not the handshake.
- **Baked-in CA roots expire.** `webpki-roots` is a compiled-in snapshot, so root
  rotation eventually needs a rebuild of everything linking it.
- **No timeout on an unreachable repo**, per the blocking-connect gap above.

## What it does not do

- No dependency resolution (see above — there is nothing to resolve).
- No partial or resumable downloads; a failed transfer restarts.
- No multiple repositories or mirrors. The format allows it; the client does not.
- No per-file conflict resolution: a package that would overwrite a foreign path
  is refused outright, not merged.
- No self-update. `grab` is shipped with the image and updated with it.
