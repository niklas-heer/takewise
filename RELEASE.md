# Releasing Takewise

Releases are prepared on the Mac that holds the Developer ID certificate, published to this repository's GitHub Releases by hand, and installed through the cask in `niklas-heer/homebrew-tap`. Nothing in CI signs or publishes.

## Prerequisites

- The pinned Rust toolchain (`mise install`) and Xcode or the Command Line Tools (`codesign`, `notarytool`, `stapler`).
- The certificate "Developer ID Application: Niklas Heer (WL5ASJ82YX)" in the login Keychain. The first signing may ask to allow `codesign` to use its key.
- The `notarytool` Keychain profile `keywink-notary`, shared with Keywink, Spokn and Focal (`xcrun notarytool store-credentials keywink-notary` creates it).

Takewise has no update feed: Homebrew updates it (`brew upgrade --cask takewise`).

## Steps

1. Start from a clean checkout of `main`. Set the version in `crates/takewise/Cargo.toml` if it changes, run `mise run check`, and commit. Pick a build number higher than the last release's (the second part of the cask's `version`).

2. Prepare a signed dry run, which never leaves the Mac:

   ```sh
   TAKEWISE_BUILD=2 scripts/release
   ```

   It builds `build/Takewise.app` with the hardened runtime and the microphone entitlement (`packaging/Takewise.entitlements`), checks the bundle identifier, the build number, `codesign --verify --deep --strict`, the entitlement and the hardened runtime, and writes the ZIP and its checksum to `build/release`. `TAKEWISE_SIGN_IDENTITY=-` runs the same steps signed ad hoc, without the certificate; such artifacts are never published.

3. Prepare the real artifacts. This submits the app to Apple's notary service, staples the ticket and writes the release files:

   ```sh
   rm -rf build/release
   TAKEWISE_BUILD=2 scripts/release --notarize
   ```

   ```text
   build/release/Takewise-<version>-<build>.zip
   build/release/Takewise-<version>-<build>.zip.sha256
   ```

4. Tag, push and publish:

   ```sh
   git tag -a v0.1.0 -m "Takewise 0.1.0"
   git push origin v0.1.0
   gh release create v0.1.0 build/release/Takewise-0.1.0-2.zip build/release/Takewise-0.1.0-2.zip.sha256 --title "Takewise 0.1.0" --notes-file <notes>
   ```

5. Update the Homebrew cask: copy `packaging/takewise.rb` to `Casks/takewise.rb` in `niklas-heer/homebrew-tap` with the new `version` (`"<version>,<build>"`) and `sha256`, and open a pull request (the tap's `main` accepts no direct pushes). The cask depends on the `whisper-cpp` and `ffmpeg` formulae, links `takewise` onto `PATH` with its `binary` stanza, and its caveats point to `takewise models install`.

6. Verify on a clean account or Mac: `brew install --cask niklas-heer/tap/takewise`, then `spctl --assess --type execute /Applications/Takewise.app`, `takewise models install`, `takewise doctor`, and one take in the recording window (the first take asks for the microphone).

## Local builds

`mise run bundle` builds `build/Takewise.app` signed ad hoc; macOS then asks for the microphone again after every rebuild. Set `TAKEWISE_SIGN_IDENTITY` to the Developer ID to keep the grant. Without Homebrew, the app's **Takewise › Install Command Line Tool…** links `~/.local/bin/takewise` to the binary inside the bundle; it never replaces a file that is not a link to a Takewise bundle. `TAKEWISE_LINK_DIR=/tmp/bin` makes it link there instead.

The speech model is not bundled: `takewise models install` downloads `ggml-large-v3-turbo.bin` (1.5 GB) from Hugging Face and checks the SHA-256 pinned in `crates/takewise-core/src/models.rs`. When whisper.cpp's repository replaces the file, update the URL, size and hash there together, from the repository's file listing (`https://huggingface.co/api/models/ggerganov/whisper.cpp/tree/main`, the `lfs.oid` of the file) and a downloaded copy.
