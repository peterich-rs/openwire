# Release Process

OpenWire publishes the user-facing crates through crates.io so downstream users
can depend on standard registry versions instead of Git source paths. The next
planned release is `0.1.1`.

## Published Crates

Publish the workspace crates in dependency order:

1. `openwire-core`
2. `openwire-tokio`
3. `openwire-rustls`
4. `openwire`
5. `openwire-cache`
6. `openwire-fastwebsockets`
7. `openwire-tungstenite`

`openwire-test` is local test support and has `publish = false`. Published
crate dev-dependencies that point at `openwire-test` remain path-only so Cargo
does not emit a registry dependency on a non-published crate.

## Versioning

All publishable crates use the workspace version from the root
`[workspace.package]` table. The same version must also be set on the internal
workspace dependencies in root `[workspace.dependencies]` so packaged manifests
can resolve each OpenWire crate from crates.io after publication.

For a version bump:

1. Update `version` in root `[workspace.package]`.
2. Update the `version = "..."`
   fields for `openwire`, `openwire-cache`, `openwire-core`,
   `openwire-fastwebsockets`, `openwire-rustls`, `openwire-tokio`, and
   `openwire-tungstenite` in root `[workspace.dependencies]`.
3. Update README installation examples and any release notes that name the
   version.
4. Run `scripts/publish-crates.sh --dry-run` to verify that package metadata and
   internal dependency versions are consistent.

## Pull Request Preflight

Run the normal CI gates before publishing:

```sh
cargo fmt --all --check
cargo clippy --workspace --all-targets -- -D warnings
cargo test --workspace --all-targets
scripts/publish-crates.sh --dry-run
```

The dry-run script is intentionally first-release safe. Cargo requires each
versioned internal dependency to already exist in the registry when generating a
full tarball for dependent crates, so before a staged release it checks package
file lists for every publishable crate and builds the `openwire-core` tarball.
The actual publish command still runs Cargo's full publish verification for each
crate after its dependencies have been published.

## Publishing

Set the repository secret `CRATES_IO_TOKEN` to a crates.io API token with
publish permission for all published OpenWire crates.

After the release PR is merged:

1. Create and push a release tag that matches the workspace version:

   ```sh
   git tag v0.1.1
   git push origin v0.1.1
   ```

2. Pushing the `v0.1.1` tag automatically runs the `Publish Crates` GitHub
   Actions workflow as a real publish.

Create the release tag only after the release PR has merged. If the intended tag
already exists on an older commit, delete and recreate it on the release commit
or choose a new version instead.

For tag pushes, the workflow derives the publish version from
`refs/tags/v<version>` and refuses to publish if it does not match the workspace
version. It publishes crates in dependency order and waits for each version to
become visible through Cargo before publishing the next dependent crate.

The same workflow can still be run manually with `dry_run=true` on a branch or
tag to execute the package preflight without requiring a crates.io token.

## Resuming a Partial Publish

crates.io rate-limits publishing many brand-new crate names in a short window.
If a staged publish fails after some crates have already uploaded, wait until
the time reported by crates.io and rerun `Publish Crates` from `main` with:

- `version` set to the same workspace version
- `dry_run=false`
- `start_at` set to the first crate that did not publish

For example, if the first five `0.1.1` crates published and the rate limit
stopped at `openwire-fastwebsockets`, rerun with
`start_at=openwire-fastwebsockets`. The workflow only allows non-tag publishing
when `start_at` is set and the ref is `main`.

## Post-Publish Verification

After the workflow finishes, verify the registry and downstream install path:

```sh
cargo info openwire@0.1.1
tmpdir="$(mktemp -d)"
cd "$tmpdir"
cargo init --bin
cargo add openwire@0.1.1
cargo check
```
