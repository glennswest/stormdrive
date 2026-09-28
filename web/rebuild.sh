#!/bin/sh
# Rebuild web/dist (and package-lock.json) on dev, never here: sc-build runs
# the pushed commit in a scratch volume, and the build comes back as a
# base64 tarball on stdout. Commit and push web/ source first.
#
#   web/rebuild.sh          npm ci/install, npm test, npm run build → web/
#   web/rebuild.sh --check  the same on dev, then fail unless the committed
#                           web/dist is byte-for-byte what the source builds
set -eu
cd "$(dirname "$0")/.."
mkdir -p tmp
log=tmp/web-rebuild.log
install='if [ -f package-lock.json ]; then npm ci --no-audit --no-fund; else npm install --no-audit --no-fund; fi'
if [ "${1:-}" = --check ]; then
    sc-build "cd web && cp -r dist ../dist.committed && ($install) >/dev/null && npm test && npm run build && diff -r ../dist.committed dist && echo 'web/dist matches its source'"
    exit
fi
sc-build "cd web && ($install) >/dev/null && npm test && npm run build && echo @@TAR-BEGIN && tar czf - package-lock.json dist | base64 -w0 && echo && echo @@TAR-END" > "$log" 2>&1 || {
    grep -v '^H4sI' "$log" | tail -40
    exit 1
}
awk '/^@@TAR-BEGIN/{f=1;next}/^@@TAR-END/{f=0}f' "$log" | base64 -d | tar xzf - -C web
grep -v '^H4sI' "$log" | grep -E 'tests|pass|fail|dist/' || true
git status --short -- web
