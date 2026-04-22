# ccomidi-nih — build/install notes

## Why the DAW wouldn't load the plugin

When `cargo xtask install ccomidi-nih` completes without error, the resulting
`.clap` can still fail to load. In this environment, two things were at play:

### 1. Architecture mismatch (primary cause)

The machine is Apple Silicon (arm64), but `rustup show active-toolchain` is
`stable-x86_64-apple-darwin`. That means cargo's default host target is
x86_64, so `cargo xtask bundle ... --release` builds an **x86_64** dylib.

A DAW running natively as arm64 on Apple Silicon will silently skip an x86_64
CLAP — it shows up in the filesystem but never appears in the plugin list.
(DAWs launched under Rosetta would load it. Most users launch native.)

Verify with:

```
lipo -archs ~/Library/Audio/Plug-Ins/CLAP/ccomidi-nih.clap/Contents/MacOS/ccomidi-nih
# must print: arm64   (not: x86_64)
```

### 2. nih_plug_xtask bundle step vs. the repo's cargo target pin

`.cargo/config.toml` in this workspace pins:

```toml
[build]
target = "aarch64-apple-darwin"
```

That forces native arm64 builds — good. **But** `nih_plug_xtask`'s bundle
step looks for the built dylib at `target/release/libccomidi_nih.dylib`,
while the target pin puts it at `target/aarch64-apple-darwin/release/…`.
Result:

```
Error: Could not find a built library at '.../target/release/libccomidi_nih.dylib'.
Error: bundle step failed — aborting install
```

So `cargo xtask install` fails for anyone whose rustup host isn't already
`aarch64-apple-darwin`.

### 3. Self-signing

`nih_plug_xtask` runs `codesign -f -s - <bundle>` after bundling. Without
that ad-hoc signature, macOS Gatekeeper will refuse to load the dylib inside
a hardened-runtime DAW. Any manual bundle step must replicate this.

## Working install recipe on this machine

Pick one:

**A. Switch rustup default to arm64 (best fix):**

```
rustup default stable-aarch64-apple-darwin
cargo xtask install ccomidi-nih
```

With this, the `.cargo/config.toml` target pin is redundant but harmless,
and `cargo xtask install` works end-to-end.

**B. Manual bundle from the pinned target dir** (if the toolchain swap isn't
desirable):

```
cargo build -p ccomidi-nih --release    # builds to target/aarch64-apple-darwin/release/
BUNDLE="$(pwd)/target/bundled/ccomidi-nih.clap"
mkdir -p "$BUNDLE/Contents/MacOS"
cp target/aarch64-apple-darwin/release/libccomidi_nih.dylib \
   "$BUNDLE/Contents/MacOS/ccomidi-nih"
printf 'BNDL????' > "$BUNDLE/Contents/PkgInfo"
# write Info.plist matching nih_plug_xtask::maybe_create_macos_bundle_metadata
codesign -f -s - "$BUNDLE"
ln -sf "$BUNDLE" ~/Library/Audio/Plug-Ins/CLAP/ccomidi-nih.clap
```

The currently-installed plugin was built this way (arm64, ad-hoc signed).

## Suggested upstream fix

Either:

- Remove `[build] target = "aarch64-apple-darwin"` from `.cargo/config.toml`
  and tell users to `rustup target add` / set their default themselves, or
- Patch `xtask/src/main.rs`'s `install_cmd` to pass `--target` through to
  `cargo xtask bundle`, and teach the symlink step to find the dylib under
  `target/<triple>/release/` instead of `target/release/`.

The second is friendlier: it lets the repo keep forcing a native build
without assuming anything about the user's rustup default.

## Cargo hang seen during diagnosis

After symlinking `target/release -> aarch64-apple-darwin/release` and
re-running `cargo xtask install`, cargo started and then hung indefinitely
with ~0s CPU usage — never printing a single line. Removing the symlink
fixes it. Don't use that workaround; use one of the options above instead.
