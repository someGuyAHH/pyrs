use std::fs::File;
use std::io::Read;
use std::path::Path;

use anyhow::{Context, Result, anyhow, bail};
use zip::ZipArchive;

/// The parts of a wheel file name (PEP 427) that pyrs cares about.
#[derive(Debug, PartialEq, Eq)]
pub struct WheelFilename {
    pub distribution: String,
    pub version: String,
    pub python_tag: String,
    pub abi_tag: String,
    pub platform_tag: String,
}

impl WheelFilename {
    pub fn parse(file_name: &str) -> Result<Self> {
        let stem = file_name
            .strip_suffix(".whl")
            .ok_or_else(|| anyhow!("{file_name} is not a wheel (no .whl suffix)"))?;
        let parts: Vec<&str> = stem.split('-').collect();
        // {dist}-{version}(-{build})?-{python}-{abi}-{platform}
        if parts.len() != 5 && parts.len() != 6 {
            bail!("{file_name} is not a valid wheel file name");
        }
        let n = parts.len();
        Ok(Self {
            distribution: parts[0].to_string(),
            version: parts[1].to_string(),
            python_tag: parts[n - 3].to_string(),
            abi_tag: parts[n - 2].to_string(),
            platform_tag: parts[n - 1].to_string(),
        })
    }

    /// Platform-independent wheels carry no compiled code.
    pub fn is_pure(&self) -> bool {
        self.abi_tag == "none" && self.platform_tag == "any"
    }

    /// Whether the wheel's ABI can load in CPython `major.minor`. Unknown ABIs (PyPy, …) pass:
    /// this check exists to explain failures early, not to second-guess every tag.
    pub fn abi_compatible(&self, major: u64, minor: u64) -> bool {
        let host = major * 100 + minor;
        let cp_version = |tag: &str| -> Option<u64> {
            let digits = tag.strip_prefix("cp")?.trim_end_matches(|c: char| c.is_ascii_alphabetic());
            let (maj, min) = digits.split_at_checked(1)?;
            Some(maj.parse::<u64>().ok()? * 100 + min.parse::<u64>().ok()?)
        };
        self.abi_tag.split('.').any(|abi| match abi {
            "none" => true,
            // abi3 wheels work on their minimum Python version and every later one.
            "abi3" => self.python_tag.split('.').filter_map(cp_version).any(|min| min <= host),
            cp if cp.starts_with("cp") => cp_version(cp) == Some(host),
            _ => true,
        })
    }
}

/// Metadata and entry points read out of a built wheel.
#[derive(Debug, Default, PartialEq, Eq)]
pub struct WheelInfo {
    pub name: String,
    pub version: String,
    pub requires_python: Option<String>,
    /// `(script name, "pkg.mod:func")`, console scripts first, then gui scripts.
    pub scripts: Vec<(String, String)>,
    /// Top-level packages that contain a `__main__.py`.
    pub runnable_packages: Vec<String>,
}

impl WheelInfo {
    pub fn read(path: &Path) -> Result<Self> {
        let file = File::open(path).with_context(|| format!("opening {}", path.display()))?;
        let mut zip = ZipArchive::new(file)
            .with_context(|| format!("{} is not a valid wheel (bad zip)", path.display()))?;

        let mut info = WheelInfo::default();
        let mut metadata = None;
        let mut entry_points = None;
        for i in 0..zip.len() {
            let name = zip.by_index_raw(i)?.name().to_string();
            let mut parts = name.split('/');
            match (parts.next(), parts.next(), parts.next()) {
                (Some(top), Some("METADATA"), None) if top.ends_with(".dist-info") => {
                    metadata = Some(name.clone());
                }
                (Some(top), Some("entry_points.txt"), None) if top.ends_with(".dist-info") => {
                    entry_points = Some(name.clone());
                }
                (Some(pkg), Some("__main__.py"), None) if !pkg.contains('.') => {
                    info.runnable_packages.push(pkg.to_string());
                }
                _ => {}
            }
        }

        let metadata = metadata.ok_or_else(|| anyhow!("{} has no METADATA", path.display()))?;
        let mut text = String::new();
        zip.by_name(&metadata)?.read_to_string(&mut text)?;
        parse_metadata(&text, &mut info)?;

        if let Some(entry_points) = entry_points {
            let mut text = String::new();
            zip.by_name(&entry_points)?.read_to_string(&mut text)?;
            info.scripts = parse_scripts(&text);
        }
        Ok(info)
    }
}

