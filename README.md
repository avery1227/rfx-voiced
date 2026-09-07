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

### Standalone

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

## Pelican

`egg-interophq-voice-node.json` is a ready-to-import egg. It builds once at
install and the startup command runs the binary, rather than recompiling on
every boot. It needs **two allocations**: the primary is the control API, and
a second one — set in the `VOICED_STREAM_PORT` variable — is the audio stream.
