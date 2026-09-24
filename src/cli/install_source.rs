//! Detects how the running binary was installed, from its path on disk.

use std::path::{Path, PathBuf};

/// How the running `native-devtools-mcp` binary got onto this machine.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum InstallSource {
    /// Platform package installed by npm or npx (`node_modules/.../bin`).
    NpmPackage,
    /// Signed macOS app bundle from the DMG (`*.app/Contents/MacOS`).
    MacAppBundle,
    /// `cargo install` output in `$CARGO_HOME/bin`.
    CargoInstall,
    /// Built from a source checkout (`target/<profile>`).
    SourceBuild,
    /// Anywhere else, e.g. a binary extracted from a release archive.
    Unknown,
}

/// Classifies an install by the binary path. `cargo_bin_dir` is
/// `$CARGO_HOME/bin` when known; `~/.cargo/bin` is always recognized.
pub fn classify_install(exe_path: &Path, cargo_bin_dir: Option<&Path>) -> InstallSource {
    let parent = exe_path.parent();
    let names: Vec<&std::ffi::OsStr> = exe_path.components().map(|c| c.as_os_str()).collect();

    if is_app_bundle_binary(exe_path) {
        InstallSource::MacAppBundle
    } else if names.iter().any(|name| *name == "node_modules") {
        InstallSource::NpmPackage
    } else if parent.is_some_and(|dir| {
        cargo_bin_dir.is_some_and(|cargo_bin| dir == cargo_bin) || dir.ends_with(".cargo/bin")
    }) {
        InstallSource::CargoInstall
    } else if names.iter().any(|name| *name == "target") {
        InstallSource::SourceBuild
    } else {
        InstallSource::Unknown
    }
}

/// `true` for `<Name>.app/Contents/MacOS/<binary>`.
fn is_app_bundle_binary(exe_path: &Path) -> bool {
    let Some(macos_dir) = exe_path.parent() else {
        return false;
    };
    let Some(contents_dir) = macos_dir.parent() else {
        return false;
    };
    macos_dir.file_name().is_some_and(|name| name == "MacOS")
        && contents_dir
            .file_name()
            .is_some_and(|name| name == "Contents")
        && contents_dir
            .parent()
            .and_then(Path::extension)
            .is_some_and(|ext| ext == "app")
}

/// Path of the running binary (symlinks resolved when possible).
pub fn current_exe_path() -> std::io::Result<PathBuf> {
    let path = std::env::current_exe()?;
    Ok(std::fs::canonicalize(&path).unwrap_or(path))
}

/// `$CARGO_HOME/bin`, if `CARGO_HOME` is set.
pub fn cargo_bin_dir_from_env() -> Option<PathBuf> {
    std::env::var_os("CARGO_HOME").map(|home| PathBuf::from(home).join("bin"))
}

/// Resolves the running binary's path and install source.
pub fn detect_current() -> std::io::Result<(PathBuf, InstallSource)> {
    let exe_path = current_exe_path()?;
    let source = classify_install(&exe_path, cargo_bin_dir_from_env().as_deref());
    Ok((exe_path, source))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn classify(path: &str) -> InstallSource {
        classify_install(Path::new(path), None)
    }

    #[test]
    fn dmg_app_bundle_binary_is_app_bundle() {
        assert_eq!(
            classify("/Applications/NativeDevtools.app/Contents/MacOS/native-devtools-mcp"),
            InstallSource::MacAppBundle
        );
    }

    #[test]
    fn npx_cache_binary_is_npm_package() {
        assert_eq!(
            classify(
                "/Users/example/.npm/_npx/0a1b2c/node_modules/@sh3ll3x3c/native-devtools-mcp-darwin-arm64/bin/native-devtools-mcp"
            ),
            InstallSource::NpmPackage
        );
    }

    #[test]
    fn global_npm_binary_on_windows_is_npm_package() {
        assert_eq!(
            classify(
                "C:/Users/example/AppData/Roaming/npm/node_modules/@sh3ll3x3c/native-devtools-mcp-win32-x64/bin/native-devtools-mcp.exe"
            ),
            InstallSource::NpmPackage
        );
    }

    #[test]
    fn default_cargo_bin_is_cargo_install() {
        assert_eq!(
            classify("/Users/example/.cargo/bin/native-devtools-mcp"),
            InstallSource::CargoInstall
        );
    }

    #[test]
    fn custom_cargo_home_bin_is_cargo_install() {
        let source = classify_install(
            Path::new("/opt/rust/cargo/bin/native-devtools-mcp"),
            Some(Path::new("/opt/rust/cargo/bin")),
        );
        assert_eq!(source, InstallSource::CargoInstall);
    }

    #[test]
    fn custom_cargo_home_does_not_match_other_bin_dirs() {
        let source = classify_install(
            Path::new("/usr/local/bin/native-devtools-mcp"),
            Some(Path::new("/opt/rust/cargo/bin")),
        );
        assert_eq!(source, InstallSource::Unknown);
    }

    #[test]
    fn checkout_target_release_is_source_build() {
        assert_eq!(
            classify("/Users/example/src/native-devtools-mcp/target/release/native-devtools-mcp"),
            InstallSource::SourceBuild
        );
    }

    #[test]
    fn app_bundle_inside_build_output_is_still_app_bundle() {
        assert_eq!(
            classify("/Users/example/src/repo/dist/NativeDevtools.app/Contents/MacOS/native-devtools-mcp"),
            InstallSource::MacAppBundle
        );
    }

    #[test]
    fn contents_macos_without_app_extension_is_not_app_bundle() {
        assert_eq!(
            classify("/Users/example/NativeDevtools/Contents/MacOS/native-devtools-mcp"),
            InstallSource::Unknown
        );
    }

    #[test]
    fn extracted_release_archive_is_unknown() {
        assert_eq!(
            classify("/usr/local/bin/native-devtools-mcp"),
            InstallSource::Unknown
        );
    }
}
