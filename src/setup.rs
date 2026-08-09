use std::{
    env, fs,
    io::{self, IsTerminal, Read, Write},
    path::{Path, PathBuf},
};

use anyhow::{Context, Result, anyhow, bail};
use bzip2::read::BzDecoder;
use libloading::Library;
use sha2::{Digest, Sha256};
use tempfile::NamedTempFile;

const VERSION: &str = "2.3.0";
const MODULE_NAME: &str = "libopenh264.so.6";
const X86_64_URL: &str = "https://ciscobinary.openh264.org/libopenh264-2.3.0-linux64.6.so.bz2";
const X86_64_SHA256: &str = "a6294cde9ae10966cc639481b5c39b7fc57b2fcad417f11d7baca3b0f7914985";
const AARCH64_URL: &str = "https://ciscobinary.openh264.org/libopenh264-2.3.0-linux-arm64.6.so.bz2";
const AARCH64_SHA256: &str = "f72a9a305ff7bc33b4a8e05636148749c3d4fa25b3900b555c531eeec3c607ac";

#[derive(Clone, Copy, Debug)]
struct Asset {
    url: &'static str,
    sha256: &'static str,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum CodecSource {
    System(PathBuf),
    Managed(PathBuf),
}

impl CodecSource {
    pub(crate) fn path(&self) -> &Path {
        match self {
            Self::System(path) | Self::Managed(path) => path,
        }
    }
}

pub(crate) fn run(yes: bool, check: bool) -> Result<()> {
    if let Some(source) = find_compatible()? {
        println!(
            "OpenH264 {VERSION} is available at {}.",
            source.path().display()
        );
        return Ok(());
    }
    if check {
        bail!(
            "OpenH264 {VERSION} is not installed. Run `cast setup --yes` to install the verified Cisco module"
        );
    }
    if !yes {
        if !io::stdin().is_terminal() {
            bail!(
                "OpenH264 setup needs confirmation. Run `cast setup --yes` in non-interactive environments"
            );
        }
        print!("Download Cisco OpenH264 {VERSION} under your user data directory? [y/N] ");
        io::stdout().flush()?;
        let mut answer = String::new();
        io::stdin().read_line(&mut answer)?;
        if !matches!(answer.trim().to_ascii_lowercase().as_str(), "y" | "yes") {
            bail!("OpenH264 setup cancelled");
        }
    }

    let asset = asset_for_arch()?;
    let response = ureq::get(asset.url)
        .call()
        .with_context(|| format!("could not download Cisco OpenH264 {VERSION}"))?;
    let mut reader = response.into_reader();
    let destination = managed_module_path()?;
    install_archive(&mut reader, asset.sha256, &destination)?;
    verify_module(&destination)?;
    println!("Installed OpenH264 {VERSION} at {}.", destination.display());
    Ok(())
}

pub(crate) fn find_compatible() -> Result<Option<CodecSource>> {
    for path in system_candidates() {
        if path.is_file() && verify_module(&path).is_ok() {
            return Ok(Some(CodecSource::System(path)));
        }
    }
    let managed = managed_module_path()?;
    if managed.is_file() && verify_module(&managed).is_ok() {
        return Ok(Some(CodecSource::Managed(managed)));
    }
    Ok(None)
}

pub(crate) fn managed_module_path() -> Result<PathBuf> {
    let data = match env::var_os("XDG_DATA_HOME") {
        Some(path) if !path.is_empty() => PathBuf::from(path),
        _ => env::var_os("HOME")
            .filter(|path| !path.is_empty())
            .map(PathBuf::from)
            .map(|path| path.join(".local/share"))
            .ok_or_else(|| anyhow!("neither XDG_DATA_HOME nor HOME is set"))?,
    };
    Ok(data.join("cast/codecs").join(MODULE_NAME))
}

fn asset_for_arch() -> Result<Asset> {
    match env::consts::ARCH {
        "x86_64" => Ok(Asset {
            url: X86_64_URL,
            sha256: X86_64_SHA256,
        }),
        "aarch64" => Ok(Asset {
            url: AARCH64_URL,
            sha256: AARCH64_SHA256,
        }),
        arch => bail!("OpenH264 setup does not provide a Linux asset for {arch}"),
    }
}

fn system_candidates() -> Vec<PathBuf> {
    let mut roots = vec![
        PathBuf::from("/usr/lib"),
        PathBuf::from("/usr/local/lib"),
        PathBuf::from("/lib"),
    ];
    roots.extend(
        ["x86_64-linux-gnu", "aarch64-linux-gnu"]
            .into_iter()
            .flat_map(|target| {
                [
                    PathBuf::from("/usr/lib").join(target),
                    PathBuf::from("/lib").join(target),
                ]
            }),
    );
    roots
        .into_iter()
        .flat_map(|root| {
            [
                root.join(MODULE_NAME),
                root.join("libopenh264.so"),
                root.join("libopenh264.so.7"),
            ]
        })
        .collect()
}

