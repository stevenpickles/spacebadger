# Releasing

Releases are cut on a release branch taken from `dev`. The branch is merged into `main`, the merge commit is tagged, and the branch is merged back into `dev`. The [release workflow](../.github/workflows/release.yml) builds and tests the downloads on every push to the release branch, and publishes them as a GitHub release when the tag is pushed. Nothing is installed or signed beyond what's described below.

The version is kept only in the workspace `Cargo.toml` (`[workspace.package] version`); the app, its window, and the Windows file properties take it from there. During development, `CHANGELOG.md` collects entries under `## [Unreleased]`.

## Steps

Shown for 0.1.0; use the new version throughout.

1. **Branch from `dev`:**

   ```sh
   git checkout dev && git pull
   git checkout -b release/v0.1.0
   ```

2. **Set the version and changelog:**

   ```sh
   scripts/release/bump.sh 0.1.0
   ```

   This sets the version in `Cargo.toml` and `Cargo.lock`, and moves the Unreleased entries into a `## [0.1.0] - <today>` section, which becomes the release notes. Edit the section if needed, then commit and push:

   ```sh
   git commit -am "Prepare 0.1.0"
   git push -u origin release/v0.1.0
   ```

3. **Test.** Every push to the branch runs CI and the release workflow without publishing:
   - the branch name has to match the version, and the changelog section has to exist;
   - the Windows, Linux, and macOS downloads are built and attached to the workflow run as artifacts (kept 90 days by default), so they can be tried before release.

   Release-only fixes (changelog wording, packaging, last bug fixes) are committed on the release branch.

4. **Merge into `main`.** Open a pull request from `release/v0.1.0` into `main` and merge it with a **merge commit**, not squash or rebase, so the same commits later merge cleanly into `dev`.

5. **Tag the merge commit:**

   ```sh
   git checkout main && git pull
   git tag v0.1.0
   git push origin v0.1.0
   ```

   The workflow checks that the tag is `v` plus the `Cargo.toml` version and points at a commit on `main`, builds on Windows, Linux (Ubuntu 22.04), and macOS (Apple Silicon), packs each download with `LICENSE`, `README.md`, and `THIRD-PARTY-LICENSES.html`, and publishes the release with the changelog section as notes, plus `SHA256SUMS`.

   If a check fails, nothing is published. Delete the tag (`git push origin :refs/tags/v0.1.0`), fix the cause on the release branch, merge it into `main` again, and re-tag.

6. **Merge back into `dev`.** Open a pull request from `release/v0.1.0` into `dev`, again with a merge commit, so the version, changelog, and any fixes made on the release branch reach `dev`. Then delete the release branch.

Pull requests from other branches that change the release files (`release.yml`, `scripts/release/`, `packaging/`, `tauri.conf.json`) also build the downloads without publishing.

## Downloads

| File | Platform | How to run |
|---|---|---|
| `SpaceBadger-x.y.z-windows-x64.zip` | Windows 10 and 11, x64 | Extract and run `SpaceBadger.exe`. Uses the WebView2 runtime that comes with Windows. |
| `SpaceBadger-x.y.z-linux-x86_64.AppImage` | Linux x86_64 with glibc 2.35 or newer (Ubuntu 22.04, Debian 12, Fedora 36, and later) | `chmod +x` and run. Uses the system's graphics libraries (`libEGL`), which every desktop has. |
| `SpaceBadger-x.y.z-macos-arm64.zip` | macOS 11 or newer, Apple Silicon | Extract and open `SpaceBadger.app`. |
| `THIRD-PARTY-LICENSES.html` | | Licenses of the bundled Rust crates and npm packages. |
| `SHA256SUMS` | | Checksums of the files above. |

The downloads aren't code-signed. Windows SmartScreen may warn on first run ("More info" → "Run anyway"). The macOS app is only ad-hoc signed and not notarized, so macOS refuses to open it the first time: Control-click it, choose Open, and confirm.

## Building a download locally

```sh
npm ci
scripts/release/notices.sh          # optional; needs cargo-about
npx tauri build --no-bundle         # Windows
npx tauri build                     # Linux and macOS
scripts/release/package.sh windows  # or linux, macos
```

The result is in `dist/`.
