use std::fs::{self, File};
use std::io::{Read, Seek, SeekFrom, Write};
use std::path::{Path, PathBuf};

use anyhow::{Context, Result, anyhow, bail};
use sha2::{Digest, Sha256};
use zip::write::SimpleFileOptions;
use zip::{CompressionMethod, ZipArchive, ZipWriter};

use crate::hash::{hex, sha256_hex};
use crate::manifest::{FORMAT_VERSION, Manifest};

const MANIFEST_NAME: &str = "manifest.json";

/// Writes a `.pyrs` bundle: `manifest.json` plus every manifest wheel under `wheels/`.
pub fn write(out: &Path, manifest: &Manifest, wheel_dir: &Path) -> Result<()> {
    manifest.validate()?;
    let result = write_inner(out, manifest, wheel_dir);
    if result.is_err() {
        let _ = fs::remove_file(out);
    }
    result
}

fn write_inner(out: &Path, manifest: &Manifest, wheel_dir: &Path) -> Result<()> {
    let file = File::create(out).with_context(|| format!("creating {}", out.display()))?;
    let mut zip = ZipWriter::new(file);

    let json = serde_json::to_vec_pretty(manifest)?;
    zip.start_file(
        MANIFEST_NAME,
        SimpleFileOptions::default().compression_method(CompressionMethod::Deflated),
    )?;
    zip.write_all(&json)?;

    // Wheels are zip files already, so compressing them again only costs time.
    let stored = SimpleFileOptions::default()
        .compression_method(CompressionMethod::Stored)
        .large_file(true);
    for wheel in &manifest.wheels {
        let path = wheel_dir.join(&wheel.file);
        let mut src = File::open(&path).with_context(|| format!("opening {}", path.display()))?;
        zip.start_file(format!("wheels/{}", wheel.file), stored)?;
        std::io::copy(&mut src, &mut zip)?;
    }
    zip.finish()?;
    Ok(())
}

pub struct Bundle {
    pub manifest: Manifest,
    /// SHA-256 of `manifest.json`. The manifest pins every wheel hash, so this identifies the content.
    pub digest: String,
    path: PathBuf,
    zip: ZipArchive<File>,
}

impl Bundle {
    pub fn open(path: &Path) -> Result<Self> {
        let file = File::open(path).with_context(|| format!("opening {}", path.display()))?;
        let mut zip = ZipArchive::new(file)
            .with_context(|| format!("{} is not a pyrs bundle (not a zip file)", path.display()))?;

        let mut bytes = Vec::new();
        zip.by_name(MANIFEST_NAME)
            .map_err(|_| anyhow!("{} is not a pyrs bundle (no {MANIFEST_NAME})", path.display()))?
            .read_to_end(&mut bytes)?;

        // Check the version before the full parse so a newer bundle gets a useful message.
        let raw: serde_json::Value =
            serde_json::from_slice(&bytes).context("manifest.json is not valid JSON")?;
        if let Some(version) = raw.get("format_version").and_then(|v| v.as_u64()) {
            if version != u64::from(FORMAT_VERSION) {
                bail!(
                    "unsupported bundle format version {version} (this pyrs understands \
                     {FORMAT_VERSION}); update pyrs or rebuild the bundle"
                );
            }
        }
        let manifest: Manifest =
            serde_json::from_value(raw).context("manifest.json has an unexpected shape")?;
        manifest.validate()?;

        Ok(Self { manifest, digest: sha256_hex(&bytes), path: path.to_path_buf(), zip })
    }

