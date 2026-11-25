# Release Process

OpenWire publishes its user-facing crates through crates.io. Each crate declares
its version in its own `Cargo.toml`. `openwire-test` is local test support with
`publish = false`. Its path-only dev-dependencies are omitted by Cargo when it
packages the published crates.

## Prepare a release PR

1. Bump the crates whose published code or dependency requirements changed.
   Keep the versions of compatible crates that can continue using their published
   packages.
2. Update minimum dependency versions where a crate uses newer APIs or behavior.
   Shared requirements in `[workspace.dependencies]` are defaults. A member can
   declare a higher minimum together with its local `path`. Cargo resolves the
   local path in the workspace and uses the version requirement on crates.io.
3. Update `release.json` with the release identifier and the exact versions of
   the crates to publish. The identifier determines the Git tag. It belongs to
   the release plan and can name a batch of different crate versions.
4. Refresh `Cargo.lock`, README installation examples, and release notes.

The `v0.1.3` plan publishes `openwire-core`, `openwire-tokio`, `openwire`, and
`openwire-cache` as `0.1.3`. `openwire-rustls`, `openwire-fastwebsockets`, and
`openwire-tungstenite` remain available as `0.1.2`.

## Verify the release PR

Use current stable Cargo and Python 3. Run the CI checks and the release preflight
from a clean commit.

```sh
cargo fmt --all --check
cargo clippy --locked --workspace --all-targets -- -D warnings
cargo test --locked --workspace --all-targets
cargo test --locked --workspace --all-targets --all-features
python3 -B -m unittest discover -s scripts -p 'test_publish_crates.py'
scripts/publish-crates.sh --dry-run --release 0.1.3
```

The preflight checks the release identifier and each selected crate version,
then asks Cargo to package and compile the selected crates with all features.
Cargo stages unpublished dependencies from the same batch in a temporary
registry and resolves other dependencies from crates.io. The preflight requires
no publish token. CI uses the current plan in `release.json` automatically.

## Publish

Configure the repository secret `CRATES_IO_TOKEN` with permission to publish
the selected crates. After the release PR merges and CI passes, use the SHA
of the merged release commit in place of `RELEASE_COMMIT_SHA`.

```sh
git fetch origin
git tag -a v0.1.3 RELEASE_COMMIT_SHA -m "OpenWire v0.1.3"
git push origin refs/tags/v0.1.3
```

Pushing the matching tag starts the real `Publish Crates` workflow. The job
validates the plan in that commit, checks the crates.io index for each version,
and passes only pending packages to Cargo. Cargo publishes the batch in
dependency order. The job also checks that each new version becomes visible.

The workflow can be run manually with `dry_run=true` and a `version` matching the
plan on the selected branch or tag. A manual publish requires the matching tag.

## Resume a partial publish

Publishing several crates is incremental. If the workflow fails after some
uploads, resolve the error and rerun the same tag workflow. After rate limiting,
wait until the time reported by crates.io. The rerun skips the exact versions
already visible, then publishes the remaining crates. A yanked version or a
registry lookup failure stops the run with an error.

## Verify published packages

Confirm the four selected versions in the registry, then use an empty directory
to check a downstream install of the new client with its companion crates.

```sh
for crate in openwire-core openwire-tokio openwire openwire-cache; do
  cargo info "${crate}@0.1.3"
done
release_check_dir="$(mktemp -d)"
cargo init --bin --name openwire-release-check "$release_check_dir"
cd "$release_check_dir"
cargo add 'openwire@=0.1.3' 'openwire-cache@=0.1.3' 'openwire-tungstenite@=0.1.2'
cargo check
```
