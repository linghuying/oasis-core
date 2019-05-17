#!/bin/bash

# Build a Docker context tarball.

# Helpful tips on writing build scripts:
# https://buildkite.com/docs/pipelines/writing-build-scripts
set -euxo pipefail

###############
# Required args
###############
dst=$1

EKIDEN_UNSAFE_SKIP_AVR_VERIFY=1
export EKIDEN_UNSAFE_SKIP_AVR_VERIFY

extra_args=""
if [ "${EKIDEN_KM_CUSTOM_KEYS:-1}" == "1" ]; then
    extra_args="--features custom-keys"
fi

# Install ekiden-tools
cargo install --force --path tools

# Build the worker, compute node and key manager
make -C go
cargo build -p ekiden-runtime-loader --release

pushd keymanager-runtime
    cargo build --release ${extra_args}
    cargo build --release --target x86_64-fortanix-unknown-sgx ${extra_args}
    cargo elf2sgxs --release
popd

tar -czf "$dst" \
    go/ekiden/ekiden \
    target/release/ekiden-runtime-loader \
    target/release/ekiden-keymanager-runtime \
    target/x86_64-fortanix-unknown-sgx/release/ekiden-keymanager-runtime.sgxs \
    docker/deployment/Dockerfile