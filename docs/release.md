# Release setup

The public repository is bitomule/bazelqueue. CI and all validation jobs require
no private key. Publication needs exactly these repository Actions secrets:

| Secret | Purpose | Minimum repository access |
| --- | --- | --- |
| RELEASE_PLZ_TOKEN | Release PRs and unsigned version tags that trigger the release workflow | bazelqueue: contents and pull requests read/write; workflow permission where required by the selected token type |
| HOMEBREW_TAP_TOKEN | Commit the isolated formula update | homebrew-tap: contents read/write |

No crates.io token, Apple signing credential or SSH key is required. Add both in
GitHub repository Settings → Secrets and variables → Actions. Do not put them in
files, source, workflow literals or command output.

For the initial release, binaries are published using the built-in GITHUB_TOKEN.
After adding the secrets, dispatch Release binaries and Homebrew with tag `v0.1.0`
to publish the prepared formula from those same immutable bytes. For later versions,
run Release preparation via workflow_dispatch and review/merge its release PR. release-plz uses tags (git_only=true)
and does not publish crates. cargo-dist builds the production ARM64 archive on
GitHub-hosted macOS. The release workflow verifies the existing tag's commit,
validates code/package contents, publishes immutable archives/checksums, then
updates Formula/bazelqueue.rb on bitomule/homebrew-tap master.

For a failed tap publication, dispatch Release binaries and Homebrew with the
existing tag. It downloads and verifies existing bytes instead of rebuilding or
replacing them. An older tag cannot downgrade a newer formula. Prereleases are
marked prerelease/non-latest and do not replace the stable tap formula.

Missing credentials produce an explicit waiting/skipped publication step. They
never change the result of Rust or native-contract CI. Initial tap publication is pending HOMEBREW_TAP_TOKEN; automatic future release
PRs require RELEASE_PLZ_TOKEN. The package generator and formula template are
validated in CI before publication.

The artifacts are not claimed to be notarized. Local Apple identities are never
exported to CI. Only ARM64 macOS is published in this release.
