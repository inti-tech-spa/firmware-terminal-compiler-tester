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
    pub system: Option<SystemToolConfig>,
}

#[derive(Debug, Clone, PartialEq, Eq, Deserialize, Serialize)]
pub struct SystemToolConfig {
    pub gcc: String,
    pub gdb: String,
    pub openocd: String,
    pub objcopy: String,
    pub objdump: String,
    pub size: String,
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
        let tools_valid = match self.tools.channel.as_str() {
            "pinned" => self.tools.system.is_none(),
            "system" => self
                .tools
                .system
                .as_ref()
                .is_some_and(SystemToolConfig::paths_are_absolute),
            _ => false,
        };
        if !tools_valid
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

impl SystemToolConfig {
    fn paths_are_absolute(&self) -> bool {
        [
            &self.gcc,
            &self.gdb,
            &self.openocd,
            &self.objcopy,
            &self.objdump,
            &self.size,
        ]
        .into_iter()
        .all(|path| Path::new(path).is_absolute())
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
        assert_eq!(error.code(), "UNSUPPORTED_PROJECT");
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

    #[test]
    fn system_tools_require_an_explicit_complete_absolute_path_set() {
        let valid = r#"
schema_version = 1
[project]
kind = "microchip-studio-cproj"
path = "firmware.cproj"
configuration = "Debug"
device = "ATSAM4SD32C"
[tools]
channel = "system"
[tools.system]
gcc = "/opt/tools/arm-none-eabi-gcc"
gdb = "/opt/tools/arm-none-eabi-gdb"
openocd = "/opt/tools/openocd"
objcopy = "/opt/tools/arm-none-eabi-objcopy"
objdump = "/opt/tools/arm-none-eabi-objdump"
size = "/opt/tools/arm-none-eabi-size"
[probe]
kind = "atmel-ice"
transport = "swd"
"#;
        let fs = FakeFs(HashMap::from([(
            PathBuf::from("samdebug.toml"),
            valid.into(),
        )]));
        let loaded = SamdebugConfig::load(&fs, Path::new("samdebug.toml"))
            .expect("explicit system tools are valid");
        assert_eq!(loaded.config.tools.channel, "system");

        let relative = valid.replace("/opt/tools/arm-none-eabi-gcc", "bin/arm-none-eabi-gcc");
        let fs = FakeFs(HashMap::from([(PathBuf::from("samdebug.toml"), relative)]));
        let error = SamdebugConfig::load(&fs, Path::new("samdebug.toml"))
            .expect_err("relative system tool must be rejected");
        assert_eq!(error.code(), "INVALID_CONFIG");
    }
}
