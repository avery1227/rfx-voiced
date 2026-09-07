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

On Windows this needs care — Git Bash rewrites paths and mangles the `-c`
string. Use a Windows path for the mount and put the commands in a FILE:

```
CI=/c/Users/you/ci
rm -rf "$CI" && mkdir -p "$CI"
git archive HEAD | tar -x -C "$CI"
cp src/*.rs "$CI/src/"        # uncommitted work
printf '%s\n' 'set -e' \
  'rustup component add rustfmt clippy >/dev/null 2>&1' \
  'cargo fmt --check' \
  'cargo clippy --all-targets -- -D warnings' \
  'cargo test --all' \
  'cargo build --release' > "$CI/ci.sh"
MSYS_NO_PATHCONV=1 docker run --rm -v "$CI:/src" -w /src rust:1-bookworm bash /src/ci.sh
```

`-w /src` with path conversion on becomes `C:/Program Files/Git/src`, and
`bash -c '... --check ...'` loses its flags. Both fail in ways that look like
the code is wrong.

This is not theoretical: the run that caught `chunks_exact_to_as_chunks` and
`sort_by` had a completely clean local clippy.

`cargo build --release` cannot run on the Windows dev box — no cmake, and
`opusic-sys` builds libopus from source. `cargo check` and `cargo test` work
off a cached artefact and will mislead you.

## Things already learned the hard way

- **Editing files from Python:** the checked-in tree is LF, the working copy
  is CRLF, and **`cargo fmt` rewrites files as LF**. Line endings therefore
  change under you mid-session. A script that reads with `newline=''` and
  splits on `'\r\n'` then gets a ONE-ELEMENT list, and `lines[i] = ...`
  silently replaces the whole file with a single line. That destroyed
  `router.rs` once and it surfaced as a parse error in a different module.

  Always read with `newline=None` (universal) and split on `'\n'`.
- **Anchor on the exact indented line, not `l.strip()`.** Looking for a
  function's closing brace with `strip() == '}'` finds the first inner brace
  instead, and the replacement then eats half the body. Use `l == '    }'`.
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

## Recording

`VOICED_RECORD_DIR` switches it on; empty or unset means off, which is a
supported way to run. `VOICED_RECORD_DAYS` defaults to 30.

Raw 8 kHz mono PCM plus a JSON sidecar, one pair per call, in day directories
named `d{epoch_days}`. Raw because the writer must never block the vocoder, and
day directories because pruning is then a `remove_dir_all` rather than stat-ing
a hundred thousand files. Retention runs hourly AND on startup.

What is recorded is the VOCODED audio - what people actually heard - taking the
cleanest lane that exists. A recording of one listener's bad reception is not
evidence of anything.

Calls shorter than 0.4s are discarded as fumbled buttons.

`rec.index` and `rec.audio` on the control plane are node-key authenticated,
and the platform proxies them - a browser never reaches them directly. Call ids
are `{epoch}-{tg}-{seq}` and therefore guessable, so the PLATFORM filters both
the listing and the audio fetch against the caller's own codeplug.

## AM

Conventional routes carry a modulation. FM captures - the stronger signal wins
and the weaker is not heard at all, which is `Keyed::Doubled` and no route. AM
does not capture: both transmitters reach the receiver, so the second gets
`Keyed::Mixed`, joins `Route.also`, and both are routed.

Summing happens at the NODE (`dsp::AmMix`), not at each client. Two talkers'
frames arriving at one listener would otherwise interleave into alternating
chunks of each, which sounds like neither. The mixer is self-clocking off
whoever is transmitting and adds a 620 Hz heterodyne while more than one radio
is up.

The FXServer decides which channels are AM - it holds the codeplug. Airband is
inferred from 108-137 MHz when a channel does not say.
