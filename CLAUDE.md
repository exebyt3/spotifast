# exebyt3/spotifast fork

Read `AGENTS.md` first: upstream's rules apply here too. This file adds the
fork's own rules.

## Builds run on GitHub Actions, never on the owner's PC

Do not run `cargo build`, `cargo test` or `cargo run` for this repository on
the local machine: a full build ties up the owner's PC. Push and let Actions
do it:

- Every push runs `.github/workflows/fork-ci.yml` (Linux tests). Check it with
  `gh run list -R exebyt3/spotifast` / `gh run watch`.
- A release: merge to `main`, push, create the `fork-vX.Y.Z-N` tag and the
  GitHub release (`gh release create`), then dispatch
  `gh workflow run fork-release.yml -R exebyt3/spotifast -f tag=fork-vX.Y.Z-N`.
  It attaches the Linux tarball and the Windows zip and installer.
- To install a build locally, download the release's Linux tarball and put
  its `spotifast` binary at `~/.local/lib/spotifast/spotifast`
  (`~/.local/bin/spotifast` links to it).

`cargo update`, `cargo metadata` and other commands that only resolve the
lockfile are fine locally.

## librespot

The fork pins librespot to `exebyt3/librespot` when it needs a fix that
`crmne/librespot` spotifast-dev does not carry yet; `Cargo.toml` says which.
Go back to crmne's revision once it carries the fix.
