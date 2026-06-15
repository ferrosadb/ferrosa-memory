# Release Process & Channels

How ferrosa-memory versions, tags, builds, and ships releases. Mirrors the
ferrosa engine's model (see `ferrosa/specs/release-process.md`).

## TL;DR

- **Versioning is automatic.** The release job derives the next SemVer from
  Conventional Commit history (`.github/scripts/next-release-version.sh`). **Do
  not hand-edit `[workspace.package] version` in `Cargo.toml` in a PR** — it is
  owned by the release automation and overwritten.
- **Releases cut on merge.** Every push to `main` (a merged PR) that carries a
  releasable commit cuts the next release automatically. A **nightly cron** runs
  as a safety-net. Both share `nightly-release.yml` and the tag-only mechanism.
  Doc/spec/CI-only merges are excluded (`paths-ignore`).
- **Two channels:**
  - **nightly** — every auto-cut `vX.Y.Z` release, marked a GitHub *prerelease*.
  - **stable** — a nightly release a maintainer *promoted* (`promote-release.yml`),
    marked *latest*, not prerelease.
- **Releases are tag-only.** `main` has a ruleset (`pull_request` +
  non-fast-forward) that rejects direct branch pushes (`GH013`); the version-bump
  commit is captured by the **tag** and never pushed to `main`. `release.yml`
  builds from the tag, so the artifact always carries the correct version.

## Pipeline

```
nightly-release.yml  (on: push→main [merge], cron 08:17 UTC, or manual)
  └─ next-release-version.sh        # SemVer from Conventional Commits since last vX.Y.Z tag
  └─ should_release? (commits since last tag) — else skip
  └─ bump Cargo.toml + commit (local only)
  └─ git tag vX.Y.Z  →  git push origin vX.Y.Z      # tag only, never main
  └─ gh workflow run release.yml -f prerelease=true # explicit: GITHUB_TOKEN tag
                                                    # pushes don't trigger on:push
release.yml  (per built tag, or manual dispatch)
  └─ build musl x86_64/aarch64 + macOS aarch64 tarballs for ferrosa-memory-mcp
  └─ SHA256SUMS
  └─ gh release create … --prerelease   # nightly channel by default
```

Conventional Commit → bump: `feat!`/`BREAKING CHANGE`→major, `feat`→minor,
anything else→patch. Non-conventional subjects degrade to **patch**.

## Promoting nightly → stable

- **UI:** Actions → *Promote Release to Stable* → enter the tag.
- **CLI:** `gh release edit vX.Y.Z --repo ferrosadb/ferrosa-memory --prerelease=false --latest`

## Runbook — cut the next 0.15 patch release

Use this path when the current stable channel should stay on the 0.15 line even
though unreleased commits contain `feat(...)` subjects that would make the
automatic release script choose `v0.16.0`.

1. Confirm CI is green on the release candidate PR and merge it to `main`.
2. Fetch tags and verify the current 0.15 base:
   `git fetch --tags origin && git tag --sort=-v:refname | head`.
3. Dry-run the patch calculation from a checkout that has current tags:
   `bash .github/scripts/next-release-version.sh patch false`.
   As of the `v0.15.2` base, this should report `next_tag=v0.15.3`.
4. In GitHub Actions, run *Release (on-merge + nightly)* manually with:
   `bump=patch`, `force=false`.
5. Wait for the dispatched *Release* workflow to publish the new prerelease.
   Verify the release assets include all target tarballs plus `SHA256SUMS`.
6. Promote the validated tag to stable with *Promote Release to Stable*, or:
   `gh release edit v0.15.3 --repo ferrosadb/ferrosa-memory --prerelease=false --latest`.

Do not re-tag or mutate an existing `v0.15.x` tag. If `v0.15.3` already exists
by the time this runbook is used, re-run step 3 and use the next computed patch
tag.

## Runbook — the release failed

1. `gh run list --workflow=nightly-release.yml` → open the failed run.
2. `GH013 … refs/heads/main` → the ruleset rejected a branch push; the workflow
   must push **tags only** (no `git push origin HEAD:main`).
3. `computed tag … already exists` → delete the stray tag or bump past it.
4. Version came out `patch` when a `feat` landed → a PR used non-Conventional
   commit subjects. Fix history hygiene.
