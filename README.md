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

In a terminal, `ml send` also prints a QR code and a link to the receive page, so a phone can open the
share straight away. `--no-qr` leaves the QR code out; nothing extra is printed when stderr is not a
terminal, so `CODE=$(ml send file)` stays clean.

## End-to-end encryption

Encryption is off unless you ask for it. With `--encrypt` (`-e`), `ml` encrypts the share locally
with AES-256-GCM and a fresh key before uploading, so the server stores only ciphertext and never
sees the key, the file name or whether it is text. `ml send -e` prints `CODE.KEY` instead of the
bare code, and its link and QR code carry the key after the `#`, which browsers never send.

```sh
ml send -e taxes.pdf                       # prints K7QX2.3q9Vh…
ml get K7QX2.3q9Vh…                        # decrypt with the key
ml get 'https://…/?code=K7QX2#k=3q9Vh…'    # or the whole link from the web page
ml get K7QX2 --key 3q9Vh…                  # or the key on its own
```

Shares encrypted on the web page open with `ml get`, and the other way round. The code alone can't
open an encrypted share.

## Shared clipboard

`ml clip sync` keeps one clipboard on your devices for as long as it runs. Copy text, an image, or
files and folders on one computer and paste them on the others. Copied files arrive as real files,
so Ctrl+V in Explorer, Finder or a Linux file manager pastes them.

```sh
ml clip sync                 # first device: starts a room, prints a code like K7X-42PQ and a QR code
ml clip sync K7X-42PQ        # every other device joins with the code
```

Press Enter in a running `ml clip sync` to show a code for one more device.

- **One session, nothing saved.** The room key lives only in memory. A computer that stops syncing
  (Ctrl+C, closing the terminal, a restart or shutdown) leaves the room, and the others carry on.
  The room ends once every device has left, and you pair again next time. Sleep and network drops
  don't count as leaving: devices reconnect by themselves. A device that vanishes without leaving
  (a phone that killed the browser tab, say) stops holding the room open once it has been away for
  a day and the others have left.
- **No account.** A pairing code works once, for 10 minutes.
- **End-to-end encrypted.** The code authenticates a key exchange (CPace), so the devices agree on a
  room key the server never sees, and a guessed code gets one try, not an offline attack. Every clip
  is sealed with AES-256-GCM before it leaves the device.
- **Text, images, files and folders**, up to 100 MB. Several files or a folder travel as one zip
  and are unpacked on the other side. Received files are kept in the cache folder
  (`~/.cache/medialinker/clip` on Linux) for the last few clips.
- **Newest copy wins.** When two devices copy at once, every device ends up with the later one, and a
  clip never bounces back and forth. A large file only replaces the clipboard once it has fully
  arrived; until then Ctrl+V pastes what was there before.
- **Phones** can't watch the clipboard in the background. Scan the QR code, or open the code on the
  MediaLinker page: it shows the latest clip with a Copy button, and Paste and Send a file buttons to
  send from the phone. Closing the tab forgets the room; it doesn't end it for the others.

On Linux, `ml` needs a desktop session (X11, or Wayland with Xwayland or the data-control protocol).

## Shell completions

`ml completions <shell>` prints a completion script for `bash`, `zsh`, `fish`, `powershell` or `elvish`:

```sh
ml completions bash > ~/.local/share/bash-completion/completions/ml
ml completions zsh > ~/.zfunc/_ml        # with fpath+=~/.zfunc in .zshrc
ml completions fish > ~/.config/fish/completions/ml.fish
ml completions powershell >> $PROFILE
```

## Releasing

Releases are built by [cargo-dist](https://opensource.axo.dev/cargo-dist/). Bump `version` in
`Cargo.toml`, merge it, then push a matching tag:

```sh
git tag v0.2.0 && git push origin v0.2.0
```

The release workflow builds every target, publishes the archives and installers to a GitHub release,
then publishes the crate to crates.io (needs the `CARGO_REGISTRY_TOKEN` repo secret).

## License

MIT, see [LICENSE](LICENSE).