    /// Opens one wheel in place, without extracting it, after checking its SHA-256.
    ///
    /// Wheels are stored uncompressed, so a wheel is just a byte range of the bundle file.
    pub fn open_wheel(&mut self, file: &str) -> Result<ZipArchive<Slice>> {
        let expected = self
            .manifest
            .wheels
            .iter()
            .find(|w| w.file == file)
            .ok_or_else(|| anyhow!("{file} is not listed in the manifest"))?
            .sha256
            .clone();
        let (start, len) = {
            let entry = self
                .zip
                .by_name(&format!("wheels/{file}"))
                .map_err(|_| anyhow!("bundle is missing wheels/{file}"))?;
            if entry.compression() != CompressionMethod::Stored {
                bail!("wheels/{file} is compressed inside the bundle; rebuild it with this pyrs");
            }
            let start = entry.data_start().ok_or_else(|| anyhow!("cannot locate wheels/{file} in the bundle"))?;
            (start, entry.size())
        };
        let corrupted = || format!("reading {file} from the bundle: the bundle is corrupted");

        let mut slice = Slice::new(File::open(&self.path).with_context(corrupted)?, start, len);
        let mut hasher = Sha256::new();
        let mut buf = vec![0u8; 64 * 1024];
        loop {
            let n = slice.read(&mut buf).with_context(corrupted)?;
            if n == 0 {
                break;
            }
            hasher.update(&buf[..n]);
        }
        if slice.pos != len || hex(&hasher.finalize()) != expected {
            bail!("checksum mismatch for {file}: the bundle is corrupted or was modified");
        }
        slice.seek(SeekFrom::Start(0))?;
        ZipArchive::new(slice).with_context(|| format!("{file} in the bundle is not a valid wheel"))
    }
}

/// A read-only window onto part of a file.
pub struct Slice {
    file: File,
    start: u64,
    len: u64,
    pos: u64,
}

impl Slice {
    fn new(file: File, start: u64, len: u64) -> Self {
        Self { file, start, len, pos: 0 }
    }
}

impl Read for Slice {
    fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
        let left = self.len - self.pos;
        if left == 0 {
            return Ok(0);
        }
        let want = buf.len().min(usize::try_from(left).unwrap_or(usize::MAX));
        self.file.seek(SeekFrom::Start(self.start + self.pos))?;
        let n = self.file.read(&mut buf[..want])?;
        self.pos += n as u64;
        Ok(n)
    }
}

impl Seek for Slice {
    fn seek(&mut self, to: SeekFrom) -> std::io::Result<u64> {
        let target = match to {
            SeekFrom::Start(n) => i128::from(n),
            SeekFrom::End(n) => i128::from(self.len) + i128::from(n),
            SeekFrom::Current(n) => i128::from(self.pos) + i128::from(n),
        };
        if target < 0 {
            return Err(std::io::Error::new(std::io::ErrorKind::InvalidInput, "seek before start"));
        }
        self.pos = u64::try_from(target).unwrap_or(u64::MAX).min(self.len);
        Ok(self.pos)
    }
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;
    use crate::manifest::{BuiltWith, Entry, EntryKind, WheelEntry};

    /// A real (tiny) wheel: a zip with the given `(name, contents)` entries.
    pub(crate) fn wheel_bytes(files: &[(&str, &str)]) -> Vec<u8> {
        let mut zip = ZipWriter::new(std::io::Cursor::new(Vec::new()));
        for (name, contents) in files {
            zip.start_file(*name, SimpleFileOptions::default()).unwrap();
            zip.write_all(contents.as_bytes()).unwrap();
        }
        zip.finish().unwrap().into_inner()
    }

    /// Writes two wheels into `dir` and returns a manifest describing them.
    fn fixture(dir: &Path) -> Manifest {
        let files = [
            ("app-1.0-py3-none-any.whl", wheel_bytes(&[("app/__init__.py", "x = 1\n")])),
            ("dep-2.0-py3-none-any.whl", wheel_bytes(&[("dep.py", "y = 2\n")])),
        ];
        let mut wheels = Vec::new();
        for (name, bytes) in &files {
            fs::write(dir.join(name), bytes).unwrap();
            wheels.push(WheelEntry { file: (*name).into(), sha256: sha256_hex(bytes) });
        }
        Manifest {
            format_version: FORMAT_VERSION,
            name: "app".into(),
            version: "1.0".into(),
            requires_python: None,
            entry: Entry { kind: EntryKind::ConsoleScript, name: "app".into(), target: "app:main".into() },
            project_wheel: "app-1.0-py3-none-any.whl".into(),
            wheels,
            pure: true,
            built_with: BuiltWith { pyrs: "t".into(), python: "3.13.0".into(), platform: "x".into() },
        }
    }

