# Releasing

A release is a git tag, `vX.Y.Z`, and consumers pin it (`tag = "vX.Y.Z"`). The Release workflow tags a commit that is
already on `main` and has passed CI, and writes the GitHub Release notes from the commits since the previous tag. It
commits nothing, so the tag, not the `version` in Cargo.toml, is the release version. Choose patch, minor or major;
the first release takes the version in Cargo.toml (0.1.0).

```sh
gh workflow run release.yml -R microscaler/connecttrade-rs -f bump=patch
```

`-f ref=<sha>` releases an earlier green commit on `main` (it must come after the newest release);
`-f version=X.Y.Z` sets the version outright.

The tag also starts the Octopilot pipeline's release job, which promotes the ct-mock image that run tested, by digest,
to `ghcr.io/microscaler/ct-mock:vX.Y.Z`.

Consumers:

```toml
connecttrade = { git = "https://github.com/microscaler/connecttrade-rs.git", tag = "vX.Y.Z" }
```
