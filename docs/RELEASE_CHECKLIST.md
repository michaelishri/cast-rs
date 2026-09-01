# Release checklist

Use this checklist to publish a complete Cast release: the version commit, annotated Git tag,
GitHub release, Homebrew formula update, and Homebrew bottles. Run the sections in order. Do not
publish the Homebrew change until the upstream GitHub release and all tap pull-request checks are
green.

## Release invariants

The following values must describe the same release:

- `Cargo.toml` package version: `X.Y.Z`
- root package version in `Cargo.lock`: `X.Y.Z`
- annotated Git tag and GitHub release: `vX.Y.Z`
- Homebrew source URL: `.../refs/tags/vX.Y.Z.tar.gz`
- Homebrew formula and bottle release version: `X.Y.Z`

Tags and published release artifacts are immutable. If released code is wrong, fix it in a new
patch release; do not move or replace a published tag.

## 1. Set up and choose the version

- [ ] Choose a semantic version and export the identifiers used below.

  ```sh
  VERSION=0.10.0
  UPSTREAM=michaelishri/cast-rs
  TAP=michaelishri/homebrew-tap
  ```

- [ ] Confirm GitHub CLI authentication can write to both repositories.

  ```sh
  gh auth status
  gh repo view "$UPSTREAM"
  gh repo view "$TAP"
  ```

- [ ] Fetch current upstream state and create a clean release-preparation worktree from
      `origin/main`. Substitute a different unused directory if necessary.

  ```sh
  git fetch origin --prune
  git worktree add "../cast-rs-release-${VERSION}" \
    -b "release/v${VERSION}" origin/main
  cd "../cast-rs-release-${VERSION}"
  ```

- [ ] Confirm the new version is greater than every existing release and does not already have a
      tag or GitHub release.

  ```sh
  git tag --list 'v*' --sort=-version:refname | head
  test -z "$(git tag --list "v${VERSION}")"
  ! gh release view "v${VERSION}" --repo "$UPSTREAM" >/dev/null 2>&1
  ```

## 2. Prepare and merge the version bump

- [ ] Edit the package `version` in `Cargo.toml` to `$VERSION`.
- [ ] Refresh `Cargo.lock`, then verify that only the intended root package version changed.

  ```sh
  cargo check
  git diff -- Cargo.toml Cargo.lock
  ```

- [ ] Update any release-specific documentation needed for this version. Release notes themselves
      are generated from merged pull requests by the release workflow.
- [ ] Run the complete local release gate.

  ```sh
  cargo fmt -- --check
  cargo clippy --locked --all-targets --all-features -- -D warnings
  cargo test --locked
  cargo build --locked --release
  ./target/release/cast --version
  test "$(./target/release/cast --version | awk '{print $2}')" = "$VERSION"
  ```

- [ ] Review the complete diff and confirm the worktree contains no generated or unrelated files.

  ```sh
  git status --short
  git diff --check
  git diff
  ```

- [ ] Commit, push, and open the version pull request.

  ```sh
  git add Cargo.toml Cargo.lock
  git commit -m "Bump version to ${VERSION}"
  git push -u origin "release/v${VERSION}"
  gh pr create \
    --repo "$UPSTREAM" \
    --base main \
    --head "release/v${VERSION}" \
    --title "Bump version to ${VERSION}" \
    --body "Prepare the v${VERSION} release."
  ```

- [ ] Wait for upstream CI, inspect any failure, and merge only when every required check is green.

  ```sh
  UPSTREAM_PR=$(gh pr view --repo "$UPSTREAM" --json number --jq .number)
  gh pr checks "$UPSTREAM_PR" --repo "$UPSTREAM" --watch --interval 15
  gh pr merge "$UPSTREAM_PR" --repo "$UPSTREAM" --squash
  ```

## 3. Tag and publish the GitHub release

- [ ] Fetch the merged commit into a clean worktree and prove `HEAD` is exactly `origin/main`.
      This prevents tagging the pre-squash pull-request commit.

  ```sh
  git fetch origin --prune
  git switch --detach origin/main
  test "$(git rev-parse HEAD)" = "$(git rev-parse origin/main)"
  test -z "$(git status --porcelain)"
  test "$(sed -n 's/^version = "\([^"]*\)"/\1/p' Cargo.toml | head -n 1)" = "$VERSION"
  ```

- [ ] Run the release helper. It repeats formatting, Clippy, and tests, creates the annotated
      `v$VERSION` tag, and pushes the tag.

  ```sh
  ./scripts/release.sh
  ```

- [ ] Verify that the tag points to the merged `main` commit and is annotated.

  ```sh
  git fetch origin --tags
  test "$(git rev-parse "v${VERSION}^{commit}")" = "$(git rev-parse origin/main)"
  test "$(git cat-file -t "v${VERSION}")" = tag
  ```

- [ ] Watch `.github/workflows/release.yml` finish successfully. It builds Apple Silicon, Intel,
      Linux x86_64, and Linux aarch64 archives and then creates the GitHub release.

  ```sh
  gh run list --repo "$UPSTREAM" --workflow release.yml --branch "v${VERSION}" --limit 3
  RELEASE_RUN=$(gh run list --repo "$UPSTREAM" --workflow release.yml \
    --branch "v${VERSION}" --limit 1 --json databaseId --jq '.[0].databaseId')
  gh run watch "$RELEASE_RUN" --repo "$UPSTREAM" --exit-status --interval 10
  ```

