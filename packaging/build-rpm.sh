#!/usr/bin/env bash
# Build the Fedora package inside a Fedora container, from the repo root:
#   ./packaging/build-rpm.sh
# The RPM lands in ./packaging/out/.
# The froklog library is a git dependency, so the container needs network
# access during the cargo build (pinned by Cargo.lock).
set -euo pipefail
cd "$(dirname "$0")/.."
OUT="packaging/out"
mkdir -p "$OUT"
GITREV=$(git rev-parse --short HEAD 2>/dev/null || echo local)
git archive --format=tar -o "$OUT/froklog-watch-src.tar" HEAD
docker run --rm -v "$PWD/$OUT":/out -v "$PWD/packaging/froklog-watch.spec":/spec.spec:ro \
    fedora:42 bash -ec '
      dnf install -y -q cargo rust gcc git-core openssl-devel pkgconf-pkg-config dbus-devel libxkbcommon-devel rpm-build
      mkdir -p /root/rpmbuild/SOURCES
      cp /out/froklog-watch-src.tar /root/rpmbuild/SOURCES/
      rpmbuild -bb --define "gitrev '"$GITREV"'" /spec.spec
      cp /root/rpmbuild/RPMS/*/froklog-watch-*.rpm /out/
    '
ls -la "$OUT"/*.rpm
