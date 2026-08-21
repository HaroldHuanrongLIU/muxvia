use std::{
    collections::BTreeSet,
    fs::{self, File},
    io::{self, Read},
    os::unix::fs::PermissionsExt,
    path::Path,
};

use ring::digest::{Context, SHA256};
use serde::Deserialize;
use thiserror::Error;

const MANIFEST_FILE: &str = "muxvia-release.json";
const FILE_CONTRACTS: [(&str, &str, bool); 5] = [
    ("control-plane", "muxvia", true),
    ("routing-service", "muxvia-routing", true),
    ("license", "LICENSE", false),
    ("third-party-notices", "THIRD_PARTY_NOTICES.md", false),
    ("extraction-manifest", "EXTRACTION_MANIFEST.json", false),
];

#[derive(Debug, Error)]
#[error("release-bundle-invalid")]
pub struct ReleaseBundleError;

#[derive(Deserialize)]
#[serde(deny_unknown_fields, rename_all = "camelCase")]
struct Manifest {
    schema_version: u8,
    product: String,
    release: String,
    target: String,
    build: String,
    rpc: Rpc,
    files: Vec<ManifestFile>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Rpc {
    major: u32,
    minor: u32,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields, rename_all = "camelCase")]
struct ManifestFile {
    role: String,
    path: String,
    executable: bool,
    byte_length: u64,
    sha256: String,
}

pub fn validate_embedded_release_bundle() -> Result<(), ReleaseBundleError> {
    let (Some(target), Some(build)) = (
        option_env!("MUXVIA_BUNDLE_TARGET"),
        option_env!("MUXVIA_BUNDLE_BUILD"),
    ) else {
        if option_env!("MUXVIA_BUNDLE_TARGET").is_some()
            || option_env!("MUXVIA_BUNDLE_BUILD").is_some()
        {
            return Err(ReleaseBundleError);
        }
        return Ok(());
    };
    if target != runtime_target() {
        return Err(ReleaseBundleError);
    }
    let executable = std::env::current_exe().map_err(|_| ReleaseBundleError)?;
    validate_release_bundle(&executable, env!("CARGO_PKG_VERSION"), target, build)
}

fn runtime_target() -> &'static str {
    #[cfg(all(target_os = "macos", target_arch = "aarch64"))]
    return "darwin-arm64";
    #[cfg(all(target_os = "macos", target_arch = "x86_64"))]
    return "darwin-x64";
    #[cfg(all(target_os = "linux", target_arch = "aarch64", target_env = "gnu"))]
    return "linux-glibc-arm64";
    #[cfg(all(target_os = "linux", target_arch = "x86_64", target_env = "gnu"))]
    return "linux-glibc-x64";
    #[allow(unreachable_code)]
    "unsupported"
}

fn validate_release_bundle(
    routing_executable: &Path,
    release: &str,
    target: &str,
    build: &str,
) -> Result<(), ReleaseBundleError> {
    let routing_executable =
        fs::canonicalize(routing_executable).map_err(|_| ReleaseBundleError)?;
    let root = routing_executable.parent().ok_or(ReleaseBundleError)?;
    if routing_executable != root.join("muxvia-routing") {
        return Err(ReleaseBundleError);
    }
    let manifest_path = root.join(MANIFEST_FILE);
    let manifest_metadata = fs::symlink_metadata(&manifest_path).map_err(|_| ReleaseBundleError)?;
    if !manifest_metadata.is_file() || manifest_metadata.file_type().is_symlink() {
        return Err(ReleaseBundleError);
    }
    let manifest: Manifest =
        serde_json::from_reader(File::open(manifest_path).map_err(|_| ReleaseBundleError)?)
            .map_err(|_| ReleaseBundleError)?;
    if manifest.schema_version != 1
        || manifest.product != "muxvia"
        || manifest.release != release
        || manifest.target != target
        || manifest.build != build
        || manifest.rpc.major != 1
        || manifest.rpc.minor != 0
        || manifest.files.len() != FILE_CONTRACTS.len()
    {
        return Err(ReleaseBundleError);
    }

    let actual_names = fs::read_dir(root)
        .map_err(|_| ReleaseBundleError)?
        .map(|entry| {
            entry.map_err(|_| ReleaseBundleError).and_then(|value| {
                value
                    .file_name()
                    .into_string()
                    .map_err(|_| ReleaseBundleError)
            })
        })
        .collect::<Result<BTreeSet<_>, _>>()?;
    let expected_names = std::iter::once(MANIFEST_FILE)
        .chain(FILE_CONTRACTS.iter().map(|(_, path, _)| *path))
        .map(str::to_owned)
        .collect::<BTreeSet<_>>();
    if actual_names != expected_names {
        return Err(ReleaseBundleError);
    }

    for (file, (role, relative_path, executable)) in
        manifest.files.iter().zip(FILE_CONTRACTS.iter())
    {
        if file.role != *role
            || file.path != *relative_path
            || file.executable != *executable
            || !valid_sha256(&file.sha256)
        {
            return Err(ReleaseBundleError);
        }
        let path = root.join(relative_path);
        let metadata = fs::symlink_metadata(&path).map_err(|_| ReleaseBundleError)?;
        if !metadata.is_file()
            || metadata.file_type().is_symlink()
            || metadata.len() != file.byte_length
            || (*executable != (metadata.permissions().mode() & 0o111 != 0))
            || sha256(&path).map_err(|_| ReleaseBundleError)? != file.sha256
        {
            return Err(ReleaseBundleError);
        }
    }
    Ok(())
}