- [ ] Verify the non-draft, non-prerelease GitHub release has all twelve expected assets: macOS
      `arm64`/`x86_64` and Linux `aarch64`/`x86_64` archives, one checksum per archive, an `otool`
      audit for each macOS archive, and an `ldd` audit for each Linux archive.

  ```sh
  gh release view "v${VERSION}" --repo "$UPSTREAM" \
    --json url,isDraft,isPrerelease,assets \
    --jq '{url,isDraft,isPrerelease,assets:[.assets[].name]}'

  RELEASE_DIR=$(mktemp -d)
  gh release download "v${VERSION}" --repo "$UPSTREAM" --dir "$RELEASE_DIR"
  (cd "$RELEASE_DIR" && shasum -a 256 -c ./*.sha256)
  ! grep -H 'not found' "$RELEASE_DIR"/*.ldd.txt
  ```

- [ ] Smoke-test an archive on a matching Mac when one is available.

  ```sh
  ARCH=$(uname -m)
  tar -xzf "$RELEASE_DIR/cast-${VERSION}-macos-${ARCH}.tar.gz" -C "$RELEASE_DIR"
  "$RELEASE_DIR/cast-${VERSION}-macos-${ARCH}/cast" --version
  codesign --verify --deep --strict \
    "$RELEASE_DIR/cast-${VERSION}-macos-${ARCH}/Cast.app"
  plutil -lint "$RELEASE_DIR/cast-${VERSION}-macos-${ARCH}/Cast.app/Contents/Info.plist"
  ```

- [ ] Complete [the Linux release matrix](LINUX_VALIDATION.md) on the tagged archives. The release
      workflow performs native x86_64 and aarch64 archive smoke tests; the x86_64 build establishes
      the Ubuntu 22.04/glibc 2.35 floor, while the hosted aarch64 build uses Ubuntu 24.04. Record the
      GNOME/KDE and real-device results in the release issue before calling Linux support complete.

## 4. Update the Homebrew formula

The tap's daily autobump workflow may already have opened a formula pull request. Use that pull
request if it targets exactly `v$VERSION`; otherwise prepare one manually.

- [ ] Clone or update the tap in a separate checkout from its current `main`.

  ```sh
  cd ..
  if test -d homebrew-tap/.git; then
    git -C homebrew-tap fetch origin --prune
  else
    git clone "git@github.com:${TAP}.git" homebrew-tap
  fi
  cd homebrew-tap
  git switch main
  git pull --ff-only origin main
  ```

- [ ] Check for an existing autobump PR. If one is correct, record its number, check it out, verify
      its formula diff as described below, and then skip to section 5.

  ```sh
  gh pr list --repo "$TAP" --state open --search "cast ${VERSION} in:title"
  TAP_PR=123
  gh pr checkout "$TAP_PR" --repo "$TAP"
  ```

- [ ] If there is no correct autobump PR, create the manual formula branch.

  ```sh
  git switch -c "cast-${VERSION}"
  ```

- [ ] Download the GitHub-generated source archive and calculate its SHA-256. The formula uses this
      source archive, not either prebuilt CLI archive.

  ```sh
  SOURCE_ARCHIVE=$(mktemp)
  curl -L --fail --show-error \
    "https://github.com/${UPSTREAM}/archive/refs/tags/v${VERSION}.tar.gz" \
    -o "$SOURCE_ARCHIVE"
  SOURCE_SHA=$(shasum -a 256 "$SOURCE_ARCHIVE" | awk '{print $1}')
  printf '%s\n' "$SOURCE_SHA"
  ```

- [ ] Edit `Formula/cast.rb`:

  - change its source URL to `v$VERSION.tar.gz`;
  - replace the source `sha256` with `$SOURCE_SHA`;
  - remove the old `bottle do` block—publication regenerates it;
  - preserve the license, dependencies, resources, compatibility patches, install method, test
    block, and livecheck unless the new source requires a deliberate change.

- [ ] Confirm the formula contains only the expected release update.

  ```sh
  git diff --check
  git diff -- Formula/cast.rb
  rg "v${VERSION}|${SOURCE_SHA}" Formula/cast.rb
  ```

- [ ] Commit, push, and open the tap pull request.

  ```sh
  git add Formula/cast.rb
  git commit -m "cast ${VERSION}"
  git push -u origin "cast-${VERSION}"
  gh pr create \
    --repo "$TAP" \
    --base main \
    --head "cast-${VERSION}" \
    --title "cast ${VERSION}" \
    --body "Update Cast to v${VERSION} and build Homebrew bottles."
  TAP_PR=$(gh pr view --repo "$TAP" --json number --jq .number)
  ```

## 5. Build and publish Homebrew bottles

