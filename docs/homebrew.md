# Homebrew distribution

The release workflow follows the existing `GordonBeeming/homebrew-tap` pattern: a dedicated repository deploy key updates a formula with a separately signed Git commit. It changes only `Formula/sprig.rb` in the tap. Source builds and native tests run before publication.

After the first release is published:

```sh
brew install gordonbeeming/tap/sprig
tool --help
```

The initial package is a prebuilt Apple Silicon executable for macOS 26 or newer. It installs the provisional executable name `tool`; Intel and Linux packages are not published. Homebrew installs Git as a dependency. Users do not need Rust or Xcode to install the packaged binary. The executable has an ad-hoc signature, not Developer ID notarization.

## Credentials

- `HOMEBREW_TAP_DEPLOY_KEY`: a dedicated Ed25519 write key registered on `GordonBeeming/homebrew-tap`. Its private key is stored in the `ai-secrets` item **GitHub Deploy Key - sprig Homebrew tap (OpenSSH)** and the same-named Actions secret in `GordonBeeming/sprig`.
- `COMMIT_SIGNING_KEY`: the existing Actions signing key from the `ai-secrets` item **GitHub Commit Signing Key - Actions (OpenSSH)**, exposed only to the tap publication step.
- Release uploads use the job-scoped GitHub token. No personal token is needed by the workflow.

Private keys are never included in source, release archives or formula files. Tap SSH authentication pins GitHub's Ed25519 host key and disables prompts. Release jobs have a single concurrency group. The publisher uses a `gb/chore/...` branch, signed commits and a normal fast-forward push, so concurrent tap changes fail safely.

## Publishing

Commit and review the implementation and release setup first. Set the Cargo package version, then explicitly create and push its matching `vMAJOR.MINOR.PATCH` tag when authorising a release. This setup does not itself publish the uncommitted implementation.

The workflow checks the tag against `Cargo.toml` and requires its commit to be on `main`, runs formatting/lint/tests, builds the arm64 executable, signs it ad hoc, and generates its archive, SHA-256 manifest and formula. It creates a draft release, uploads and downloads the complete payload for comparison, then publishes it before updating the tap. The formula checksum is computed from the actual archive. Published assets are never overwritten. An incomplete draft can be repaired only when its recorded payload marker matches the original package artifact. The tap publisher rejects version downgrades.

If only tap publication fails, rerun the failed publication job using its original package artifact. A concurrent tap update causes a safe push failure; retry from the latest tap state. Rebuilding the whole release may produce different archive metadata, so the workflow refuses to replace an existing release with different bytes.

The formula generator and shell/workflow validation can run locally without publishing. A live Homebrew download/install test requires the first tagged release and tap update; the install command above is not available until then. No tap formula or release was published as part of preparing this setup.