fn valid_sha256(value: &str) -> bool {
    value.len() == 64
        && value
            .bytes()
            .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
}

fn sha256(path: &Path) -> io::Result<String> {
    let mut file = File::open(path)?;
    let mut context = Context::new(&SHA256);
    let mut buffer = [0_u8; 64 * 1024];
    loop {
        let read = file.read(&mut buffer)?;
        if read == 0 {
            break;
        }
        context.update(&buffer[..read]);
    }
    Ok(context
        .finish()
        .as_ref()
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect())
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    use std::{fs::OpenOptions, io::Write, os::unix::fs::OpenOptionsExt, path::PathBuf};
    use tempfile::TempDir;

    fn fixture() -> (TempDir, PathBuf) {
        let root = TempDir::new().unwrap();
        for (name, contents, mode) in [
            ("muxvia", b"control-plane".as_slice(), 0o755),
            ("muxvia-routing", b"routing-service".as_slice(), 0o755),
            ("LICENSE", b"license".as_slice(), 0o644),
            ("THIRD_PARTY_NOTICES.md", b"notices".as_slice(), 0o644),
            ("EXTRACTION_MANIFEST.json", b"extractions".as_slice(), 0o644),
        ] {
            let path = root.path().join(name);
            let mut file = OpenOptions::new()
                .write(true)
                .create_new(true)
                .mode(mode)
                .open(path)
                .unwrap();
            file.write_all(contents).unwrap();
        }
        let files = FILE_CONTRACTS
            .iter()
            .map(|(role, path, executable)| {
                let path_on_disk = root.path().join(path);
                json!({
                    "role": role,
                    "path": path,
                    "executable": executable,
                    "byteLength": fs::metadata(&path_on_disk).unwrap().len(),
                    "sha256": sha256(&path_on_disk).unwrap(),
                })
            })
            .collect::<Vec<_>>();
        fs::write(
            root.path().join(MANIFEST_FILE),
            serde_json::to_vec(&json!({
                "schemaVersion": 1,
                "product": "muxvia",
                "release": "0.1.0",
                "target": runtime_target(),
                "build": "0123456789abcdef",
                "rpc": { "major": 1, "minor": 0 },
                "files": files,
            }))
            .unwrap(),
        )
        .unwrap();
        let executable = root.path().join("muxvia-routing");
        (root, executable)
    }

    #[test]
    fn validates_a_complete_bound_bundle() {
        let (_root, executable) = fixture();
        validate_release_bundle(&executable, "0.1.0", runtime_target(), "0123456789abcdef")
            .unwrap();
    }

    #[test]
    fn rejects_metadata_and_integrity_mismatches() {
        for (release, target, build) in [
            ("0.2.0", runtime_target(), "0123456789abcdef"),
            ("0.1.0", "other", "0123456789abcdef"),
            ("0.1.0", runtime_target(), "different"),
        ] {
            let (_root, executable) = fixture();
            assert!(validate_release_bundle(&executable, release, target, build).is_err());
        }
        let (root, executable) = fixture();
        fs::write(root.path().join("muxvia"), "tampered").unwrap();
        assert!(
            validate_release_bundle(&executable, "0.1.0", runtime_target(), "0123456789abcdef")
                .is_err()
        );
    }
}