fn parse_metadata(text: &str, info: &mut WheelInfo) -> Result<()> {
    let mut name = None;
    let mut version = None;
    // Headers end at the first blank line; the rest is the long description.
    for line in text.lines().take_while(|l| !l.trim().is_empty()) {
        let Some((key, value)) = line.split_once(':') else { continue };
        let value = value.trim();
        match key {
            "Name" => name = Some(value.to_string()),
            "Version" => version = Some(value.to_string()),
            "Requires-Python" => info.requires_python = Some(value.to_string()),
            _ => {}
        }
    }
    info.name = name.ok_or_else(|| anyhow!("wheel METADATA has no Name"))?;
    info.version = version.ok_or_else(|| anyhow!("wheel METADATA has no Version"))?;
    Ok(())
}

/// Reads `[console_scripts]` and `[gui_scripts]` out of an `entry_points.txt`.
/// PEP 503 name normalization: `My_App.Core` and `my-app-core` are the same project.
pub fn normalize(name: &str) -> String {
    let mut out = String::new();
    for c in name.chars().flat_map(char::to_lowercase) {
        if matches!(c, '-' | '_' | '.') {
            if !out.ends_with('-') {
                out.push('-');
            }
        } else {
            out.push(c);
        }
    }
    out
}

pub fn parse_scripts(text: &str) -> Vec<(String, String)> {
    let mut console = Vec::new();
    let mut gui = Vec::new();
    let mut section = "";
    for line in text.lines() {
        let line = line.trim();
        if line.is_empty() || line.starts_with(['#', ';']) {
            continue;
        }
        if let Some(header) = line.strip_prefix('[').and_then(|l| l.strip_suffix(']')) {
            section = header;
            continue;
        }
        let target = match section {
            "console_scripts" => &mut console,
            "gui_scripts" => &mut gui,
            _ => continue,
        };
        if let Some((name, spec)) = line.split_once('=') {
            // Drop an optional `[extra]` suffix: `pkg.mod:func [extra]`.
            let spec = spec.split('[').next().unwrap_or(spec).trim();
            target.push((name.trim().to_string(), spec.to_string()));
        }
    }
    console.extend(gui);
    console
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_pure_and_native_names() {
        let pure = WheelFilename::parse("requests-2.32.3-py3-none-any.whl").unwrap();
        assert!(pure.is_pure());
        assert_eq!(pure.version, "2.32.3");

        let native =
            WheelFilename::parse("numpy-2.1.0-cp313-cp313-manylinux_2_17_x86_64.whl").unwrap();
        assert!(!native.is_pure());
        assert_eq!(native.python_tag, "cp313");

        // Optional build tag.
        let built = WheelFilename::parse("pkg-1.0-1-py3-none-any.whl").unwrap();
        assert!(built.is_pure());
        assert_eq!(built.distribution, "pkg");
    }

    #[test]
    fn abi3_wheels_are_not_pure() {
        assert!(!WheelFilename::parse("x-1-cp39-abi3-win_amd64.whl").unwrap().is_pure());
        assert!(!WheelFilename::parse("x-1-py3-none-linux_x86_64.whl").unwrap().is_pure());
    }

    #[test]
    fn abi_compatibility() {
        let parse = |n| WheelFilename::parse(n).unwrap();
        assert!(parse("a-1-py3-none-any.whl").abi_compatible(3, 13));
        assert!(parse("a-1-cp313-cp313-manylinux_2_17_x86_64.whl").abi_compatible(3, 13));
        assert!(!parse("a-1-cp313-cp313-manylinux_2_17_x86_64.whl").abi_compatible(3, 12));
        assert!(parse("a-1-cp39-abi3-win_amd64.whl").abi_compatible(3, 13));
        assert!(!parse("a-1-cp311-abi3-win_amd64.whl").abi_compatible(3, 10));
        assert!(parse("a-1-cp310-cp310-linux_x86_64.whl").abi_compatible(3, 10));
    }

    #[test]
    fn rejects_non_wheels() {
        assert!(WheelFilename::parse("pkg-1.0.tar.gz").is_err());
        assert!(WheelFilename::parse("pkg-1.0.whl").is_err());
    }

    #[test]
    fn parses_scripts_in_order() {
        let text = "\
[console_scripts]
hello = hello.cli:main
other = hello.other:run [extra]

[gui_scripts]
hello-gui = hello.gui:start

[some.plugin]
ignored = a:b
";
        assert_eq!(
            parse_scripts(text),
            vec![
                ("hello".to_string(), "hello.cli:main".to_string()),
                ("other".to_string(), "hello.other:run".to_string()),
                ("hello-gui".to_string(), "hello.gui:start".to_string()),
            ]
        );
    }

    #[test]
    fn metadata_headers_stop_at_blank_line() {
        let text = "Metadata-Version: 2.4\nName: hello\nVersion: 1.0\nRequires-Python: >=3.10\n\nName: not-a-header\n";
        let mut info = WheelInfo::default();
        parse_metadata(text, &mut info).unwrap();
        assert_eq!(info.name, "hello");
        assert_eq!(info.version, "1.0");
        assert_eq!(info.requires_python.as_deref(), Some(">=3.10"));
    }
}
