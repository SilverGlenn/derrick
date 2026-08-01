#!/usr/bin/env bash
# Convenience wrapper around build.cmd (which sets up MSVC + Rust env).
set -e
cd "$(dirname "$0")"
cmd //c build.cmd "$@"