- [ ] Record the actual PR branch. This also supports PRs created by the autobump workflow, whose
      branch name may differ from the manual `cast-$VERSION` convention.

  ```sh
  TAP_BRANCH=$(gh pr view "$TAP_PR" --repo "$TAP" --json headRefName --jq .headRefName)
  ```

- [ ] Wait for every `brew test-bot` check on the tap pull request. The matrix audits Linux and
      builds/tests supported macOS targets. Never publish from a red or still-running matrix.

  ```sh
  gh pr checks "$TAP_PR" --repo "$TAP" --watch --interval 15
  ```

- [ ] Inspect the workflow's uploaded artifacts. A green job can legitimately omit a bottle when
      that runner has unbottled dependencies, so verify which platforms actually produced
      `bottles_*` artifacts rather than inferring that from check status.

  ```sh
  TEST_RUN=$(gh run list --repo "$TAP" --workflow tests.yml \
    --branch "$TAP_BRANCH" --limit 1 --json databaseId --jq '.[0].databaseId')
  gh api "repos/${TAP}/actions/runs/${TEST_RUN}/artifacts" \
    --jq '.artifacts[] | [.name, .size_in_bytes, .expired] | @tsv'
  ```

- [ ] Capture the exact tested pull-request head SHA, then dispatch the bottle publication
      workflow with both the PR number and SHA. The SHA guard prevents publishing an untested later
      push.

  ```sh
  HEAD_SHA=$(gh pr view "$TAP_PR" --repo "$TAP" --json headRefOid --jq .headRefOid)
  gh workflow run publish.yml --repo "$TAP" \
    -f pull_request="$TAP_PR" \
    -f head_sha="$HEAD_SHA"
  ```

- [ ] Identify and watch the newly dispatched `brew pr-pull` run.

  ```sh
  gh run list --repo "$TAP" --workflow publish.yml --event workflow_dispatch --limit 3
  PUBLISH_RUN=$(gh run list --repo "$TAP" --workflow publish.yml \
    --event workflow_dispatch --limit 1 --json databaseId --jq '.[0].databaseId')
  gh run watch "$PUBLISH_RUN" --repo "$TAP" --exit-status --interval 10
  ```

`brew pr-pull` closes the tap PR, commits the formula and generated bottle block to `main`, creates
the `cast-$VERSION` tag/release in the tap, and uploads the bottle archives. A closed rather than
GitHub-merged PR is expected for this workflow.

## 6. Verify the published Homebrew release

- [ ] Confirm tap `main` contains the new source version and generated bottle checksums.

  ```sh
  git fetch origin --prune --tags
  git switch main
  git pull --ff-only origin main
  rg "v${VERSION}|cast-${VERSION}|bottle do" Formula/cast.rb
  ```

- [ ] Confirm the tap bottle release is public and lists the expected bottle assets.

  ```sh
  gh release view "cast-${VERSION}" --repo "$TAP" \
    --json url,isDraft,isPrerelease,assets \
    --jq '{url,isDraft,isPrerelease,assets:[.assets[].name]}'
  ```

- [ ] Download every bottle, verify that its checksum matches the formula, and confirm it contains
      `cast/$VERSION/bin/cast`.

  ```sh
  BOTTLE_DIR=$(mktemp -d)
  gh release download "cast-${VERSION}" --repo "$TAP" --dir "$BOTTLE_DIR"
  shasum -a 256 "$BOTTLE_DIR"/*.bottle.tar.gz
  for archive in "$BOTTLE_DIR"/*.bottle.tar.gz; do
    tar -tzf "$archive" | grep "cast/${VERSION}/bin/cast$"
  done
  ```

- [ ] Test the public installation path on a supported Mac.

  ```sh
  brew update
  brew upgrade michaelishri/tap/cast || brew install michaelishri/tap/cast
  cast --version
  test "$(cast --version | awk '{print $2}')" = "$VERSION"
  brew test michaelishri/tap/cast
  ```

- [ ] Review the generated GitHub release notes and edit them if necessary.
- [ ] Announce the release only after both the upstream archive and Homebrew installation checks
      pass.

## Failure and recovery rules

- If local checks or a pull-request check fails, fix the branch and rerun the complete relevant
  gate before continuing.
- If the upstream release workflow fails transiently after the tag is pushed, rerun that workflow;
  do not recreate or move the tag.
- If a code or packaging defect is discovered after publication, make a new patch release and start
  this checklist again.
- If a tap PR fails, update that same PR until all checks pass. Do not dispatch `publish.yml` early.
- If bottle publication fails, inspect the `brew pr-pull` logs. Rerun it with the same PR and tested
  head SHA only when the failure is transient and tap `main` was not partially updated.
- Never manually edit published bottle checksums. They must be generated by `brew pr-pull` from the
  tested workflow artifacts.

## Definition of done

- [ ] `origin/main`, `v$VERSION`, and the GitHub release refer to the intended upstream commit.
- [ ] Both upstream macOS architecture archives and checksum files are published and verified.
- [ ] Tap `main` contains the matching formula source URL, source SHA, and generated bottle block.
- [ ] The tap's `cast-$VERSION` release contains every bottle named in the formula.
- [ ] `brew install michaelishri/tap/cast`, `cast --version`, and `brew test` succeed on a supported
      Mac.
