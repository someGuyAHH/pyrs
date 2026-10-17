use anyhow::{Result, bail};
use serde::{Deserialize, Serialize};

pub const FORMAT_VERSION: u32 = 1;

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Manifest {
    pub format_version: u32,
    pub name: String,
    pub version: String,
    pub requires_python: Option<String>,
    pub entry: Entry,
    /// File name (inside `wheels/`) of the project's own wheel.
    pub project_wheel: String,
    pub wheels: Vec<WheelEntry>,
    /// True when every wheel is `none-any`, i.e. the bundle is portable across platforms.
    pub pure: bool,
    pub built_with: BuiltWith,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Entry {
    pub kind: EntryKind,
    pub name: String,
    /// `pkg.mod:func` for console scripts, the package name for `module` entries.
    pub target: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum EntryKind {
    ConsoleScript,
    Module,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct WheelEntry {
    pub file: String,
    pub sha256: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct BuiltWith {
    pub pyrs: String,
    pub python: String,
    pub platform: String,
}

impl Manifest {
    pub fn validate(&self) -> Result<()> {
        if self.format_version != FORMAT_VERSION {
            bail!(
                "unsupported bundle format version {} (this pyrs understands {FORMAT_VERSION}); \
                 update pyrs or rebuild the bundle",
                self.format_version
            );
        }
        for wheel in &self.wheels {
            check_file_name(&wheel.file)?;
        }
        if !self.wheels.iter().any(|w| w.file == self.project_wheel) {
            bail!("manifest refers to {} but does not list it as a wheel", self.project_wheel);
        }
        Ok(())
    }
}

/// Wheel names from a manifest become paths on disk, so they must be a bare file name.
pub fn check_file_name(name: &str) -> Result<()> {
    let bare = !name.is_empty()
        && name != "."
        && name != ".."
        && !name.contains(['/', '\\', '\0'])
        && !name.contains(':');
    if !bare {
        bail!("manifest contains an unsafe file name: {name:?}");
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    pub(crate) fn sample() -> Manifest {
        Manifest {
            format_version: FORMAT_VERSION,
            name: "hello".into(),
            version: "1.2.3".into(),
            requires_python: Some(">=3.10".into()),
            entry: Entry {
                kind: EntryKind::ConsoleScript,
                name: "hello".into(),
                target: "hello.cli:main".into(),
            },
            project_wheel: "hello-1.2.3-py3-none-any.whl".into(),
            wheels: vec![
                WheelEntry { file: "hello-1.2.3-py3-none-any.whl".into(), sha256: "aa".into() },
                WheelEntry { file: "dep-26.0-py3-none-any.whl".into(), sha256: "bb".into() },
            ],
            pure: true,
            built_with: BuiltWith {
                pyrs: "0.1.0".into(),
                python: "3.13.5".into(),
                platform: "linux-x86_64".into(),
            },
        }
    }

    #[test]
    fn round_trips_through_json() {
        let m = sample();
        let json = serde_json::to_string_pretty(&m).unwrap();
        assert!(json.contains("\"console_script\""));
        assert_eq!(serde_json::from_str::<Manifest>(&json).unwrap(), m);
    }

    #[test]
    fn valid_sample_passes() {
        sample().validate().unwrap();
    }

    #[test]
    fn rejects_future_format_version() {
        let mut m = sample();
        m.format_version = FORMAT_VERSION + 1;
        let err = m.validate().unwrap_err().to_string();
        assert!(err.contains("unsupported bundle format version"), "{err}");
    }

    #[test]
    fn rejects_unlisted_project_wheel() {
        let mut m = sample();
        m.project_wheel = "other-1.0-py3-none-any.whl".into();
        assert!(m.validate().is_err());
    }

    #[test]
    fn rejects_path_traversal_in_wheel_names() {
        for bad in ["../evil.whl", "a/b.whl", "a\\b.whl", "..", "", "C:evil.whl"] {
            assert!(check_file_name(bad).is_err(), "{bad:?} should be rejected");
        }
        check_file_name("fine-1.0-py3-none-any.whl").unwrap();
    }
}
