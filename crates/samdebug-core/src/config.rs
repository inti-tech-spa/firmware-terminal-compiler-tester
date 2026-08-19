use std::path::Path;

use serde::{Deserialize, Serialize};

use crate::{Diagnostic, ErrorCategory, SamdebugError, SamdebugResult, ports::FileSystem};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize, Serialize)]
pub enum Configuration {
    Debug,
    Release,
}

#[derive(Debug, Clone, PartialEq, Eq, Deserialize, Serialize)]
pub struct ProjectConfig {
    pub kind: String,
    pub path: String,
    pub configuration: Configuration,
    pub device: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Deserialize, Serialize)]
pub struct ToolConfig {
    pub channel: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Deserialize, Serialize)]
pub struct ProbeConfig {
    pub kind: String,
    pub transport: String,
    #[serde(default = "default_speed")]
    pub speed_khz: u32,
    pub serial: Option<String>,
}

const fn default_speed() -> u32 {
    1_000
}

#[derive(Debug, Clone, PartialEq, Eq, Deserialize, Serialize)]
pub struct SamdebugConfig {
    pub schema_version: u32,
    pub project: ProjectConfig,
    pub tools: ToolConfig,
    pub probe: ProbeConfig,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LoadedConfig {
    pub config: SamdebugConfig,
    pub warnings: Vec<Diagnostic>,
}

impl SamdebugConfig {
    pub fn load(fs: &dyn FileSystem, path: &Path) -> SamdebugResult<LoadedConfig> {
        let text = fs.read_to_string(path)?;
        let mut ignored = Vec::new();
        let deserializer = toml::Deserializer::parse(&text).map_err(|error| {
            SamdebugError::new(ErrorCategory::Command, "INVALID_CONFIG", error.to_string())
        })?;
        let config: Self =
            serde_ignored::deserialize(deserializer, |path| ignored.push(path.to_string()))
                .map_err(|error| {
                    SamdebugError::new(ErrorCategory::Command, "INVALID_CONFIG", error.to_string())
                })?;
        config.validate()?;
        let warnings = ignored
            .into_iter()
            .map(|location| Diagnostic {
                code: "UNKNOWN_CONFIG_KEY".into(),
                message: "unknown samdebug.toml key was ignored".into(),
                location: Some(location),
            })
            .collect();
        Ok(LoadedConfig { config, warnings })
    }

    pub fn validate(&self) -> SamdebugResult<()> {
        if self.schema_version != 1 {
            return Err(SamdebugError::new(
                ErrorCategory::Command,
                "UNSUPPORTED_CONFIG_SCHEMA",
                "samdebug.toml schema_version must be 1",
            ));
        }
        if self.project.kind != "microchip-studio-cproj" || self.project.device != "ATSAM4SD32C" {
            return Err(SamdebugError::new(
                ErrorCategory::Command,
                "UNSUPPORTED_PROJECT",
                "v1 requires a Microchip Studio ATSAM4SD32C project",
            ));
        }
        if self.tools.channel != "pinned"
            || self.probe.kind != "atmel-ice"
            || self.probe.transport != "swd"
            || self.probe.speed_khz == 0
        {
            return Err(SamdebugError::new(
                ErrorCategory::Command,
                "INVALID_CONFIG",
                "invalid tool or probe configuration",
            ));
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use std::{
        collections::HashMap,
        path::{Path, PathBuf},
    };

    use crate::{ErrorCategory, SamdebugConfig, SamdebugError, SamdebugResult, ports::FileSystem};

    #[derive(Debug)]
    struct FakeFs(HashMap<PathBuf, String>);

    impl FileSystem for FakeFs {
        fn read_to_string(&self, path: &Path) -> SamdebugResult<String> {
            self.0
                .get(path)
                .cloned()
                .ok_or_else(|| SamdebugError::new(ErrorCategory::Command, "NOT_FOUND", "missing"))
        }
    }

    #[test]
    fn loads_and_defaults_valid_config() {
        let text = r#"
schema_version = 1
[project]
kind = "microchip-studio-cproj"
path = "firmware.cproj"
configuration = "Debug"
device = "ATSAM4SD32C"
[tools]
channel = "pinned"
[probe]
kind = "atmel-ice"
transport = "swd"
"#;
        let fs = FakeFs(HashMap::from([(
            PathBuf::from("samdebug.toml"),
            text.into(),
        )]));
        let loaded = SamdebugConfig::load(&fs, Path::new("samdebug.toml")).expect("valid config");
        assert_eq!(loaded.config.probe.speed_khz, 1_000);
        assert!(loaded.warnings.is_empty());
    }

    #[test]
    fn rejects_wrong_device() {
        let text = r#"
schema_version = 1
[project]
kind = "microchip-studio-cproj"
path = "firmware.cproj"
configuration = "Debug"
device = "ATSAM4E"
[tools]
channel = "pinned"
[probe]
kind = "atmel-ice"
transport = "swd"
"#;
        let fs = FakeFs(HashMap::from([(
            PathBuf::from("samdebug.toml"),
            text.into(),
        )]));
        let error =
            SamdebugConfig::load(&fs, Path::new("samdebug.toml")).expect_err("reject device");
        assert_eq!(error.code, "UNSUPPORTED_PROJECT");
    }

    #[test]
    fn unknown_keys_are_reported_as_warnings() {
        let text = r#"
schema_version = 1
future_key = true
[project]
kind = "microchip-studio-cproj"
path = "firmware.cproj"
configuration = "Debug"
device = "ATSAM4SD32C"
[tools]
channel = "pinned"
[probe]
kind = "atmel-ice"
transport = "swd"
"#;
        let fs = FakeFs(HashMap::from([(
            PathBuf::from("samdebug.toml"),
            text.into(),
        )]));
        let loaded = SamdebugConfig::load(&fs, Path::new("samdebug.toml")).expect("valid config");
        assert_eq!(loaded.warnings.len(), 1);
        assert_eq!(loaded.warnings[0].code, "UNKNOWN_CONFIG_KEY");
    }
}