#[repr(C)]
#[derive(Default)]
struct OpenH264Version {
    major: u32,
    minor: u32,
    revision: u32,
    reserved: u32,
}

fn verify_module(path: &Path) -> Result<()> {
    let library = unsafe { Library::new(path) }
        .with_context(|| format!("could not load OpenH264 module {}", path.display()))?;
    let version = unsafe {
        let get_version = library
            .get::<unsafe extern "C" fn(*mut OpenH264Version)>(b"WelsGetCodecVersionEx\0")
            .context("OpenH264 module does not export WelsGetCodecVersionEx")?;
        let mut version = OpenH264Version::default();
        get_version(&mut version);
        version
    };
    if (version.major, version.minor) != (2, 3) {
        bail!(
            "OpenH264 module reports {}.{}.{}; Cast requires a compatible 2.3.x module",
            version.major,
            version.minor,
            version.revision
        );
    }
    Ok(())
}

fn install_archive(reader: &mut dyn Read, expected_sha256: &str, destination: &Path) -> Result<()> {
    let mut archive = Vec::new();
    reader
        .read_to_end(&mut archive)
        .context("could not read the OpenH264 archive")?;
    let actual = format!("{:x}", Sha256::digest(&archive));
    if actual != expected_sha256 {
        bail!("OpenH264 archive checksum mismatch: expected {expected_sha256}, got {actual}");
    }
    let parent = destination
        .parent()
        .ok_or_else(|| anyhow!("OpenH264 destination has no parent directory"))?;
    fs::create_dir_all(parent).with_context(|| format!("could not create {}", parent.display()))?;
    let mut decoder = BzDecoder::new(archive.as_slice());
    let mut temporary = NamedTempFile::new_in(parent)
        .with_context(|| format!("could not create a temporary file in {}", parent.display()))?;
    io::copy(&mut decoder, temporary.as_file_mut())
        .context("could not decompress the OpenH264 archive")?;
    temporary.as_file().sync_all()?;
    temporary
        .persist(destination)
        .map_err(|error| error.error)
        .with_context(|| format!("could not atomically install {}", destination.display()))?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use bzip2::{Compression, write::BzEncoder};

    fn archive(data: &[u8]) -> (Vec<u8>, String) {
        let mut encoder = BzEncoder::new(Vec::new(), Compression::best());
        encoder.write_all(data).unwrap();
        let archive = encoder.finish().unwrap();
        let checksum = format!("{:x}", Sha256::digest(&archive));
        (archive, checksum)
    }

    #[test]
    fn install_verifies_and_atomically_replaces_the_module() {
        let directory = tempfile::tempdir().unwrap();
        let destination = directory.path().join(MODULE_NAME);
        fs::write(&destination, b"old").unwrap();
        let (archive, checksum) = archive(b"new module");
        install_archive(&mut archive.as_slice(), &checksum, &destination).unwrap();
        assert_eq!(fs::read(destination).unwrap(), b"new module");
    }

    #[test]
    fn checksum_mismatch_preserves_an_existing_module() {
        let directory = tempfile::tempdir().unwrap();
        let destination = directory.path().join(MODULE_NAME);
        fs::write(&destination, b"old").unwrap();
        let (archive, _) = archive(b"new module");
        let error =
            install_archive(&mut archive.as_slice(), &"0".repeat(64), &destination).unwrap_err();
        assert!(error.to_string().contains("checksum mismatch"));
        assert_eq!(fs::read(destination).unwrap(), b"old");
    }

    #[test]
    fn corrupt_archive_does_not_replace_an_existing_module() {
        let directory = tempfile::tempdir().unwrap();
        let destination = directory.path().join(MODULE_NAME);
        fs::write(&destination, b"old").unwrap();
        let archive = b"not bzip2";
        let checksum = format!("{:x}", Sha256::digest(archive));
        assert!(install_archive(&mut archive.as_slice(), &checksum, &destination).is_err());
        assert_eq!(fs::read(destination).unwrap(), b"old");
    }

    #[test]
    fn architecture_assets_are_pinned_to_cisco_230() {
        let asset = asset_for_arch().unwrap();
        assert!(asset.url.starts_with("https://ciscobinary.openh264.org/"));
        assert!(asset.url.contains("2.3.0"));
        assert_eq!(asset.sha256.len(), 64);
    }

    #[test]
    fn module_name_matches_the_230_binary_abi() {
        assert_eq!(
            Path::new(MODULE_NAME)
                .extension()
                .and_then(|value| value.to_str()),
            Some("6")
        );
    }
}
