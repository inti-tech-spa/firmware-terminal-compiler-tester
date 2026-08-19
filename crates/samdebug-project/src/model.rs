use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct XmlLocation {
    pub file: String,
    pub line: u32,
    pub column: u32,
    pub element: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ImportWarning {
    pub code: String,
    pub message: String,
    pub location: XmlLocation,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SourceKind {
    C,
    Assembly,
    PreprocessedAssembly,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SourceInput {
    pub path: String,
    pub kind: SourceKind,
    pub link_name: Option<String>,
    pub external: bool,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[allow(clippy::struct_excessive_bools)]
pub struct ArtifactRequests {
    pub elf: bool,
    pub map: bool,
    pub bin: bool,
    pub hex: bool,
    pub disassembly: bool,
    pub eeprom: bool,
    pub srec: bool,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct BuildPlan {
    pub schema_version: u32,
    pub project_file: String,
    pub project_name: String,
    pub configuration: String,
    pub device: String,
    pub output_name: String,
    pub output_extension: String,
    pub build_directory: String,
    pub sources: Vec<SourceInput>,
    pub headers: Vec<String>,
    pub symbols: Vec<String>,
    pub include_directories: Vec<String>,
    pub compiler_flags: Vec<String>,
    pub assembler_include_directories: Vec<String>,
    pub assembler_flags: Vec<String>,
    pub libraries: Vec<String>,
    pub library_search_paths: Vec<String>,
    pub linker_script: Option<String>,
    pub linker_flags: Vec<String>,
    pub artifacts: ArtifactRequests,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ImportResult {
    pub plan: BuildPlan,
    pub warnings: Vec<ImportWarning>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct InitReport {
    pub config: String,
    pub plan: String,
    pub configuration: String,
    pub sources: usize,
    pub warnings: Vec<ImportWarning>,
}
