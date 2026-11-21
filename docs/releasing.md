# Shipping ArgosFS

## One-click release

The workflow is Actions → **Release** → **Run workflow**, on **main**.
While AUR is under maintenance or the AUR account has not been provisioned,
uncheck **Publish AUR packages**. GitHub Releases and crates.io publishing
continue independently; the AUR step will be explicitly skipped. Re-enable
it on future releases once AUR access is ready.
Leave the optional version blank to use Cargo.toml's version. This workflow
intentionally does **not** change project versions: update Cargo.toml and
Cargo.lock in a reviewed PR before shipping a new version.

Each run enforces the following order:

1. Validate that the version is stable SemVer, is equal to Cargo.toml, and
   that the requested commit is the current main tip. A preexisting tag must
   point to the exact same commit.
2. Test the library, validate the vendored codec, package the crate, and
   repeat the erasure compatibility oracle from the packaged registry source.
3. Build x86_64 and aarch64 Linux binaries on native GitHub-hosted runners.
4. Generate one git-archive source tarball, SHA256SUMS, and a GitHub Release
   named vX.Y.Z. The tag is created only after all checks and builds pass.
5. Publish the crate using OIDC Trusted Publishing (or an initial bootstrap
   token), and update the two AUR packages.

The GitHub Release contains:
- argosfs-X.Y.Z-source.tar.gz: tagged Git source including pinned codec source.
- argosfs-X.Y.Z-x86_64-unknown-linux-gnu.tar.gz: Linux x86_64 binary.
- argosfs-X.Y.Z-aarch64-unknown-linux-gnu.tar.gz: Linux ARM64 binary.
- SHA256SUMS: the hashes of all three archives.

The binary archives also contain the license, README and optional deployment
integrations. These shared objects are **dynamically** linked to the system
(FUSE3/glibc): they are not static or standalone kernel modules.

A rerun uses the existing immutable tag and verifies the full published
release instead of overwriting release assets. The crates.io publication is
skipped only when that version already exists. AUR pushes are no-ops when the
package metadata has not changed. Failed downstream publishing jobs can be
rerun after fixing credentials without creating a different release version.
A new release is impossible while the current Cargo.toml version already has
an older, differently positioned Git tag.

## Required one-time external setup

### crates.io (first release)

1. Sign in to https://crates.io with your GitHub account, and create an API
   token with the minimal permissions needed for the initial crate.
2. Create a GitHub Actions repository secret called
   **CRATES_IO_BOOTSTRAP_TOKEN** containing the token. Do not commit it, post it
   in a PR, or paste it into issue comments.
3. Run the release workflow for the first *new* version. The initial release
   registers the previously unclaimed crate name **argosfs**.
4. On https://crates.io/crates/argosfs/settings, configure Trusted Publishing:
   - Owner: fwerkor
   - Repository: ArgosFS
   - Workflow filename: release.yml
   - GitHub environment: leave empty (no environment is configured)
5. Remove the CRATES_IO_BOOTSTRAP_TOKEN secret after the initial release.
   All future releases use rust-lang/crates-io-auth-action@v1 to obtain a
   short-lived OIDC token. Do not keep a long-lived registry token configured.

The crates.io archive uses the registry's reed-solomon-engine =0.2.0 (the
repository's pinned vendor copy is excluded by Cargo packaging); the release
verification runs the compatibility tests against the published dependency.

### AUR (once)

1. Register/log in to https://aur.archlinux.org, and add the dedicated SSH
   **public** key used for the release bot to the SSH Public Key field on
   the account page. Never upload the private key.
2. In GitHub → Settings → Secrets and variables → Actions, configure the
   repository secret **AUR_SSH_PRIVATE_KEY** with the corresponding private key.
   Prefer a dedicated unencrypted deploy key with no other account privileges.
3. Verify that the SSH key is accepted by the AUR account. The first workflow
   run creates two previously unclaimed AUR Git repositories with SSH pushes:
   **argosfs** and **argosfs-bin**.

The source package downloads the immutable release source archive, checks
its SHA-256, uses locked Cargo dependencies, and installs the actual binary.
The binary package downloads the native Linux release tarballs with
architecture-specific hashes; the two packages conflict so users choose one.
The workflow generates .SRCINFO with makepkg in an Arch Linux container.
Credential errors are hard failures; a green workflow never means an
unpublished AUR package.

AUR package release processing follows the GitHub Release. If a package name
is no longer available or an account cannot push, fix the AUR account or name
before rerunning the failed AUR job. The workflow does not force-push.

## Using Nix

The repository provides a version-aware flake with pinned nixpkgs inputs:

~~~bash
nix build github:fwerkor/ArgosFS
nix profile install github:fwerkor/ArgosFS
nix run github:fwerkor/ArgosFS -- --help
nix develop github:fwerkor/ArgosFS
~~~

The flake supports x86_64-linux and aarch64-linux, uses the tracked Cargo.lock,
and includes non-enabling systemd and initramfs integration files in
share/argosfs. It does not automatically configure or mount any raw devices.

## Local checks for release automation

~~~bash
python3 -m unittest discover -s scripts/release -p 'test_*.py' -v
python3 scripts/release/release.py check
cargo package --locked
nix flake check --no-build
nix build .#argosfs
~~~

## Security and failure recovery

- Dispatch only from main; no PR, arbitrary ref, or tag push can trigger
  credentialed publishing.
- Keep the GitHub Actions workflow file on main reviewed and protected; trusted
  publisher authorization applies to this workflow's identity.
- The AUR SSH key must only authorize the intended AUR account.
- Do not rewrite existing release tags or published release attachments;
  publish a new version instead.
- A published filesystem is privileged system software. Maintain a consistent
  on-disk compatibility policy, changelog, crash/recovery regression tests,
  and a separate verification of proposed new raw-block deployments.
- GitHub Releases, crates.io, and AUR are separate downstream destinations.
  If one fails, recover with the existing same-version workflow run; do not
  alter the already-published source commit or claim all channels succeeded.
