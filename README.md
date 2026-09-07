# rfx-voiced

The voice node for the InteropHQ P25 radio system.

It connects **out** to each FXServer's built-in Mumble server as an ordinary
client, listens to every channel, runs what it hears through a vocoder chain
that sounds like a P25 radio, and streams the result to game clients over a
WebSocket. It never joins a game, never injects audio back into Mumble, and
never holds a player's session.

Because it is a Mumble *client* rather than a replacement server, there is no
UDP path and no OCB2 crypto to implement: voice arrives tunnelled over the
existing TLS connection.

## Running it

Two listeners, and they have different exposure:

| Variable | Default | Who reaches it |
|---|---|---|
| `VOICED_CONTROL` | `127.0.0.1:8787` | FXServers only |
| `VOICED_STREAM` | `0.0.0.0:8788` | Players' game clients — must be public |

### Deploy once, then leave it alone

With a platform key set there is **nothing to enrol and nothing to restart**.
The node polls the platform every `VOICED_POLL_SECONDS` (default 60) and
reconciles what it finds: it attaches taps for servers that appeared, detaches
servers that went away, and rebuilds its key index so a server added a minute
ago can authenticate immediately.

A node deployed before any server exists is not an error - it comes up, serves
nothing, and attaches when servers appear. A server that changes host or port
is treated as gone and then new, because reconnecting forever to an address
that no longer serves it is the failure that looks most like working.

A failed poll keeps the last good list rather than emptying it, so an
unreachable dashboard does not detach every server on the network.

Where its server list comes from, in order of preference:

1. The platform, when `VOICED_PLATFORM_URL` and `VOICED_PLATFORM_KEY` are set.
2. `servers.cache.json`, the last good response. A dashboard outage should
   degrade the platform — no new enrolments take effect — rather than stop it.
3. `servers.json`, hand-written, which is how the node runs standalone.

The node receives key **hashes** from the platform, never keys, so a
compromised node cannot impersonate the servers it serves.

### Everything else

| Variable | Default | |
|---|---|---|
| `VOICED_PLATFORM_URL` | — | InteropHQ base URL |
| `VOICED_PLATFORM_KEY` | — | Node key. A different principal from a server key |
| `VOICED_USER` | `[999] radiotap` | Tap name. FiveM names clients `[id] name` |
| `VOICED_CODEC2_MODE` | `3200` | 3200 or 2400 |
| `VOICED_CACHE` | `servers.cache.json` | |
| `VOICED_STORE` | `servers.json` | |
| `VOICED_POLL_SECONDS` | `60` | How often the server list is reconciled |

### Standalone

Only for running with **no platform**. With `VOICED_PLATFORM_URL` set, servers
come from the dashboard and none of this is needed.

```
rfx-voiced enroll <name> <host:port>   add a server, print its key
rfx-voiced list                        show enrolled servers
rfx-voiced rotate <id>                 re-issue a key, killing the old one
rfx-voiced revoke <id>                 remove a server
```

`servers.json` holds enrolled keys **in plaintext**. It is gitignored, and it
should be treated like any other password file.

## Building

libopus is compiled from source by `opusic-sys`, so a build needs `cmake` and
a C++ compiler. Everything else — protoc included — is vendored.

```
cargo build --release
```

`Dockerfile` builds it in two stages and produces a small runtime image.

The binary links only libc, libm and libgcc - libopus and ring are static, and
rustls carries its own root store - so it runs on any glibc at least as new as
the one it was built against. CI builds on Debian 12; the Pelican runtime image
is Debian 13. Do not run it on Alpine: musl is a different libc, not an older
one.

## Pelican

`egg-interophq-voice-node.json` is a ready-to-import egg. It builds once at
install and the startup command runs the binary, rather than recompiling on
every boot. It needs **two allocations**: the primary is the control API, and
a second one — set in the `VOICED_STREAM_PORT` variable — is the audio stream.

## Deploying an update

```
git tag -a v0.1.4 -m "what changed" && git push origin v0.1.4
```

Then in Pelican: **Settings -> Reinstall Server**.

**Reinstall, not restart.** The startup command runs the binary already on
disk and downloads nothing; only the install script fetches, and it pulls the
latest release. A restart after tagging looks like it worked and changes
nothing.

Reinstall writes only the binary. `servers.json`, the cache and everything else
in the volume are left alone.

Pushing to `main` ships nothing on its own - the release job builds **the tag**,
so an untagged fix rebuilds nothing and a tag on a broken commit rebuilds the
broken commit.

## Releases

Tagging `v*` builds a Linux x86_64 binary, attaches it and the Pelican egg to a
GitHub release, and pushes a container image to `ghcr.io`. Releases are cut
from tags rather than from every commit, because "which version is that node
running" is the first question anyone asks when one misbehaves.

```
git tag -a v0.1.0 -m "First release" && git push origin v0.1.0
```
