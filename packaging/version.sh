#!/bin/sh
# The version being built: a release tag's ("v1.2.3" or "1.2.3"), else
# Cargo.toml's. Release builds export it as FF_VERSION, which the
# programs report and the packages and installer carry.
case "${GITHUB_REF:-}" in
refs/tags/*) echo "${GITHUB_REF_NAME#v}" ;;
*) sed -n 's/^version = "\(.*\)"/\1/p' "$(dirname "$0")/../Cargo.toml" | head -n 1 ;;
esac
