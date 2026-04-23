//! Workspace xtask binary.
//!
//! `nih_plug_xtask` gives us `bundle` / `bundle-universal` / `bundle-cross`
//! for free. On top of that we add one custom subcommand:
//!
//! ```text
//! cargo xtask install <package> [--no-bundle]
//! ```
//!
//! which (re)creates a *symlink* from the OS-appropriate VST3 plugin directory
//! back into `target/bundled/<package>.vst3`. The upshot: run it once per
//! cloned workspace, and every subsequent `cargo xtask bundle ... --release`
//! is picked up by the host automatically — no re-copy, no re-install.

use anyhow::bail;
use std::path::{Path, PathBuf};
use std::process::Command;

/// nih_plug_xtask re-exports `Result<T> = Result<T, Box<dyn Error + Send + Sync>>`.
/// We use the same alias so `?` flows cleanly between our code and the
/// library's.
fn main() -> nih_plug_xtask::Result<()> {
    // `skip(1)` drops the binary name so `args[0]` is the subcommand.
    let mut args = std::env::args().skip(1).peekable();

    match args.peek().map(String::as_str) {
        Some("install") => {
            // Consume "install" so `install_cmd` only sees its own args.
            let _ = args.next();
            install_cmd(args.collect())
        }
        _ => {
            // Delegate everything else — bundle, bundle-universal, etc. —
            // to the library. It reads `std::env::args()` itself, so we
            // don't need to pass anything through.
            nih_plug_xtask::main()
        }
    }
}

/// Implementation of `cargo xtask install <package> [--no-bundle]`.
///
/// Steps:
///   1. Optionally (unless `--no-bundle`) invoke `cargo xtask bundle <pkg> --release`.
///   2. Symlink `~/.../VST3/<pkg>.vst3` → `target/bundled/<pkg>.vst3`.
fn install_cmd(raw_args: Vec<String>) -> nih_plug_xtask::Result<()> {
    // --- tiny hand-rolled arg parser ------------------------------------
    // We avoid pulling in `clap` (or anything) since the workspace already
    // has a heavy dep graph and this tool is trivial.
    let mut packages: Vec<String> = Vec::new();
    let mut skip_bundle = false;
    let mut bundle_universal = false;

    for arg in raw_args {
        match arg.as_str() {
            "--no-bundle" => skip_bundle = true,
            "--universal" => bundle_universal = true,
            "-h" | "--help" => {
                print_help();
                return Ok(());
            }
            other if other.starts_with('-') => {
                bail!("unknown flag: {other}");
            }
            _ => packages.push(arg),
        }
    }

    if packages.is_empty() {
        print_help();
        bail!("no package name given");
    }

    // --- step 1: bundle --------------------------------------------------
    if !skip_bundle {
        let bundle_sub = if bundle_universal { "bundle-universal" } else { "bundle" };
        eprintln!("==> running cargo xtask {bundle_sub} <packages> --release");
        let status = Command::new("cargo")
            .arg("xtask")
            .arg(bundle_sub)
            .args(&packages)
            .arg("--release")
            .status()?;
        if !status.success() {
            bail!("bundle step failed — aborting install");
        }
    }

    // --- step 2: symlink per package ------------------------------------
    let vst3_dir = user_vst3_plugin_dir()?;
    std::fs::create_dir_all(&vst3_dir)?;

    for pkg in &packages {
        link_one(pkg, &vst3_dir)?;
    }
    Ok(())
}

fn link_one(pkg: &str, vst3_dir: &Path) -> nih_plug_xtask::Result<()> {
    let bundle_name = format!("{pkg}.vst3");
    let src = std::env::current_dir()?
        .join("target")
        .join("bundled")
        .join(&bundle_name);

    if !src.exists() {
        bail!(
            "{} does not exist — run `cargo xtask bundle {pkg} --release` first, \
             or drop the --no-bundle flag",
            src.display()
        );
    }

    let dst = vst3_dir.join(&bundle_name);

    // If there's already something at dst, decide what to do:
    //   - a symlink (ours or anyone's): replace it (we own the name)
    //   - a real file/dir: refuse — that's probably a hand-installed copy
    //     the user cares about.
    if dst.is_symlink() {
        std::fs::remove_file(&dst)?;
    } else if dst.exists() {
        bail!(
            "{} exists and is not a symlink; refusing to overwrite a real install. \
             Remove it by hand and re-run if you want the symlink.",
            dst.display()
        );
    }

    // `symlink` is in os-specific modules — use cfg! to reach the right one.
    #[cfg(unix)]
    std::os::unix::fs::symlink(&src, &dst)?;
    #[cfg(windows)]
    std::os::windows::fs::symlink_dir(&src, &dst)?;

    println!("linked {} → {}", dst.display(), src.display());
    Ok(())
}

/// Per-OS user-scope VST3 plugin directory.
///
/// Reference: <https://steinbergmedia.github.io/vst3_dev_portal/pages/Technical+Documentation/Locations+Format/Plugin+Format.html>
fn user_vst3_plugin_dir() -> nih_plug_xtask::Result<PathBuf> {
    #[cfg(target_os = "macos")]
    {
        let home = std::env::var("HOME")?;
        Ok(PathBuf::from(home).join("Library/Audio/Plug-Ins/VST3"))
    }
    #[cfg(target_os = "linux")]
    {
        let home = std::env::var("HOME")?;
        Ok(PathBuf::from(home).join(".vst3"))
    }
    #[cfg(target_os = "windows")]
    {
        // User-scope on Windows is `%LOCALAPPDATA%\Programs\Common\VST3`.
        let appdata = std::env::var("LOCALAPPDATA")?;
        Ok(PathBuf::from(appdata).join("Programs").join("Common").join("VST3"))
    }
}

fn print_help() {
    eprintln!(
        "\
cargo xtask install <package>... [flags]

Builds each package's .vst3 bundle and symlinks it into the user-scope
VST3 plugin directory. Idempotent — rerunning replaces stale symlinks,
and never overwrites a non-symlink at the destination.

Flags:
  --no-bundle     skip the bundle step (assume target/bundled/<pkg>.vst3
                  already exists; useful for re-linking only)
  --universal     build as a universal (x86_64 + arm64) binary on macOS

Examples:
  cargo xtask install ccomidi-nih
  cargo xtask install ccomidi-nih --universal
  cargo xtask install ccomidi-nih --no-bundle
"
    );
}
