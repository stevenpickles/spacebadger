# Releasing

Releases are cut on a release branch taken from `dev`. The branch is merged into `main`, the merge commit is tagged, and the branch is merged back into `dev`. The [release workflow](../.github/workflows/release.yml) builds and tests the downloads on every push to the release branch, and publishes them as a GitHub release when the tag is pushed. Nothing is installed or signed beyond what's described below.

The version is kept only in the workspace `Cargo.toml` (`[workspace.package] version`); the app, its window, and the Windows file properties take it from there. The window title shows `v0.1.0` only for a build of the commit tagged `v0.1.0` with no uncommitted changes; any other build shows `v0.1.0+dev.<commit>` (plus `.dirty` with uncommitted changes), so a release branch reads as a development build until it is tagged. During development, `CHANGELOG.md` collects entries under `## [Unreleased]`.

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

   The workflow checks that the tag is `v` plus the `Cargo.toml` version and points at a commit on `main`, builds on Windows, Linux (Ubuntu 22.04), and macOS (Apple Silicon), packs each download with `LICENSE`, `README.md`, and `THIRD-PARTY-LICENSES.html`, and publishes the release with the changelog section as notes, plus `SHA256SUMS`. It then builds the winget and Chocolatey packages and submits them when their secrets are set; see [Package managers](#package-managers).

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

## Package managers

The Windows zip is also offered through winget (`StevenPickles.SpaceBadger`) and Chocolatey (`spacebadger`). Both install the same portable `SpaceBadger.exe` and add a `spacebadger` command; nothing else is installed.

| Path | Contents |
|---|---|
| `packaging/winget/template/` | winget manifests (schema 1.10.0) with placeholders |
| `packaging/winget/manifests/s/StevenPickles/SpaceBadger/<version>/` | Generated manifests, in the layout of `microsoft/winget-pkgs` |
| `packaging/chocolatey/template/` | Chocolatey `.nuspec` and install script with placeholders |
| `packaging/chocolatey/spacebadger/` | Generated package for the latest release |
| `scripts/release/winget.sh`, `chocolatey.sh` | Fill in the version, download URL, and SHA-256 from the published release's `SHA256SUMS` |

The Chocolatey package downloads the zip when it's installed instead of embedding it, so it needs no `VERIFICATION.txt` or `LICENSE.txt`.

After a tag publishes a release, the release workflow runs the [packages workflow](../.github/workflows/packages.yml), which regenerates both, attaches them to the run (`winget-manifests`, `chocolatey-package`), and submits each one whose secret is set:

| Secret | Used for |
|---|---|
| `WINGET_TOKEN` | A pull request to `microsoft/winget-pkgs` with `wingetcreate submit`. A classic personal access token with the `public_repo` scope; wingetcreate forks `winget-pkgs` into that account. |
| `CHOCO_API_KEY` | `choco push` to the Chocolatey community repository. The API key from your chocolatey.org account. |

Without a secret, that step logs a notice and is skipped, so the workflow only builds. To run it for an existing release, use **Actions → Packages → Run workflow** and enter the version. Commit the regenerated files on the release branch or on `dev` afterwards, so the repository matches what was submitted:

```sh
scripts/release/winget.sh 0.2.0
scripts/release/chocolatey.sh 0.2.0
winget validate --manifest packaging/winget/manifests/s/StevenPickles/SpaceBadger/0.2.0
choco pack packaging/chocolatey/spacebadger/spacebadger.nuspec --output-directory dist
```

### First submission

The first version of a package is reviewed by hand in both repositories, so submit it once yourself, then let the workflow handle later versions.

1. **winget.** Fork [`microsoft/winget-pkgs`](https://github.com/microsoft/winget-pkgs), copy `packaging/winget/manifests/s/StevenPickles/SpaceBadger/0.2.0/` to the same path in the fork, and open a pull request titled `New package: StevenPickles.SpaceBadger version 0.2.0`. Its bots validate the manifests and test-install the app; a moderator merges it. (Alternatively, with [wingetcreate](https://github.com/microsoft/winget-create) installed: `wingetcreate submit --token <token> packaging/winget/manifests/s/StevenPickles/SpaceBadger/0.2.0`.)
2. **Chocolatey.** Create an account at [community.chocolatey.org](https://community.chocolatey.org/account/Register), confirm the email, and copy the API key from the account page. Then:

   ```sh
   choco pack packaging/chocolatey/spacebadger/spacebadger.nuspec --output-directory dist
   choco push dist/spacebadger.0.2.0.nupkg --source https://push.chocolatey.org/ --api-key <key>
   ```

   The package goes through automated validation and verification, then moderation, which can take days for a new package. Answer any comments on the package page.
3. **Secrets.** In the repository's **Settings → Secrets and variables → Actions**, add `WINGET_TOKEN` and `CHOCO_API_KEY` as above. Later releases are then submitted automatically. Update pull requests to winget-pkgs are usually merged by its bots once validation passes.

Both registries keep the version once accepted: re-running the workflow for a version that's already submitted opens a duplicate winget pull request and fails the Chocolatey push.

## Building a download locally

```sh
npm ci
scripts/release/notices.sh          # optional; needs cargo-about
npx tauri build --no-bundle         # Windows
npx tauri build                     # Linux and macOS
scripts/release/package.sh windows  # or linux, macos
```

The result is in `dist/`.
