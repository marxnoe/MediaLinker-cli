# MediaLinker-cli

`ml` shares files and text through a [MediaLinker](https://github.com/marxnoe/MediaLinker)
server. Every share gets a 5-character code that works until it expires.

## Install

macOS and Linux:

```sh
curl --proto '=https' --tlsv1.2 -LsSf https://github.com/marxnoe/MediaLinker-cli/releases/latest/download/medialinker-cli-installer.sh | sh
```

Windows (PowerShell):

```powershell
powershell -ExecutionPolicy Bypass -c "irm https://github.com/marxnoe/MediaLinker-cli/releases/latest/download/medialinker-cli-installer.ps1 | iex"
```

With Cargo:

```sh
cargo binstall medialinker-cli   # prebuilt binary, needs cargo-binstall
cargo install medialinker-cli    # build from source
```

Prebuilt archives for Linux (x86_64, ARM64), macOS (Intel, Apple Silicon) and Windows are on the
[releases page](https://github.com/marxnoe/MediaLinker-cli/releases).

## Server

`ml` talks to https://medialinker.noecore.com by default. To use another MediaLinker server, save it once:

```sh
ml server https://media.example.com
```

`--server <URL>` or the `ML_SERVER` environment variable override the saved server.

## Usage

```sh
ml send photo.jpg            # share a file, prints the code
ml send "meet at 6"          # share text
ml send notes.txt --text     # share the words "notes.txt", not the file
cat log.txt | ml send        # share stdin as text
tar cz dir | ml send -n dir.tgz   # share stdin as a file
ml send video.mp4 --ttl 7d   # expiry: 1h, 24h (default) or 7d

ml get K7QX2                 # prints text, or saves a file in the current directory
ml get K7QX2 -o out.jpg      # save under another name (-o - writes to stdout)
```

`ml get` never overwrites an existing file unless you pass `--force`.

## Releasing

Releases are built by [cargo-dist](https://opensource.axo.dev/cargo-dist/). Bump `version` in
`Cargo.toml`, merge it, then push a matching tag:

```sh
git tag v0.2.0 && git push origin v0.2.0
```

The release workflow builds every target, publishes the archives and installers to a GitHub release,
then publishes the crate to crates.io (needs the `CARGO_REGISTRY_TOKEN` repo secret).
