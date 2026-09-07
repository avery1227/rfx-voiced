#!/bin/bash
# Voice node installation script, for the Pelican egg.
#
# Server Files: /mnt/server
#
# This replaces the stock Rust Bot installer, which only clones the repository
# and leaves `cargo run --release` as the startup command. That recompiles on
# every boot - a cold cache is 180 crates plus libopus from source - which is
# minutes of CPU on a host that is also running game servers, with voice down
# the whole time. Here the build happens ONCE, at install, and the startup
# command runs the binary.
#
# INSTALL CONTAINER must have cargo, cmake and g++. Use the same image as the
# runtime one (Dockerfile.yolk); the stock Rust yolk has none of the three that
# matter, which is what makes libopus fail to build.

set -e

apt update
apt install -y --no-install-recommends git ca-certificates

mkdir -p /mnt/server
cd /mnt/server

# Cargo writes to $HOME. Without this it tries to use root's, which is not on
# the volume, so the registry cache is thrown away with the container.
export HOME=/mnt/server
export CARGO_HOME=/mnt/server/.cargo

## add git ending if it's not on the address
if [[ ${GIT_ADDRESS} != *.git ]]; then
    GIT_ADDRESS=${GIT_ADDRESS}.git
fi

if [ -z "${USERNAME}" ] && [ -z "${ACCESS_TOKEN}" ]; then
    echo -e "using anon api call"
else
    GIT_ADDRESS="https://${USERNAME}:${ACCESS_TOKEN}@$(echo -e ${GIT_ADDRESS} | cut -d/ -f3-)"
fi

if [ "$(ls -A /mnt/server)" ]; then
    echo -e "/mnt/server directory is not empty."
    if [ -d .git ]; then
        echo -e ".git directory exists"
        if [ -f .git/config ]; then
            echo -e "loading info from git config"
            ORIGIN=$(git config --get remote.origin.url)
        else
            echo -e "files found with no git config"
            echo -e "closing out without touching things to not break anything"
            exit 10
        fi
    fi

    if [ "${ORIGIN}" == "${GIT_ADDRESS}" ]; then
        echo "pulling latest from github"
        git pull
    fi
else
    echo -e "/mnt/server is empty.\ncloning files into repo"
    if [ -z ${BRANCH} ]; then
        echo -e "cloning default branch"
        git clone ${GIT_ADDRESS} .
    else
        echo -e "cloning ${BRANCH}'"
        git clone --single-branch --branch ${BRANCH} ${GIT_ADDRESS} .
    fi
fi

# The crate is not necessarily at the repository root - this repo keeps it at
# resources/[local]/rfx_p25/voiced. CRATE_DIR is an egg variable so the same
# script works for a repo that is only the node.
CRATE_DIR="${CRATE_DIR:-.}"
cd "/mnt/server/${CRATE_DIR}"

echo -e "building rfx-voiced (this is the slow part, and it only happens here)"
cargo build --release

cp target/release/rfx-voiced /mnt/server/rfx-voiced
chmod +x /mnt/server/rfx-voiced

# target/ is several GB of intermediates and the binary is already copied out.
# Keeping it would count against the server's disk quota for no benefit; a
# reinstall rebuilds from the registry cache in .cargo, which is kept.
rm -rf target

echo -e "install complete - start command should be ./rfx-voiced"
exit 0
