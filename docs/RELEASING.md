# Releasing

A release is made by pushing a version tag to a commit on `main`. The [release workflow](../.github/workflows/release.yml) builds the portable downloads and publishes them as a GitHub release; nothing is installed or signed beyond what's described below.

## Steps

1. On `dev`, set the new version in the workspace `Cargo.toml` (`[workspace.package] version`). It is the only place the version is kept: the app, its window, and the Windows file properties all take it from there.
2. Run `cargo check --workspace` so `Cargo.lock` picks up the version.
3. In `CHANGELOG.md`, move the entries under `## [Unreleased]` into a new `## [x.y.z] - YYYY-MM-DD` section. That section becomes the release notes; the release fails if it's missing.
4. Merge `dev` into `main` through a pull request.
5. Tag the merge commit on `main` and push the tag:

   ```sh
   git checkout main && git pull
   git tag v0.1.0
   git push origin v0.1.0
   ```

The workflow then:

- checks that the tag is `v` plus the `Cargo.toml` version and points at a commit on `main`;
- builds on Windows, Linux (Ubuntu 22.04), and macOS (Apple Silicon);
- packs each download with `LICENSE`, `README.md`, and `THIRD-PARTY-LICENSES.html`;
- publishes the release with the changelog section as notes, plus `SHA256SUMS`.

If a check fails, nothing is published. Delete the tag (`git push origin :refs/tags/v0.1.0`), fix the cause on `main`, and tag again.

Pull requests that change the release files (`release.yml`, `scripts/release/`, `packaging/`, `tauri.conf.json`) run the same builds without publishing, and keep the downloads as workflow artifacts.

## Downloads

| File | Platform | How to run |
|---|---|---|
| `SpaceBadger-x.y.z-windows-x64.zip` | Windows 10 and 11, x64 | Extract and run `SpaceBadger.exe`. Uses the WebView2 runtime that comes with Windows. |
| `SpaceBadger-x.y.z-linux-x86_64.AppImage` | Linux x86_64 with glibc 2.35 or newer | `chmod +x` and run. |
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
