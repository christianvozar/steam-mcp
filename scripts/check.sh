#!/usr/bin/env sh
# Local quality gate: the same checks a CI service would run, at no cost.
# Usage: scripts/check.sh            (format, lint, tests, release build)
#        scripts/check.sh --fuzz     (also run an extended fuzz pass)
set -eu
cd "$(dirname "$0")/.."

echo "==> cargo fmt --check"
cargo fmt --check

echo "==> cargo clippy"
cargo clippy --all-targets --locked -- -D warnings

echo "==> cargo test"
cargo test --locked

echo "==> cargo build --release"
cargo build --release --locked

if [ "${1:-}" = "--fuzz" ]; then
    for seed in 1 2 3 42 1337; do
        echo "==> fuzz seed $seed"
        FUZZ_SEED=$seed FUZZ_ITERS=100000 cargo test --release --locked --test fuzz
    done
fi

echo "All checks passed."
