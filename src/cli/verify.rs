use sha2::{Digest, Sha256};
use std::fs;
use std::path::Path;

use super::install_source::{self, InstallSource};
use super::{BOLD, DIM, GREEN, RED, RESET, YELLOW};

const REPO: &str = "sh3ll3x3c/native-devtools-mcp";
const VERSION: &str = env!("CARGO_PKG_VERSION");

pub fn run() {
    println!();
    println!("{BOLD}native-devtools-mcp v{VERSION} — Binary Verification{RESET}");
    println!();

    // Step 1: Hash the current binary
    let (exe_path, source) = match install_source::detect_current() {
        Ok(detected) => detected,
        Err(e) => {
            println!("{RED}✗{RESET} Failed to determine binary path: {e}");
            std::process::exit(1);
        }
    };

    let local_hash = match hash_file(&exe_path) {
        Ok(h) => h,
        Err(e) => {
            println!("{RED}✗{RESET} Failed to hash binary: {e}");
            std::process::exit(1);
        }
    };

    println!("  Binary:  {}", exe_path.display());
    println!("  SHA-256: {DIM}{local_hash}{RESET}");
    println!();

    if let Some(reason) = unpublished_checksum_reason(source) {
        println!("  {YELLOW}?{RESET} Cannot verify this binary against release checksums.");
        println!("    {reason}");
        println!();
        if source == InstallSource::MacAppBundle {
            if let Some(bundle) = exe_path.ancestors().nth(3) {
                println!("  Check the app's code signature and notarization instead:");
                println!(
                    "    codesign --verify --deep --strict --verbose=2 \"{}\"",
                    bundle.display()
                );
                println!(
                    "    spctl --assess --type execute --verbose \"{}\"",
                    bundle.display()
                );
                println!();
            }
        }
        println!("  To verify an official binary, run this command from the npm package:");
        println!("    npx native-devtools-mcp verify");
        println!();
        std::process::exit(2);
    }

    // Step 2: Fetch expected checksums from GitHub
    let checksums_url =
        format!("https://github.com/{REPO}/releases/download/v{VERSION}/checksums.txt");

    println!("  Fetching checksums from GitHub release v{VERSION}...");

    let checksums_text = match fetch_checksums(&checksums_url) {
        Ok(text) => text,
        Err(e) => {
            println!();
            println!("  {YELLOW}?{RESET} Could not fetch checksums: {e}");
            println!();
            println!("  This may mean:");
            println!("  - No internet connection");
            println!("  - This is a development build with no matching release");
            println!("  - The release does not include checksums yet");
            println!();
            println!("  Your local hash: {BOLD}{local_hash}{RESET}");
            println!("  Compare manually at: https://github.com/{REPO}/releases/tag/v{VERSION}");
            println!();
            std::process::exit(2);
        }
    };

    let expected_hash = match find_expected_hash(&checksums_text) {
        Some(hash) => hash,
        None => {
            println!();
            println!(
                "  {YELLOW}?{RESET} No matching checksum found for this platform in the release."
            );
            println!("  Your local hash: {local_hash}");
            println!("  Check manually at: https://github.com/{REPO}/releases/tag/v{VERSION}");
            println!();
            std::process::exit(2);
        }
    };

    match compare_hashes(source, &local_hash, &expected_hash) {
        Verdict::Verified => {
            println!();
            println!("  {GREEN}✓ Verified{RESET} — binary matches the official GitHub release.");
            println!();
        }
        Verdict::Mismatch => {
            println!();
            println!("  {RED}✗ Mismatch{RESET} — binary does NOT match the official release!");
            println!();
            println!("  Local:    {local_hash}");
            println!("  Expected: {expected_hash}");
            println!();
            std::process::exit(1);
        }
        Verdict::Unconfirmed => {
            println!();
            println!("  {YELLOW}!{RESET} Hash does not match the official release binary.");
            println!();
            println!("  Local:    {local_hash}");
            println!("  Official: {expected_hash}");
            println!();
            println!("  The install source could not be identified from the binary path:");
            println!("  {}", exe_path.display());
            println!(
                "  If you extracted it from the release archive unchanged, treat it as modified."
            );
            println!(
                "  If it was built locally or copied from elsewhere, a different hash is expected."
            );
            println!();
            std::process::exit(2);
        }
    }
}

/// Outcome of comparing the local hash to the published one.
#[derive(Debug, PartialEq, Eq)]
enum Verdict {
    /// Hashes match.
    Verified,
    /// The binary is the published release artifact, and its hash differs.
    Mismatch,
    /// Hashes differ, but the binary may not be that artifact.
    Unconfirmed,
}

