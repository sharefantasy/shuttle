# Releasing Shuttle

Shuttle uses one release kernel with three platform front-ends. A stable
tag publishes four binary archives, two architecture-specific external-tool
bundles, and three multi-architecture images:

- `linux-amd64`
- `linux-arm64`
- `darwin-amd64`
- `darwin-arm64`
- `shuttle-core:<version>` for the SHUTTLE executable and non-root runtime only
- `shuttle-tools:<version>` for `/opt/shuttle-tools` only
- `shuttle:<version>` for the exact core-plus-tools composition; this is
  the backwards-compatible Debian full image (shell, CA store and HOME retained)
- `shuttle-tools-v<version>-linux-{amd64,arm64}.tar.zst` plus manifest
  and SHA-256 sidecars

Every platform Release also receives `SHA256SUMS`, CycloneDX 1.5 SBOMs,
`container-images.json`, and `release-manifest.json`. The manifest binds the
tag, source commit, archive digests, and the three distinct immutable image
digests. Each image entry also records its amd64 and arm64 child manifest
digests. The full entry binds the exact core and tools index digests used as
its build inputs.

## Release topology

GitHub Actions is the canonical binary builder because its hosted runners cover
both Linux architectures and both macOS architectures. Gitea Actions and Gitee
Go import those four archives from the configured canonical Release URL,
verify them against the canonical `SHA256SUMS`, build their own multi-arch
container image, then publish a local Release with a platform-local image
digest. This avoids pretending a Linux-only Gitee runner can produce native
Darwin binaries.

The common implementation lives in `release/`. Workflow YAML only binds tag,
repository, registry, and secret values:

- GitHub: `.github/workflows/release.yml`
- Gitea: `.gitea/workflows/release.yml`
- Gitee Go: `.workflow/release.yml`

## Repository setup

### GitHub

No custom Release token is required. The workflow uses `GITHUB_TOKEN` with
`contents: write` and `packages: write`, and publishes the image to
`ghcr.io/<owner>/<repo>`. Enable Actions read/write workflow permissions for
the repository. GitHub-hosted `macos-15-intel`, `macos-15`,
`ubuntu-24.04`, and `ubuntu-24.04-arm` runners must be available.

### Gitea

Run the workflow on a self-hosted Linux runner labelled `self-hosted`, `linux`,
and `docker`. It needs Docker Buildx, QEMU/binfmt, Git, curl, and Python 3.11+.
The selected Buildx builder must use a multi-platform-capable driver such as
`docker-container`; the legacy `docker` driver cannot push a two-platform
manifest unless the daemon's containerd image store is enabled.

Variables:

| Name | Meaning |
| --- | --- |
| `CANONICAL_RELEASE_BASE_URL` | Direct asset URL template, for example `https://github.com/OWNER/REPO/releases/download/{tag}` |
| `GITEA_CONTAINER_REGISTRY` | Registry host used by `docker login` |
| `GITEA_CONTAINER_IMAGE` | Full image path without a tag |

Secrets:

| Name | Meaning |
| --- | --- |
| `GITEA_RELEASE_TOKEN` | Token allowed to create/update Releases and attachments |
| `GITEA_CONTAINER_USERNAME` | Registry username |
| `GITEA_CONTAINER_PASSWORD` | Registry password or scoped token |
| `CANONICAL_RELEASE_TOKEN` | Optional bearer token when canonical assets are private |

### Gitee Go

Import `.workflow/release.yml` in the Gitee Go UI and bind it to a
Docker-capable host group. Its checked-in push branch filters are intentionally
empty; configure a `v*.*.*` tag trigger (or run it manually) in the UI. The
workflow deliberately requires `RELEASE_TAG` and `SOURCE_COMMIT` as explicit
variables because Gitee Go installations differ in which tag context variables
they expose. The execution environment needs Docker Buildx, QEMU/binfmt, Git,
curl, Python 3.11+, and Bash. Validate `make release-test` on a normal CI runner;
Gitee Go's release step runs only the dependency-minimal mirror kernel.
The selected Buildx builder must likewise support multi-platform pushes.