    fn written(tmp: &Path, manifest: &Manifest, src: &Path) -> PathBuf {
        let out = tmp.join("app.pyrs");
        write(&out, manifest, src).unwrap();
        out
    }

    fn setup() -> (tempfile::TempDir, PathBuf, Manifest) {
        let tmp = tempfile::tempdir().unwrap();
        let src = tmp.path().join("src");
        fs::create_dir(&src).unwrap();
        let manifest = fixture(&src);
        (tmp, src, manifest)
    }

    #[test]
    fn write_open_read_round_trip() {
        let (tmp, src, manifest) = setup();
        let out = written(tmp.path(), &manifest, &src);

        let mut bundle = Bundle::open(&out).unwrap();
        assert_eq!(bundle.manifest, manifest);
        assert_eq!(bundle.digest.len(), 64);

        let mut wheel = bundle.open_wheel("dep-2.0-py3-none-any.whl").unwrap();
        let mut text = String::new();
        wheel.by_name("dep.py").unwrap().read_to_string(&mut text).unwrap();
        assert_eq!(text, "y = 2\n");
        // Opening one wheel doesn't disturb the others.
        assert!(bundle.open_wheel("app-1.0-py3-none-any.whl").unwrap().by_name("app/__init__.py").is_ok());
    }

    #[test]
    fn detects_a_tampered_wheel() {
        let (tmp, src, mut manifest) = setup();
        manifest.wheels[0].sha256 = sha256_hex(b"something else");
        let out = written(tmp.path(), &manifest, &src);
        let err = Bundle::open(&out).unwrap().open_wheel(&manifest.wheels[0].file).err().unwrap().to_string();
        assert!(err.contains("checksum mismatch"), "{err}");
    }

    #[test]
    fn unlisted_wheels_cannot_be_opened() {
        let (tmp, src, manifest) = setup();
        let out = written(tmp.path(), &manifest, &src);
        assert!(Bundle::open(&out).unwrap().open_wheel("other-1-py3-none-any.whl").is_err());
    }

    #[test]
    fn same_content_gives_same_digest() {
        let (tmp, src, manifest) = setup();
        let (a, b) = (tmp.path().join("a.pyrs"), tmp.path().join("b.pyrs"));
        write(&a, &manifest, &src).unwrap();
        write(&b, &manifest, &src).unwrap();
        assert_eq!(Bundle::open(&a).unwrap().digest, Bundle::open(&b).unwrap().digest);
    }

    #[test]
    fn rejects_files_that_are_not_bundles() {
        let tmp = tempfile::tempdir().unwrap();
        let text = tmp.path().join("notes.pyrs");
        fs::write(&text, "hello").unwrap();
        let err = Bundle::open(&text).err().unwrap().to_string();
        assert!(err.contains("not a pyrs bundle"), "{err}");

        let empty = tmp.path().join("empty.pyrs");
        fs::write(&empty, wheel_bytes(&[("readme.txt", "")])).unwrap();
        let err = Bundle::open(&empty).err().unwrap().to_string();
        assert!(err.contains("no manifest.json"), "{err}");
    }

    #[test]
    fn newer_format_gets_a_clear_error() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("future.pyrs");
        fs::write(&path, wheel_bytes(&[(MANIFEST_NAME, r#"{"format_version": 99, "new": true}"#)])).unwrap();
        let err = Bundle::open(&path).err().unwrap().to_string();
        assert!(err.contains("unsupported bundle format version 99"), "{err}");
    }

    #[test]
    fn hostile_wheel_names_are_rejected() {
        let (tmp, src, mut manifest) = setup();
        manifest.wheels[0].file = "../escape.whl".into();
        manifest.project_wheel = "../escape.whl".into();
        let out = tmp.path().join("evil.pyrs");
        assert!(write(&out, &manifest, &src).is_err());
        assert!(!out.exists());
    }
}