/// Why `checksums.txt` has no entry for this install's binary, or `None`
/// when it does.
///
/// `checksums.txt` covers the npm platform binaries (the same bytes as the
/// release archives) plus the archives and the DMG file. It has no entry for
/// the signed binary inside the app bundle, which is built and signed in a
/// separate job, and none for anything compiled on the user's machine.
fn unpublished_checksum_reason(source: InstallSource) -> Option<&'static str> {
    match source {
        InstallSource::MacAppBundle => Some(
            "The app bundle binary is signed separately; the release publishes a checksum for the DMG only.",
        ),
        InstallSource::CargoInstall => Some(
            "This binary was built by `cargo install` on this machine; no checksum is published for local builds.",
        ),
        InstallSource::SourceBuild => Some(
            "This binary was built from source on this machine; no checksum is published for local builds.",
        ),
        InstallSource::NpmPackage | InstallSource::Unknown => None,
    }
}

/// Compares hashes for a source that has a published checksum. Only the npm
/// package binary is known to be the exact artifact in `checksums.txt`, so
/// only it yields `Mismatch`.
fn compare_hashes(source: InstallSource, local_hash: &str, expected_hash: &str) -> Verdict {
    if local_hash.eq_ignore_ascii_case(expected_hash) {
        Verdict::Verified
    } else if source == InstallSource::NpmPackage {
        Verdict::Mismatch
    } else {
        Verdict::Unconfirmed
    }
}

fn hash_file(path: &Path) -> Result<String, String> {
    let data = fs::read(path).map_err(|e| format!("read error: {e}"))?;
    let mut hasher = Sha256::new();
    hasher.update(&data);
    Ok(format!("{:x}", hasher.finalize()))
}

fn fetch_checksums(url: &str) -> Result<String, String> {
    let response = ureq::get(url).call().map_err(|e| format!("{e}"))?;
    response
        .into_body()
        .read_to_string()
        .map_err(|e| format!("failed to read response: {e}"))
}

fn find_expected_hash(checksums: &str) -> Option<String> {
    let platform_binary = if cfg!(target_os = "macos") {
        "native-devtools-mcp (aarch64-apple-darwin)"
    } else if cfg!(target_os = "windows") {
        "native-devtools-mcp.exe (x86_64-pc-windows-msvc)"
    } else {
        return None;
    };

    for line in checksums.lines() {
        // Format: "hash  filename"
        if let Some((hash, name)) = line.split_once("  ") {
            if name.trim() == platform_binary {
                return Some(hash.trim().to_string());
            }
        }
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    const HASH_A: &str = "3f5c0e6a9b1d2c4e5f60718293a4b5c6d7e8f90112233445566778899aabbccd";
    const HASH_B: &str = "0000000000000000000000000000000000000000000000000000000000000001";

    #[test]
    fn unpublished_reasons_name_the_install_mode() {
        let reason = |source| unpublished_checksum_reason(source).unwrap_or_default();
        assert!(reason(InstallSource::MacAppBundle).contains("app bundle"));
        assert!(reason(InstallSource::CargoInstall).contains("cargo install"));
        assert!(reason(InstallSource::SourceBuild).contains("from source"));
    }

    #[test]
    fn npm_package_and_unknown_are_compared() {
        assert_eq!(unpublished_checksum_reason(InstallSource::NpmPackage), None);
        assert_eq!(unpublished_checksum_reason(InstallSource::Unknown), None);
    }

    #[test]
    fn npm_binary_with_different_hash_is_mismatch() {
        assert_eq!(
            compare_hashes(InstallSource::NpmPackage, HASH_A, HASH_B),
            Verdict::Mismatch
        );
    }

    #[test]
    fn unknown_binary_with_different_hash_is_unconfirmed() {
        assert_eq!(
            compare_hashes(InstallSource::Unknown, HASH_A, HASH_B),
            Verdict::Unconfirmed
        );
    }

    #[test]
    fn matching_hash_is_verified_regardless_of_case() {
        assert_eq!(
            compare_hashes(InstallSource::Unknown, HASH_A, &HASH_A.to_uppercase()),
            Verdict::Verified
        );
    }

    #[test]
    fn expected_hash_is_read_from_platform_binary_line_not_archive_or_dmg() {
        // Layout written by the release workflow's "Generate checksums" step.
        let checksums = format!(
            "{HASH_A}  native-devtools-mcp (aarch64-apple-darwin)\n\
             {HASH_B}  native-devtools-mcp.exe (x86_64-pc-windows-msvc)\n\
             1111111111111111111111111111111111111111111111111111111111111111  native-devtools-mcp-aarch64-apple-darwin.tar.gz\n\
             2222222222222222222222222222222222222222222222222222222222222222  NativeDevtools-1.0.0.dmg\n"
        );

        let expected = find_expected_hash(&checksums);

        if cfg!(target_os = "macos") {
            assert_eq!(expected.as_deref(), Some(HASH_A));
        } else if cfg!(target_os = "windows") {
            assert_eq!(expected.as_deref(), Some(HASH_B));
        } else {
            assert_eq!(expected, None);
        }
    }
}
