#!/usr/bin/env bash
set -euo pipefail

repo_root="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
cd "$repo_root"

if [[ -n "$(git status --porcelain)" ]]; then
  echo "Refusing to release with uncommitted changes." >&2
  exit 1
fi

version="$(sed -n 's/^version = "\([^"]*\)"/\1/p' Cargo.toml | head -n 1)"
if [[ -z "$version" ]]; then
  echo "Could not read package.version from Cargo.toml." >&2
  exit 1
fi

tag="v$version"
if git rev-parse --verify --quiet "refs/tags/$tag" >/dev/null; then
  echo "Tag $tag already exists." >&2
  exit 1
fi

cargo fmt -- --check
cargo clippy --all-targets --all-features -- -D warnings
cargo test --locked

git tag --annotate "$tag" --message "Release $tag"
git push origin "$tag"

echo "Pushed $tag. GitHub Actions will build and publish the release assets."