Variables:

| Name | Meaning |
| --- | --- |
| `RELEASE_TAG` | Existing stable tag, for example `v0.2.0` |
| `SOURCE_COMMIT` | Full commit OID referenced by the tag |
| `RELEASE_OWNER` | Gitee repository owner |
| `RELEASE_REPO` | Gitee repository name |
| `CANONICAL_RELEASE_BASE_URL` | Direct canonical asset URL template; `{tag}` is replaced at runtime |
| `CONTAINER_REGISTRY` | Registry host; this may be any OCI registry reachable from Gitee Go |
| `CONTAINER_IMAGE` | Full image path without a tag |

Secrets:

| Name | Meaning |
| --- | --- |
| `GITEE_TOKEN` | Token allowed to create/update Releases and attachments |
| `CONTAINER_USERNAME` | Registry username |
| `CONTAINER_PASSWORD` | Registry password or scoped token |
| `RELEASE_SOURCE_TOKEN` | Optional bearer token when canonical assets are private |

Do not put tokens in repository variables, command URLs, Docker build args, or
image labels. The scripts use Authorization headers and `docker login --password-stdin`.

## Release procedure

1. Update `[workspace.package].version`, crate versions, `Cargo.lock`,
   `CHANGELOG.md`, and `release/v<version>.md` in one commit.
2. Run `make release-gate` and `make release-test`. If a Docker daemon is
   available, also run `make docker-smoke`; never infer image sizes from static
   validation.
3. Create an annotated `v<version>` tag on that exact commit and push the
   commit plus tag to GitHub, Gitea, and Gitee.
4. Let GitHub complete first. Its Release becomes the canonical binary source.
   Mirror workflows retry unavailable canonical assets for up to 30 minutes, so
   tag-triggered Gitea runs may safely start in parallel.
5. Set each mirror's `CANONICAL_RELEASE_BASE_URL` once, using the `{tag}`
   placeholder, and run or rerun the Gitea/Gitee workflow.
6. Compare `release-manifest.json` across platforms. Binary archive digests must
   match; container digests may differ by registry, but every digest must be
   canonical `sha256:...` and resolvable in its registry.

The workflows are idempotent: rerunning a tag updates Release metadata and
replaces same-named attachments. Release scripts refuse a tag/version mismatch,
a tag/source-commit mismatch, missing target archives, duplicate assets, or a
checksum mismatch.

Release automation added after an existing tag does not rewrite that tag. It
takes effect on the next versioned tag; never move an already published tag to
pick up workflow changes.

## Local verification

```sh
make release-test
make docker-smoke
```

`make release-test` is Docker-free. It validates the bundle schema, deterministic
archive/verifier, negative security cases, size-exception policy, shell scripts,
and GitHub/Gitea/Gitee workflow wiring.

`make docker-smoke` builds and checks the host architecture for all three targets.
The immutable budgets are core 30 MiB, tools 60 MiB, and full 100 MiB. An
over-budget image fails unless `release/size-exceptions.json` contains exactly one
entry bound to the variant, platform, source commit, measured byte count, original
budget, and a non-empty reason. The budget itself is never raised to hide a
measurement. Multi-architecture publication and archive export remain Docker
Buildx operations in the release workflows.

The tools image and archive contain distro-sourced Git, ripgrep, Universal Ctags,
GNU tar, and procps under `/opt/shuttle-tools`, plus exact payload checksums, package
versions and binary archive hashes, upstream source archive URLs and hashes,
version and behavior probes, license notices, and a CycloneDX SBOM. Builds use
the checked-in Debian snapshot timestamp and validate direct package archives
before installing them. Release tooling exports the filesystem archives from
the already-published tools image, so image and archive consumers share one
payload.
They intentionally contain no Docker/container-use CLI, provider runtime, Node,
Pi, credentials, or writable configuration.
