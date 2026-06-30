# Release process

This page is the canonical procedure for cutting a release tag. A release
is the point where a commit becomes immutable and shipped as a binary, so
the checks below are mandatory: a single red CI job on a tag means the
release artifacts never publish.

The procedure reflects the workflows in `.github/workflows/` and the
conventions encoded in `CHANGELOG.md`. Run every step in order; do not
skip the local re-run.

## Versioning

neenee follows [Semantic Versioning][semver]:

[semver]: https://semver.org/spec/v2.0.0.html

| Bump type | When to use it |
|-----------|----------------|
| Patch (`0.13.0` → `0.13.1`) | Bug fixes and internal performance work with no user-facing behavior change |
| Minor (`0.13.0` → `0.14.0`) | New user-visible features or behavior changes, backward compatible |
| Major (`0.13.0` → `1.0.0`) | Breaking changes to the CLI, config schema, or session format |

When in doubt between patch and minor, choose minor. A feature commit on
`main` (a `feat:` prefix, or any CHANGELOG entry under a `### Added` or
`### Changed` heading) forces at least a minor bump.

## Pre-release checklist

Run this checklist on the `main` branch with a clean working tree. Every
command below mirrors a job in `ci.yml`; running them locally catches
failures before a tag triggers the release workflow.

### 1. Confirm the working tree is releasable

Verify the branch and that there are no uncommitted changes:

```bash
git checkout main
git pull --ff-only
git status   # working tree clean
```

Confirm there are commits since the last tag worth releasing:

```bash
git describe --tags --abbrev=0   # e.g. v0.13.0
git log --oneline v0.13.0..HEAD
```

If the list is empty, there is nothing to release.

### 2. Run the full CI matrix locally

Each command maps to a `ci.yml` job. The gating jobs use `-D warnings`
exactly as CI does, so a green local run is a green CI run. The
`--exclude neenee-quant --exclude neenee-quant-gui` flags mirror the
workspace-selection policy: those crates are work-in-progress and not
gated or shipped.

```bash
# Format (fmt job). cargo fmt has no --exclude, so select gated crates.
cargo fmt --all --check \
  -p neenee-core -p neenee-store -p neenee-providers -p neenee-tools \
  -p neenee-agent -p neenee-code -p neenee-server \
  -p neenee-tui -p neenee-tui-view

# Clippy (clippy job).
RUSTFLAGS="-D warnings" cargo clippy \
  --workspace --all-targets --locked \
  --exclude neenee-quant --exclude neenee-quant-gui

# Tests (test job).
cargo test --workspace --locked --no-fail-fast \
  --exclude neenee-quant --exclude neenee-quant-gui

# Docs (doc job). Catches broken intra-doc links and private-item links.
RUSTDOCFLAGS="-D warnings" cargo doc \
  --workspace --no-deps --document-private-items --locked \
  --exclude neenee-quant --exclude neenee-quant-gui

# MSRV (msrv job). Pin the toolchain to the declared rust-version (1.95).
rustup run 1.95.0 cargo check \
  --workspace --all-targets --locked \
  --exclude neenee-quant --exclude neenee-quant-gui
```

