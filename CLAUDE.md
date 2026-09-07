# rfx-voiced — working notes

The voice node for InteropHQ. Connects out to each FXServer's built-in Mumble
server as a client, vocodes what it hears, and streams it to game clients and
dispatch consoles.

Deployed on **Pelican**, at `nyc-voice.interophq.com` behind a Cloudflare
Tunnel.

## Shipping an update

```
git tag -a v0.1.4 -m "what changed" && git push origin v0.1.4
```

Then, in Pelican: **Settings → Reinstall Server**.

**Reinstall, not restart.** The startup command is `./rfx-voiced` — it runs the
binary already on disk and downloads nothing. Only the install script fetches,
and it pulls the latest GitHub release. A restart after tagging appears to
work and changes nothing, which is a confusing half-hour if you forget.

The node prints its version as the first line of every run:

```
rfx-voiced v0.1.3
```

That is how you confirm a deploy took. `./rfx-voiced --version` says the same.
Before this existed the only way to tell was to infer it from behaviour, and a
restart that changed nothing looked identical to a successful update.

Reinstall is safe here: the install script writes only the binary and never
touches `servers.json`, the cache, or anything else in the volume.

**Push main first, wait for CI, then tag.** Pushing both at once starts CI and
Release on the same commit at the same time, which builds the image twice and -
worse - tags a commit before anything has validated it. That is exactly how
v0.1.0 shipped broken.

`git push origin main` alone ships nothing. Tags are what CI releases from,
and the release job checks out **the tag** — so a fix on `main` that isn't
tagged rebuilds nothing. A tag pointing at a broken commit rebuilds the broken
commit, which is how v0.1.0 failed.

## Before pushing

CI runs `cargo fmt --check`, `cargo clippy --all-targets -- -D warnings`,
tests, a release build and a binary smoke test.

**This machine's clippy is older than the runner's stable**, so a clean local
clippy does not mean a green CI. Reproduce against the real toolchain:

```
git archive HEAD | tar -x -C /tmp/ci
docker run --rm -v /tmp/ci:/src -w /src rust:1-bookworm bash -c \
  'apt-get update -qq && apt-get install -y -qq cmake && \
   rustup component add rustfmt clippy && \
   cargo fmt --check && cargo clippy --all-targets -- -D warnings && cargo test --all'
```

`cargo build --release` cannot run on the Windows dev box — no cmake, and
`opusic-sys` builds libopus from source. `cargo check` and `cargo test` work
off a cached artefact and will mislead you.

## Things already learned the hard way

- **Editing files from Python:** this tree is CRLF. Multi-line match strings
  written with `\n` silently fail to match while single-line ones succeed.
  Read with `newline=''` and translate `\n` to the file's ending.
- **The repo is private.** The Pelican egg's install needs `USERNAME` and
  `ACCESS_TOKEN` (a PAT with `repo`), and release assets must be fetched
  through the assets API — `browser_download_url` is unauthenticated and 404s.
- **Never echo a URL with credentials spliced in.** An earlier install script
  printed the PAT to the panel log in full.
- The install script prints `=== rfx-voiced install script rN ===` first, so a
  log tells you immediately whether an egg edit actually took. Pelican keeps
  the install script on the **egg**; a server attached to a duplicated egg runs
  the old one and reports success.
- The egg carries a stable `uuid`, so re-importing updates in place rather than
  creating a duplicate.

## Deployment shape

- Two allocations: primary is the control API, second is the audio stream. Only
  the stream must be reachable from players.
- Behind Cloudflare Tunnel with path rules `^/control` and `^/stream`.
  cloudflared does **not** strip the matched prefix, which is why control takes
  the last path segment as its message name.
- The stream pings every 25s. Cloudflare closes idle WebSockets at ~100s and a
  radio channel is silent most of the time.
