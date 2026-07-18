use std::fs::{self, File};
use std::io::Read;
use std::path::{Path, PathBuf};

use sha2::{Digest, Sha256};

const FIXTURE_ID: &str = "ferrumc-fixture-dynamic";
const MANIFEST_TEMPLATE: &str = include_str!("../../plugin.toml.in");
const COPY_BUFFER_BYTES: usize = 64 * 1024;

pub(crate) fn package_bundle(library: &Path, plugins_root: &Path) -> PathBuf {
    let filename = library
        .file_name()
        .and_then(|name| name.to_str())
        .expect("fixture artifact has a UTF-8 filename");
    assert!(
        is_safe_fixture_filename(filename),
        "fixture artifact filename is safe for the TOML template"
    );

    fs::create_dir_all(plugins_root).expect("create plugins root");
    let bundle = plugins_root.join(FIXTURE_ID);
    fs::create_dir_all(&bundle).expect("create fixture bundle");
    let copied_library = bundle.join(filename);
    fs::copy(library, &copied_library).expect("copy fixture artifact");

    let digest = sha256_file(&copied_library);
    let manifest = MANIFEST_TEMPLATE
        .replace("{{LIBRARY}}", filename)
        .replace("{{LIBRARY_SHA256}}", &hex_digest(digest))
        .replace("{{SERVER_API}}", env!("CARGO_PKG_VERSION"));
    let temporary_manifest = bundle.join(".plugin.toml.tmp");
    fs::write(&temporary_manifest, manifest).expect("write fixture manifest");
    fs::rename(&temporary_manifest, bundle.join("plugin.toml")).expect("publish fixture manifest");

    bundle
}

fn is_safe_fixture_filename(filename: &str) -> bool {
    !filename.is_empty()
        && filename
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'.' | b'_' | b'-'))
}

fn sha256_file(path: &Path) -> [u8; 32] {
    let mut file = File::open(path).expect("open copied fixture for hashing");
    let mut hasher = Sha256::new();
    let mut buffer = vec![0_u8; COPY_BUFFER_BYTES];
    loop {
        let read = file.read(&mut buffer).expect("hash copied fixture");
        if read == 0 {
            break;
        }
        hasher.update(&buffer[..read]);
    }
    hasher.finalize().into()
}

fn hex_digest(digest: [u8; 32]) -> String {
    const HEX: &[u8; 16] = b"0123456789abcdef";
    let mut encoded = String::with_capacity(64);
    for byte in digest {
        encoded.push(char::from(HEX[usize::from(byte >> 4)]));
        encoded.push(char::from(HEX[usize::from(byte & 0x0f)]));
    }
    encoded
}

#[cfg(test)]
mod tests {
    use super::{hex_digest, is_safe_fixture_filename, package_bundle};
    use std::fs;
    use std::path::PathBuf;

    #[test]
    fn packager_hashes_the_exact_copied_bytes_into_plugin_toml() {
        let scratch = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .join("../../../.codex-tmp/p53-package-unit")
            .join(std::process::id().to_string());
        if let Err(error) = fs::remove_dir_all(&scratch) {
            assert_eq!(error.kind(), std::io::ErrorKind::NotFound);
        }
        fs::create_dir_all(&scratch).expect("create repo-local fixture scratch");
        let library = scratch.join("fixture.so");
        fs::write(&library, b"fixture").expect("write deterministic artifact bytes");
        let plugins_root = scratch.join("plugins");

        let bundle = package_bundle(&library, &plugins_root);
        assert_eq!(
            fs::read(bundle.join("fixture.so")).expect("read copied fixture"),
            b"fixture"
        );
        let manifest =
            fs::read_to_string(bundle.join("plugin.toml")).expect("read generated manifest");
        assert!(manifest.contains("library = \"fixture.so\""));
        assert!(manifest.contains(
            "library_sha256 = \"f16d05ec6b29248d2c61adb1e9263f78e4f7bace1b955014a2d17872cfe4064d\""
        ));
        assert!(!manifest.contains("{{"));

        fs::remove_dir_all(&scratch).expect("remove fixture scratch");
    }

    #[test]
    fn lowercase_digest_encoding_is_exact() {
        assert_eq!(
            hex_digest([0xab; 32]),
            "abababababababababababababababababababababababababababababababab"
        );
    }

    #[test]
    fn artifact_filename_grammar_prevents_toml_injection() {
        for valid in [
            "libferrumc_plugin_fixture_dynamic.so",
            "fixture-1.dll",
            "fixture.dylib",
        ] {
            assert!(is_safe_fixture_filename(valid), "{valid}");
        }
        for invalid in [
            "",
            "fixture\".so",
            "fixture\nlibrary_sha256=\"00",
            "fixture\\.so",
            "fixture so",
        ] {
            assert!(!is_safe_fixture_filename(invalid), "{invalid:?}");
        }
    }
}
