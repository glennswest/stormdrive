#!/bin/sh
# Build stormdrive's test image context for the commit checked out (#11).
#
#   test/build.sh [target]        default x86_64-unknown-linux-musl
#
# Per stormcentral docs/test-standard.md this runs first, in the checkout on
# the build box, with cargo, and stages the static test binary in
# test/.stage/; test/Containerfile (context: the repo root) packages it
# FROM scratch. One image serves all three suites (`/test <suite>`).
# With STAGE_ONLY=1 it stops after staging and prints the path; otherwise it
# also runs `podman build` and tags stormdrive-test. It builds only the test
# crate, never the daemon.
set -eu
target=${1:-x86_64-unknown-linux-musl}
root=$(cd "$(dirname "$0")/.." && pwd)
commit=$(git -C "$root" rev-parse HEAD 2>/dev/null || echo unknown)

cargo build --release --locked --target "$target" -p stormdrive-test --manifest-path "$root/Cargo.toml"
tdir=$(cargo metadata --format-version 1 --no-deps --manifest-path "$root/Cargo.toml" |
    sed 's/.*"target_directory":"\([^"]*\)".*/\1/')
stage="$root/test/.stage"
rm -rf "$stage"
mkdir -p "$stage"
cp "$tdir/$target/release/stormdrive-test" "$stage/"

if [ "${STAGE_ONLY:-0}" = 1 ]; then
    echo "$stage"
    exit 0
fi
podman build -f "$root/test/Containerfile" --build-arg COMMIT="$commit" -t stormdrive-test "$root"
