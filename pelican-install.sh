#!/bin/bash
# InteropHQ voice node - install
#
# Server Files: /mnt/server
#
# Two modes:
#
#   release  download the prebuilt binary from a GitHub release. Seconds, and
#            no compiler, so it cannot run the panel out of memory. Default.
#   source   clone and cargo build. Needs ~2GB and several minutes.
#
# Building on the panel was the original design and it was wrong: rustc runs
# one process per core, each wanting hundreds of megabytes, and Pelican gives
# the install container the server's memory limit. A 1GB server OOM-kills the
# build partway through with SIGKILL and no explanation beyond "signal: 9".

set -e

# Printed first, so the log says which script ran. Two reinstalls were spent
# debugging a script that was not the one being edited: the panel keeps the
# install script on the EGG, and a server attached to a different (or
# duplicated) egg silently runs the old one while reporting success.
echo "=== rfx-voiced install script r4 (release mode) ==="

export DEBIAN_FRONTEND=noninteractive
apt-get update
apt-get install -y --no-install-recommends ca-certificates curl jq git

mkdir -p /mnt/server
cd /mnt/server

# The recorder writes here. Created now rather than on first call so a
# permissions problem surfaces during install, where somebody is watching,
# instead of silently disabling recording weeks later.
mkdir -p /mnt/server/recordings

export HOME=/mnt/server
export CARGO_HOME=/mnt/server/.cargo

if [[ ${GIT_ADDRESS} != *.git ]]; then
    GIT_ADDRESS=${GIT_ADDRESS}.git
fi

# NEVER echo a URL after credentials have been spliced into it. The panel's
# install log is visible to anyone with access to the server, is kept, and is
# the first thing people paste when asking for help - so a token printed here
# is a token to treat as public.
REPO=$(echo "${GIT_ADDRESS}" | sed -E 's#.*github\.com[:/]([^/]+/[^/]+)\.git#\1#')
AUTH_URL="${GIT_ADDRESS}"
if [ -n "${USERNAME}" ] && [ -n "${ACCESS_TOKEN}" ]; then
    AUTH_URL="https://${USERNAME}:${ACCESS_TOKEN}@$(echo -e ${GIT_ADDRESS} | cut -d/ -f3-)"
fi

BRANCH="${BRANCH:-main}"
MODE="${INSTALL_MODE:-release}"

install_from_release() {
    echo "looking for the latest release of ${REPO}"

    local hdr=(-H "Accept: application/vnd.github+json")
    [ -n "${ACCESS_TOKEN}" ] && hdr+=(-H "Authorization: Bearer ${ACCESS_TOKEN}")

    local json
    json=$(curl -fsSL "${hdr[@]}" "https://api.github.com/repos/${REPO}/releases/latest") || return 1

    local tag asset_id
    tag=$(echo "${json}" | jq -r '.tag_name // empty')
    asset_id=$(echo "${json}" | jq -r \
        '.assets[]? | select(.name | endswith("x86_64-unknown-linux-gnu.tar.gz")) | .id' | head -1)

    [ -n "${asset_id}" ] || return 1
    echo "found ${tag}, asset ${asset_id}"

    # The asset endpoint, not browser_download_url: that one is unauthenticated
    # and 404s on a private repository.
    curl -fsSL "${hdr[@]}" -H "Accept: application/octet-stream" \
        "https://api.github.com/repos/${REPO}/releases/assets/${asset_id}" \
        -o /tmp/voiced.tar.gz || return 1

    tar -xzf /tmp/voiced.tar.gz -C /tmp
    local bin
    bin=$(find /tmp -maxdepth 3 -type f -name rfx-voiced | head -1)
    [ -n "${bin}" ] || return 1

    cp "${bin}" /mnt/server/rfx-voiced
    chmod +x /mnt/server/rfx-voiced
    rm -rf /tmp/voiced.tar.gz
    echo "installed ${tag} from release"
}

install_from_source() {
    echo "building from source"
    apt-get install -y --no-install-recommends cmake g++ curl build-essential

    if ! command -v cargo >/dev/null 2>&1; then
        curl --proto '=https' --tlsv1.2 -sSf https://sh.rustup.rs \
            | sh -s -- -y --profile minimal --no-modify-path
        export PATH="${CARGO_HOME}/bin:${PATH}"
    fi

    local src=/mnt/server/source

    # The checkout is owned by the server's uid, and this script runs as root.
    # Git refuses to touch a repository owned by somebody else - a sensible
    # default on a shared machine, and exactly wrong here, where "somebody
    # else" is the container this very install is for.
    git config --global --add safe.directory '*'

    if [ -d "${src}/.git" ]; then
        echo "updating existing checkout"
        cd "${src}"
        git remote set-url origin "${AUTH_URL}"
        git fetch --all --prune
        git reset --hard "origin/${BRANCH}"
    else
        echo "cloning ${REPO} (${BRANCH})"
        rm -rf "${src}"
        git clone --depth 1 --single-branch --branch "${BRANCH}" "${AUTH_URL}" "${src}"
    fi

    cd "${src}/${CRATE_DIR:-.}"
    [ -f Cargo.toml ] || { echo "no Cargo.toml at ${CRATE_DIR:-.}"; exit 1; }

    # Two jobs, not one per core. Peak memory is roughly jobs x rustc, and the
    # install container inherits the server's memory limit - unbounded
    # parallelism is what turns a 1GB server into a SIGKILL halfway through
    # compiling syn.
    echo "building with ${CARGO_JOBS:-2} job(s) - the slow part, and it only happens here"
    cargo build --release --jobs "${CARGO_JOBS:-2}"

    cp target/release/rfx-voiced /mnt/server/rfx-voiced
    chmod +x /mnt/server/rfx-voiced
    rm -rf target
}

if [ "${MODE}" = "source" ]; then
    install_from_source
elif ! install_from_release; then
    echo "no usable release found - falling back to building from source"
    install_from_source
fi

# Leaving a broken install to be discovered at startup as "No such file or
# directory" wastes the one place that could have said what went wrong.
if [ ! -x /mnt/server/rfx-voiced ]; then
    echo "INSTALL FAILED - no binary at /mnt/server/rfx-voiced"
    exit 1
fi

echo "install complete - $(/mnt/server/rfx-voiced help | head -1)"
exit 0