A failure here blocks the release. Fix the code, not the checklist.
The common failure modes are documented in
[Fixing common failures](#fixing-common-failures).

The `audit` and `deny` jobs run in CI only (they need the GitHub token
and a network fetch of the advisory database). Glance at their result on
the latest `main` CI run before tagging; a new advisory in a transitive
dependency is the one thing the local re-run cannot catch.

### 3. Bump the version

The version bump is a single dedicated commit, separate from any code
fixes. The commit message follows the established convention
`release: bump version to vX.Y.Z`.

Bump every workspace member in lockstep. neenee uses a single shared
version across all crates, so a partial bump breaks the path-dependency
graph.

```bash
# Bump all 11 crate manifests from the old to the new version.
for f in crates/*/Cargo.toml; do
  sed -i 's/^version = "0.13.0"/version = "0.14.0"/' "$f"
done
```

Refresh `Cargo.lock` so it carries the new version. `cargo check`
rewrites the lock for the workspace members:

```bash
cargo check --workspace --locked \
  --exclude neenee-quant --exclude neenee-quant-gui
```

Verify all members moved together:

```bash
grep -h '^version' crates/*/Cargo.toml | sort -u   # one line: 0.14.0
git diff --stat Cargo.lock                          # 11 version bumps
```

### 4. Finalize the changelog

Promote the `[Unreleased]` section in `CHANGELOG.md` to a versioned
section, and seed a fresh empty `[Unreleased]` above it for the next
cycle. The release date is the current date in `YYYY-MM-DD` form.

The change looks like this at the top of the file:

```markdown
## [Unreleased]

## [0.14.0] - 2026-07-02

### Fixed
...
```

Every user-visible change since the last tag must have an entry under
the versioned section. If a change landed on `main` without a changelog
entry, add one now — a tag is the last chance to record it.

### 5. Commit and verify

Stage the version bump and changelog together:

```bash
git add crates/*/Cargo.toml Cargo.lock CHANGELOG.md
git commit -m "release: bump version to v0.14.0"
```

Re-run the format and clippy jobs once more on the release commit. The
version bump itself cannot break them, but this guards against a stale
local cache masking an issue.

### 6. Tag and push

The release workflow triggers on a `v*` tag. The tag name is the version
with a `v` prefix; the tag must point at the release commit.

```bash
git tag v0.14.0
git push origin main
git push origin v0.14.0
```

Pushing the tag triggers `release.yml`, which builds release binaries
for five targets and publishes a GitHub Release with auto-generated
notes. Do not delete and re-tag: anyone who pulled the tag now has a
different object. If the release workflow fails, fix forward with a new
patch tag.

## After the release

Watch the `Release` workflow run to completion. Confirm the GitHub
Release appears with all five archive assets attached:

| Asset | Target |
|-------|--------|
| `neenee-<ver>-x86_64-unknown-linux-gnu.tar.gz` | Linux x86-64 |
| `neenee-<ver>-aarch64-unknown-linux-gnu.tar.gz` | Linux ARM64 |
| `neenee-<ver>-x86_64-unknown-linux-musl.tar.gz` | Linux x86-64 (static) |
| `neenee-<ver>-aarch64-apple-darwin.tar.gz` | macOS ARM64 |
| `neenee-<ver>-x86_64-apple-darwin.tar.gz` | macOS x86-64 |

If an asset is missing, the `build` job for that target failed. Re-run
the failed job from the Actions UI; the artifacts are not published
until `publish` succeeds.

## Fixing common failures

These are the failure modes that most often block a release. All of them
make a gating job red and must be fixed before tagging.

| Symptom | Job | Fix |
|---------|-----|-----|
| Code formatting diff | `fmt` | `cargo fmt --all` on the affected crates |
| Lint error with a suggested rewrite | `clippy` | Apply the suggestion, or add a scoped `#[allow(...)]` with a comment when the rewrite changes behavior |
| `unresolved link` or `links to private item` | `doc` | Replace the intra-doc link `[`Foo`]` with plain code span `` `Foo` `` for narrative references, or fix the path for genuine API links |
| Test failure | `test` | Fix the code; never weaken an assertion to green a release |
| MSRV build error | `msrv` | A dependency or language feature exceeds `rust-version`. Pin the dependency or lower the feature usage |

When `clippy` suggests a rewrite that would change runtime behavior
(for example, replacing a manual counter with `enumerate`), do not apply
it blindly. Add a scoped `#[allow(clippy::...)]` with a one-line comment
explaining why the lint does not apply. The workspace `clippy.toml` pins
suggestions to the declared MSRV, so this table assumes the MSRV is
already in sync.
