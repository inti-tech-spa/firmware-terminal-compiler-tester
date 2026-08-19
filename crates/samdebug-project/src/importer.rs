use std::{
    collections::BTreeSet,
    fs::{self, OpenOptions},
    io::Write,
    path::{Component, Path},
};

use roxmltree::{Document, Node};
use samdebug_core::{
    Configuration, ErrorCategory, ProbeConfig, ProjectConfig, SamdebugConfig, SamdebugError,
    SamdebugResult, ToolConfig,
};
use serde_json::json;

use crate::model::{
    ArtifactRequests, BuildPlan, ImportResult, ImportWarning, InitReport, SourceInput, SourceKind,
    XmlLocation,
};

const STANDARD_IMPORT: &str = "$(AVRSTUDIO_EXE_PATH)/Vs/Compiler.targets";

#[allow(clippy::too_many_lines)]
pub fn import_cproj(path: &Path, configuration: Configuration) -> SamdebugResult<ImportResult> {
    let bytes = fs::read(path).map_err(|error| project_io("PROJECT_READ_FAILED", &error))?;
    let text = std::str::from_utf8(&bytes).map_err(|error| {
        SamdebugError::new(
            ErrorCategory::Project,
            "PROJECT_NOT_UTF8",
            error.to_string(),
        )
    })?;
    let text = text.trim_start_matches('\u{feff}');
    let document = Document::parse(text).map_err(|error| {
        SamdebugError::new(
            ErrorCategory::Project,
            "PROJECT_XML_INVALID",
            error.to_string(),
        )
        .with_details(json!({
            "file": path.display().to_string(),
            "line": error.pos().row,
            "column": error.pos().col
        }))
    })?;
    let root = document.root_element();
    if root.tag_name().name() != "Project" {
        return Err(node_error(
            path,
            &document,
            root,
            "UNSUPPORTED_PROJECT",
            "root element must be Project",
        ));
    }
    validate_unsupported_constructs(path, &document, root)?;
    let base = root
        .children()
        .find(|node| node.has_tag_name("PropertyGroup") && node.attribute("Condition").is_none())
        .ok_or_else(|| {
            node_error(
                path,
                &document,
                root,
                "PROJECT_METADATA_MISSING",
                "unconditional PropertyGroup is required",
            )
        })?;
    validate_identity(path, &document, base)?;
    let configuration_name = match configuration {
        Configuration::Debug => "Debug",
        Configuration::Release => "Release",
    };
    let groups: Vec<_> = root
        .children()
        .filter(|node| {
            node.has_tag_name("PropertyGroup")
                && node
                    .attribute("Condition")
                    .and_then(parse_configuration_condition)
                    == Some(configuration_name)
        })
        .collect();
    if groups.len() != 1 {
        return Err(node_error(
            path,
            &document,
            root,
            "CONFIGURATION_UNRESOLVED",
            format!("expected exactly one {configuration_name} configuration group"),
        ));
    }
    let settings = groups[0];
    validate_conflicting_scalars(
        path,
        &document,
        base,
        &[
            "Name",
            "OutputFileName",
            "OutputFileExtension",
            "ToolchainName",
            "Language",
            "OutputType",
            "avrdevice",
        ],
    )?;
    validate_conflicting_scalars(
        path,
        &document,
        settings,
        &[
            "armgcc.compiler.optimization.level",
            "armgcc.compiler.optimization.DebugLevel",
            "armgcc.compiler.miscellaneous.OtherFlags",
            "armgcc.compiler.optimization.OtherFlags",
            "armgcc.preprocessingassembler.general.AssemblerFlags",
            "armgcc.preprocessingassembler.debugging.DebugLevel",
            "armgcc.linker.miscellaneous.LinkerFlags",
            "armgcc.linker.optimization.GarbageCollectUnusedSections",
            "armgcc.common.outputfiles.bin",
            "armgcc.common.outputfiles.hex",
            "armgcc.common.outputfiles.lss",
            "armgcc.common.outputfiles.eep",
            "armgcc.common.outputfiles.srec",
        ],
    )?;
    let project_root = path.parent().unwrap_or_else(|| Path::new("."));
    let canonical_root = project_root
        .canonicalize()
        .map_err(|error| project_io("PROJECT_ROOT_INVALID", &error))?;
    let project_name = child_text(base, "Name")
        .or_else(|| path.file_stem().and_then(|name| name.to_str()))
        .ok_or_else(|| {
            node_error(
                path,
                &document,
                base,
                "PROJECT_NAME_MISSING",
                "missing Name",
            )
        })?
        .to_owned();
    let mut warnings = Vec::new();
    let include_directories = parse_include_paths(
        path,
        &document,
        settings,
        "armgcc.compiler.directories.IncludePaths",
        project_root,
        &mut warnings,
    )?;
    let assembler_include_directories = parse_include_paths(
        path,
        &document,
        settings,
        "armgcc.assembler.general.IncludePaths",
        project_root,
        &mut warnings,
    )?;
    let (sources, headers) = parse_items(
        path,
        &document,
        root,
        project_root,
        &canonical_root,
        &project_name,
        configuration_name,
    )?;
    let mut compiler_flags = tokenize_node(
        path,
        &document,
        descendant(settings, "armgcc.compiler.miscellaneous.OtherFlags"),
        &project_name,
        configuration_name,
    )?;
    append_label_flag(
        settings,
        "armgcc.compiler.optimization.level",
        &["-O0", "-O1", "-O2", "-O3", "-Os", "-Og"],
        &mut compiler_flags,
    );
    append_label_flag(
        settings,
        "armgcc.compiler.optimization.DebugLevel",
        &["-g", "-g1", "-g2", "-g3"],
        &mut compiler_flags,
    );
    append_unique(
        &mut compiler_flags,
        tokenize_node(
            path,
            &document,
            descendant(settings, "armgcc.compiler.optimization.OtherFlags"),
            &project_name,
            configuration_name,
        )?,
    );
    let mut assembler_flags = tokenize_node(
        path,
        &document,
        descendant(
            settings,
            "armgcc.preprocessingassembler.general.AssemblerFlags",
        ),
        &project_name,
        configuration_name,
    )?;
    append_label_flag(
        settings,
        "armgcc.preprocessingassembler.debugging.DebugLevel",
        &["-g", "-g1", "-g2", "-g3", "-Wa,-g"],
        &mut assembler_flags,
    );
    let mut linker_flags = tokenize_node(
        path,
        &document,
        descendant(settings, "armgcc.linker.miscellaneous.LinkerFlags"),
        &project_name,
        configuration_name,
    )?;
    if bool_setting(
        settings,
        "armgcc.linker.optimization.GarbageCollectUnusedSections",
    ) {
        push_unique(&mut linker_flags, "-Wl,--gc-sections".into());
    }
    let linker_node = descendant(settings, "armgcc.linker.miscellaneous.LinkerFlags");
    let linker_script = normalize_linker_script(path, &document, linker_node, &mut linker_flags)?;
    let output_node = descendant(base, "OutputFileName");
    let output_name = expand_value(
        output_node
            .and_then(|node| node.text())
            .unwrap_or("$(MSBuildProjectName)"),
        &project_name,
        configuration_name,
    )
    .map_err(|error| expression_error(path, &document, output_node.unwrap_or(base), &error))?;
    let extension_node = descendant(base, "OutputFileExtension");
    let output_extension = expand_value(
        extension_node
            .and_then(|node| node.text())
            .unwrap_or(".elf"),
        &project_name,
        configuration_name,
    )
    .map_err(|error| expression_error(path, &document, extension_node.unwrap_or(base), &error))?;
    Ok(ImportResult {
        plan: BuildPlan {
            schema_version: 1,
            project_file: path
                .file_name()
                .and_then(|name| name.to_str())
                .unwrap_or_default()
                .into(),
            project_name: project_name.clone(),
            configuration: configuration_name.into(),
            device: "ATSAM4SD32C".into(),
            output_name,
            output_extension,
            build_directory: format!(".samdebug/build/{configuration_name}"),
            sources,
            headers,
            symbols: checked_values(
                path,
                &document,
                settings,
                "armgcc.compiler.symbols.DefSymbols",
                &project_name,
                configuration_name,
            )?,
            include_directories,
            compiler_flags,
            assembler_include_directories,
            assembler_flags,
            libraries: checked_values(
                path,
                &document,
                settings,
                "armgcc.linker.libraries.Libraries",
                &project_name,
                configuration_name,
            )?,
            library_search_paths: normalize_paths(
                path,
                &document,
                values_nodes(settings, "armgcc.linker.libraries.LibrarySearchPaths"),
                &project_name,
                configuration_name,
            )?,
            linker_script,
            linker_flags,
            artifacts: ArtifactRequests {
                elf: true,
                map: true,
                bin: bool_setting(settings, "armgcc.common.outputfiles.bin"),
                hex: bool_setting(settings, "armgcc.common.outputfiles.hex"),
                disassembly: bool_setting(settings, "armgcc.common.outputfiles.lss"),
                eeprom: bool_setting(settings, "armgcc.common.outputfiles.eep"),
                srec: bool_setting(settings, "armgcc.common.outputfiles.srec"),
            },
        },
        warnings,
    })
}

pub fn initialize_project(path: &Path, configuration: Configuration) -> SamdebugResult<InitReport> {
    let imported = import_cproj(path, configuration)?;
    let root = path.parent().unwrap_or_else(|| Path::new("."));
    let config_path = root.join("samdebug.toml");
    if config_path.exists() {
        return Err(SamdebugError::new(
            ErrorCategory::Command,
            "CONFIG_ALREADY_EXISTS",
            format!("{} already exists", config_path.display()),
        ));
    }
    let state = root.join(".samdebug");
    let canonical_root = root
        .canonicalize()
        .map_err(|error| project_io("PROJECT_ROOT_INVALID", &error))?;
    if let Ok(metadata) = fs::symlink_metadata(&state)
        && (metadata.file_type().is_symlink() || !metadata.is_dir())
    {
        return Err(SamdebugError::new(
            ErrorCategory::Project,
            "UNSAFE_STATE_DIRECTORY",
            ".samdebug must be a real project-local directory",
        ));
    }
    fs::create_dir_all(&state).map_err(|error| project_io("STATE_DIRECTORY_FAILED", &error))?;
    let canonical_state = state
        .canonicalize()
        .map_err(|error| project_io("STATE_DIRECTORY_INVALID", &error))?;
    if !canonical_state.starts_with(&canonical_root) {
        return Err(SamdebugError::new(
            ErrorCategory::Project,
            "STATE_DIRECTORY_ESCAPE",
            ".samdebug resolves outside the project",
        ));
    }
    let plan_path = state.join("import-plan.json");
    let config = SamdebugConfig {
        schema_version: 1,
        project: ProjectConfig {
            kind: "microchip-studio-cproj".into(),
            path: path
                .file_name()
                .and_then(|name| name.to_str())
                .unwrap_or_default()
                .into(),
            configuration,
            device: "ATSAM4SD32C".into(),
        },
        tools: ToolConfig {
            channel: "pinned".into(),
            system: None,
        },
        probe: ProbeConfig {
            kind: "atmel-ice".into(),
            transport: "swd".into(),
            speed_khz: 1_000,
            serial: None,
        },
    };
    let config_bytes = toml::to_string_pretty(&config).map_err(|error| {
        SamdebugError::new(
            ErrorCategory::Command,
            "CONFIG_ENCODE_FAILED",
            error.to_string(),
        )
    })?;
    let plan_bytes = serde_json::to_vec_pretty(&imported.plan).map_err(|error| {
        SamdebugError::new(
            ErrorCategory::Project,
            "PLAN_ENCODE_FAILED",
            error.to_string(),
        )
    })?;
    write_atomic_new(&plan_path, &plan_bytes)?;
    if let Err(error) = write_atomic_new(&config_path, config_bytes.as_bytes()) {
        let _ = fs::remove_file(&plan_path);
        return Err(error);
    }
    Ok(InitReport {
        config: config_path.display().to_string(),
        plan: plan_path.display().to_string(),
        configuration: imported.plan.configuration,
        sources: imported.plan.sources.len(),
        warnings: imported.warnings,
    })
}

fn validate_identity(
    path: &Path,
    document: &Document<'_>,
    base: Node<'_, '_>,
) -> SamdebugResult<()> {
    for (tag, expected, code) in [
        (
            "ToolchainName",
            "com.Atmel.ARMGCC.C",
            "UNSUPPORTED_TOOLCHAIN",
        ),
        ("Language", "C", "UNSUPPORTED_LANGUAGE"),
        ("OutputType", "Executable", "UNSUPPORTED_OUTPUT_TYPE"),
        ("avrdevice", "ATSAM4SD32C", "UNSUPPORTED_DEVICE"),
    ] {
        let node = descendant(base, tag)
            .ok_or_else(|| node_error(path, document, base, code, format!("missing {tag}")))?;
        if node.text().unwrap_or_default().trim() != expected {
            return Err(node_error(
                path,
                document,
                node,
                code,
                format!("{tag} must be {expected}"),
            ));
        }
    }
    Ok(())
}

#[allow(clippy::too_many_lines)]
fn validate_unsupported_constructs(
    path: &Path,
    document: &Document<'_>,
    root: Node<'_, '_>,
) -> SamdebugResult<()> {
    for node in root.descendants().filter(Node::is_element) {
        let name = node.tag_name().name();
        let lower = name.to_ascii_lowercase();
        if name == "Target"
            || name == "Exec"
            || lower == "makefile"
            || lower == "externalmakefile"
            || lower.contains("prebuild")
            || lower.contains("postbuild")
            || lower.contains("custombuild")
        {
            return Err(node_error(
                path,
                document,
                node,
                "IMPORTED_BUILD_HOOK_REJECTED",
                "pre/post/custom build commands are never executed; remove the hook or reproduce it as explicit source/build settings",
            ));
        }
        if let Some(include) = node.attribute("Include")
            && (include.contains('*') || include.contains('?'))
        {
            return Err(node_error(
                path,
                document,
                node,
                "WILDCARD_INPUT_REJECTED",
                "replace wildcard inputs with explicit project items",
            ));
        }
        if let Some(condition) = node.attribute("Condition") {
            if parse_configuration_condition(condition).is_none() {
                return Err(node_error(
                    path,
                    document,
                    node,
                    "UNRESOLVED_CONDITION",
                    format!("unsupported MSBuild condition: {condition}"),
                ));
            }
            let supported_scope =
                name == "PropertyGroup" && node.parent().is_some_and(|parent| parent == root);
            if !supported_scope {
                return Err(node_error(
                    path,
                    document,
                    node,
                    "CONDITION_SCOPE_UNSUPPORTED",
                    "conditions are supported only on top-level configuration PropertyGroup elements",
                ));
            }
        }
        if name == "Import" {
            let imported = normalize_separators(node.attribute("Project").unwrap_or_default());
            if collapse_slashes(&imported) != STANDARD_IMPORT {
                return Err(node_error(
                    path,
                    document,
                    node,
                    "CUSTOM_MSBUILD_IMPORT_REJECTED",
                    "only the standard Microchip Studio Compiler.targets import is recognized and it is never executed",
                ));
            }
        }
        for value in node
            .attributes()
            .map(|attribute| attribute.value())
            .chain(node.text())
        {
            if value.contains("$([") {
                return Err(node_error(
                    path,
                    document,
                    node,
                    "MSBUILD_PROPERTY_FUNCTION_REJECTED",
                    "MSBuild property functions are not evaluated",
                ));
            }
            if value.contains("@(") || value.contains("%(") {
                return Err(node_error(
                    path,
                    document,
                    node,
                    "MSBUILD_ITEM_EXPRESSION_REJECTED",
                    "MSBuild item lists and transforms are not evaluated",
                ));
            }
            let remainder = [
                "$(AVRSTUDIO_EXE_PATH)",
                "$(Configuration)",
                "$(MSBuildProjectDirectory)",
                "$(MSBuildProjectName)",
                "$(ProjectDir)",
                "%24(PackRepoDir)",
                "%24(ProjectDir)",
            ]
            .into_iter()
            .fold(value.to_owned(), |text, allowed| text.replace(allowed, ""));
            if remainder.contains("$(") || remainder.contains("%24(") {
                return Err(node_error(
                    path,
                    document,
                    node,
                    "UNKNOWN_MSBUILD_EXPRESSION",
                    format!("unsupported MSBuild expression in {value}"),
                ));
            }
        }
    }
    Ok(())
}

#[allow(clippy::too_many_lines)]
fn parse_items(
    path: &Path,
    document: &Document<'_>,
    root: Node<'_, '_>,
    project_root: &Path,
    canonical_root: &Path,
    project_name: &str,
    configuration: &str,
) -> SamdebugResult<(Vec<SourceInput>, Vec<String>)> {
    let mut sources = Vec::new();
    let mut headers = Vec::new();
    let mut seen = BTreeSet::new();
    for node in root
        .descendants()
        .filter(|node| node.has_tag_name("Compile"))
    {
        let raw = node.attribute("Include").ok_or_else(|| {
            node_error(
                path,
                document,
                node,
                "INPUT_PATH_MISSING",
                "Compile item has no Include",
            )
        })?;
        let expanded = expand_value(raw, project_name, configuration)
            .map_err(|error| expression_error(path, document, node, &error))?;
        let normalized = normalize_separators(&expanded);
        let link_node = node.children().find(|child| child.has_tag_name("Link"));
        let link_name = link_node
            .and_then(|child| child.text())
            .map(|value| {
                expand_value(value, project_name, configuration)
                    .map(|expanded| normalize_separators(&expanded))
                    .map_err(|error| expression_error(path, document, link_node.unwrap(), &error))
            })
            .transpose()?;
        let relative = clean_relative(&normalized, link_name.is_some())
            .map_err(|message| node_error(path, document, node, "UNSAFE_PROJECT_PATH", message))?;
        let candidate = project_root.join(&relative);
        let metadata = fs::symlink_metadata(&candidate).map_err(|_| {
            node_error(
                path,
                document,
                node,
                "GENERATED_INPUT_UNAVAILABLE",
                format!("input does not exist at import time: {normalized}"),
            )
        })?;
        if metadata.file_type().is_symlink() || !metadata.is_file() {
            return Err(node_error(
                path,
                document,
                node,
                "UNSAFE_PROJECT_PATH",
                "input must be a regular non-symlink file",
            ));
        }
        let canonical = candidate
            .canonicalize()
            .map_err(|error| project_io("INPUT_CANONICALIZE_FAILED", &error))?;
        let external = !canonical.starts_with(canonical_root);
        if external && link_name.is_none() {
            return Err(node_error(
                path,
                document,
                node,
                "PROJECT_PATH_ESCAPE",
                "an external input requires explicit Link metadata",
            ));
        }
        let key = canonical.to_string_lossy().into_owned();
        if !seen.insert(key) {
            return Err(node_error(
                path,
                document,
                node,
                "CONFLICTING_DUPLICATE_INPUT",
                format!("input is listed more than once: {normalized}"),
            ));
        }
        match candidate
            .extension()
            .and_then(|extension| extension.to_str())
        {
            Some("c") => sources.push(SourceInput {
                path: normalized,
                kind: SourceKind::C,
                link_name,
                external,
            }),
            Some("s" | "asm") => sources.push(SourceInput {
                path: normalized,
                kind: SourceKind::Assembly,
                link_name,
                external,
            }),
            Some("S") => sources.push(SourceInput {
                path: normalized,
                kind: SourceKind::PreprocessedAssembly,
                link_name,
                external,
            }),
            Some("h" | "hpp") => headers.push(normalized),
            Some("cc" | "cpp" | "cxx") => {
                return Err(node_error(
                    path,
                    document,
                    node,
                    "CPP_INPUT_REJECTED",
                    "C++ inputs are not supported",
                ));
            }
            _ => {
                return Err(node_error(
                    path,
                    document,
                    node,
                    "UNSUPPORTED_INPUT",
                    "Compile items must be C, assembly, or header files",
                ));
            }
        }
    }
    if sources.is_empty() {
        return Err(node_error(
            path,
            document,
            root,
            "NO_SOURCE_INPUTS",
            "project contains no compilable inputs",
        ));
    }
    Ok((sources, headers))
}

fn parse_include_paths(
    path: &Path,
    document: &Document<'_>,
    settings: Node<'_, '_>,
    tag: &str,
    project_root: &Path,
    warnings: &mut Vec<ImportWarning>,
) -> SamdebugResult<Vec<String>> {
    let nodes = values_nodes(settings, tag);
    let mut local = Vec::new();
    let mut vendor = Vec::new();
    for node in nodes {
        let value = node.text().unwrap_or_default().trim();
        if value.contains("%24(PackRepoDir)") || value.contains("$(PackRepoDir)") {
            vendor.push(node);
        } else {
            let normalized = normalize_setting_path(value, "", "")
                .map_err(|error| expression_error(path, document, node, &error))?;
            if !project_root.join(&normalized).is_dir() {
                return Err(node_error(
                    path,
                    document,
                    node,
                    "INCLUDE_DIRECTORY_MISSING",
                    format!("include directory does not exist: {normalized}"),
                ));
            }
            push_unique(&mut local, normalized);
        }
    }
    for node in vendor {
        let value = node.text().unwrap_or_default();
        let replacement = if value.contains("CMSIS") && value.contains("Core") {
            local.iter().any(|item| {
                item.to_ascii_lowercase()
                    .contains("thirdparty/cmsis/include")
            })
        } else if value.contains("SAM4S_DFP") {
            local
                .iter()
                .any(|item| item.to_ascii_lowercase().contains("cmsis/sam4s/include"))
        } else {
            false
        };
        if !replacement {
            return Err(node_error(
                path,
                document,
                node,
                "MISSING_VENDOR_PACK",
                "copy the required CMSIS/SAM4S pack files into the project and add a project-relative include directory",
            ));
        }
        warnings.push(ImportWarning {
            code: "VENDOR_PACK_PATH_OMITTED".into(),
            message: format!("omitted Microchip Studio pack path: {value}"),
            location: location(path, document, node),
        });
    }
    Ok(local)
}

fn normalize_paths(
    path: &Path,
    document: &Document<'_>,
    nodes: Vec<Node<'_, '_>>,
    project_name: &str,
    configuration: &str,
) -> SamdebugResult<Vec<String>> {
    let mut result = Vec::new();
    for node in nodes {
        let value = expand_value(node.text().unwrap_or_default(), project_name, configuration)
            .map_err(|error| node_error(path, document, node, error.code(), error.to_string()))?;
        push_unique(
            &mut result,
            normalize_setting_path(&value, project_name, configuration)?,
        );
    }
    Ok(result)
}

fn normalize_linker_script(
    path: &Path,
    document: &Document<'_>,
    source_node: Option<Node<'_, '_>>,
    flags: &mut Vec<String>,
) -> SamdebugResult<Option<String>> {
    let mut script = None;
    let mut index = 0;
    while index < flags.len() {
        let candidate = if flags[index] == "-T" {
            flags.get(index + 1).cloned()
        } else {
            flags[index].strip_prefix("-T").map(str::to_owned)
        };
        if let Some(value) = candidate {
            if script.is_some() {
                return Err(node_error(
                    path,
                    document,
                    source_node.unwrap_or(document.root_element()),
                    "CONFLICTING_LINKER_SCRIPT",
                    "multiple linker scripts are unsupported",
                ));
            }
            script = Some(normalize_setting_path(&value, "", "").map_err(|error| {
                expression_error(
                    path,
                    document,
                    source_node.unwrap_or(document.root_element()),
                    &error,
                )
            })?);
            if flags[index] == "-T" {
                flags.drain(index..=index + 1);
            } else {
                flags.remove(index);
            }
        } else {
            index += 1;
        }
    }
    if let Some(script) = &script {
        let root = path.parent().unwrap_or_else(|| Path::new("."));
        if !root.join(script).is_file() {
            return Err(node_error(
                path,
                document,
                source_node.unwrap_or(document.root_element()),
                "LINKER_SCRIPT_MISSING",
                format!("linker script does not exist: {script}"),
            ));
        }
    }
    Ok(script)
}

fn tokenize_node(
    path: &Path,
    document: &Document<'_>,
    node: Option<Node<'_, '_>>,
    project_name: &str,
    configuration: &str,
) -> SamdebugResult<Vec<String>> {
    let Some(node) = node else {
        return Ok(Vec::new());
    };
    let expanded = expand_value(node.text().unwrap_or_default(), project_name, configuration)
        .map_err(|error| expression_error(path, document, node, &error))?;
    tokenize(&expanded)
        .map_err(|message| node_error(path, document, node, "INVALID_ARGUMENT_LIST", message))
}

fn tokenize(value: &str) -> Result<Vec<String>, String> {
    let mut result = Vec::new();
    let mut current = String::new();
    let mut quote = None;
    let mut escaped = false;
    for character in value.chars() {
        if escaped {
            current.push(character);
            escaped = false;
        } else if character == '\\' && quote == Some('"') {
            escaped = true;
        } else if matches!(character, '\'' | '"') {
            if quote == Some(character) {
                quote = None;
            } else if quote.is_none() {
                quote = Some(character);
            } else {
                current.push(character);
            }
        } else if character.is_whitespace() && quote.is_none() {
            if !current.is_empty() {
                result.push(std::mem::take(&mut current));
            }
        } else {
            current.push(character);
        }
    }
    if quote.is_some() || escaped {
        return Err("unterminated quote or escape in option list".into());
    }
    if !current.is_empty() {
        result.push(current);
    }
    Ok(result)
}

fn append_label_flag(
    settings: Node<'_, '_>,
    tag: &str,
    choices: &[&str],
    output: &mut Vec<String>,
) {
    let Some(text) = descendant(settings, tag).and_then(|node| node.text()) else {
        return;
    };
    if let Some(choice) = choices
        .iter()
        .filter(|choice| text.contains(**choice))
        .max_by_key(|choice| choice.len())
    {
        push_unique(output, (*choice).into());
    }
}

fn checked_values(
    path: &Path,
    document: &Document<'_>,
    settings: Node<'_, '_>,
    tag: &str,
    project_name: &str,
    configuration: &str,
) -> SamdebugResult<Vec<String>> {
    values_nodes(settings, tag)
        .into_iter()
        .filter_map(|node| {
            let value = node.text()?.trim();
            (!value.is_empty()).then_some((node, value))
        })
        .map(|(node, value)| {
            expand_value(value, project_name, configuration)
                .map_err(|error| expression_error(path, document, node, &error))
        })
        .collect()
}

fn validate_conflicting_scalars(
    path: &Path,
    document: &Document<'_>,
    scope: Node<'_, '_>,
    tags: &[&str],
) -> SamdebugResult<()> {
    for tag in tags {
        let nodes: Vec<_> = scope
            .descendants()
            .filter(|node| node.has_tag_name(*tag))
            .collect();
        let distinct: BTreeSet<_> = nodes
            .iter()
            .map(|node| node.text().unwrap_or_default().trim())
            .collect();
        if distinct.len() > 1 {
            return Err(node_error(
                path,
                document,
                nodes[1],
                "CONFLICTING_SCALAR_SETTING",
                format!("conflicting values for {tag}"),
            ));
        }
    }
    Ok(())
}

fn values_nodes<'a>(settings: Node<'a, 'a>, tag: &str) -> Vec<Node<'a, 'a>> {
    descendant(settings, tag)
        .into_iter()
        .flat_map(|container| {
            container
                .descendants()
                .filter(|node| node.has_tag_name("Value"))
        })
        .collect()
}

fn descendant<'a>(node: Node<'a, 'a>, tag: &str) -> Option<Node<'a, 'a>> {
    node.descendants()
        .find(|candidate| candidate.has_tag_name(tag))
}

fn child_text<'a>(node: Node<'a, 'a>, tag: &str) -> Option<&'a str> {
    node.children()
        .find(|candidate| candidate.has_tag_name(tag))
        .and_then(|candidate| candidate.text())
        .map(str::trim)
}

fn bool_setting(settings: Node<'_, '_>, tag: &str) -> bool {
    descendant(settings, tag)
        .and_then(|node| node.text())
        .is_some_and(|text| text.trim().eq_ignore_ascii_case("true"))
}

fn parse_configuration_condition(condition: &str) -> Option<&str> {
    let compact: String = condition
        .chars()
        .filter(|character| !character.is_whitespace())
        .collect();
    match compact.as_str() {
        "'$(Configuration)'=='Debug'" | "\"$(Configuration)\"==\"Debug\"" => Some("Debug"),
        "'$(Configuration)'=='Release'" | "\"$(Configuration)\"==\"Release\"" => Some("Release"),
        _ => None,
    }
}

fn expand_value(value: &str, project_name: &str, configuration: &str) -> SamdebugResult<String> {
    let mut expanded = value
        .replace("$(MSBuildProjectName)", project_name)
        .replace("$(Configuration)", configuration)
        .replace("%24(ProjectDir)\\", "")
        .replace("%24(ProjectDir)/", "")
        .replace("$(ProjectDir)\\", "")
        .replace("$(ProjectDir)/", "")
        .replace("%24(ProjectDir)", "")
        .replace("$(ProjectDir)", "");
    if expanded.contains("$(") || expanded.contains("%24(") || expanded.contains("$([") {
        return Err(SamdebugError::new(
            ErrorCategory::Project,
            "UNKNOWN_MSBUILD_EXPRESSION",
            format!("unsupported MSBuild expression in {value}"),
        ));
    }
    expanded = normalize_separators(&expanded);
    Ok(collapse_slashes(&expanded))
}

fn normalize_setting_path(
    value: &str,
    project_name: &str,
    configuration: &str,
) -> SamdebugResult<String> {
    let expanded = expand_value(value, project_name, configuration)?;
    let relative = expanded.strip_prefix("../").unwrap_or(&expanded);
    clean_relative(relative, false).map_err(|message| {
        SamdebugError::new(ErrorCategory::Project, "UNSAFE_PROJECT_PATH", message)
    })
}

fn clean_relative(value: &str, allow_parent: bool) -> Result<String, String> {
    let path = Path::new(value);
    if path.is_absolute() || is_windows_absolute(value) {
        return Err(format!("absolute project path is unsupported: {value}"));
    }
    let mut parts = Vec::new();
    for component in path.components() {
        match component {
            Component::Normal(part) => parts.push(part.to_string_lossy().into_owned()),
            Component::CurDir => {}
            Component::ParentDir if allow_parent => parts.push("..".into()),
            Component::ParentDir => return Err(format!("project path escapes its root: {value}")),
            _ => return Err(format!("invalid project path: {value}")),
        }
    }
    if parts.is_empty() {
        return Err("project path is empty".into());
    }
    Ok(parts.join("/"))
}

fn is_windows_absolute(value: &str) -> bool {
    value.as_bytes().get(1) == Some(&b':') || value.starts_with("//")
}

fn normalize_separators(value: &str) -> String {
    value.replace('\\', "/")
}

fn collapse_slashes(value: &str) -> String {
    let mut result = String::with_capacity(value.len());
    let mut slash = false;
    for character in value.chars() {
        if character == '/' {
            if !slash {
                result.push(character);
            }
            slash = true;
        } else {
            result.push(character);
            slash = false;
        }
    }
    result
}

fn append_unique(output: &mut Vec<String>, values: Vec<String>) {
    for value in values {
        push_unique(output, value);
    }
}

fn push_unique(output: &mut Vec<String>, value: String) {
    if !output.contains(&value) {
        output.push(value);
    }
}

fn write_atomic_new(path: &Path, bytes: &[u8]) -> SamdebugResult<()> {
    let parent = path.parent().unwrap_or_else(|| Path::new("."));
    let temporary = parent.join(format!(
        ".{}.samdebug-new-{}",
        path.file_name()
            .and_then(|name| name.to_str())
            .unwrap_or("file"),
        std::process::id()
    ));
    let mut file = OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(&temporary)
        .map_err(|error| project_io("OUTPUT_CREATE_FAILED", &error))?;
    let result = (|| {
        file.write_all(bytes)
            .map_err(|error| project_io("OUTPUT_WRITE_FAILED", &error))?;
        file.sync_all()
            .map_err(|error| project_io("OUTPUT_SYNC_FAILED", &error))?;
        fs::hard_link(&temporary, path)
            .map_err(|error| project_io("OUTPUT_PROMOTION_FAILED", &error))
    })();
    let _ = fs::remove_file(temporary);
    result
}

fn node_error(
    path: &Path,
    document: &Document<'_>,
    node: Node<'_, '_>,
    code: impl Into<String>,
    message: impl Into<String>,
) -> SamdebugError {
    let location = location(path, document, node);
    SamdebugError::new(ErrorCategory::Project, code, message)
        .with_details(serde_json::to_value(location).expect("XML location serializes"))
}

fn expression_error(
    path: &Path,
    document: &Document<'_>,
    node: Node<'_, '_>,
    error: &SamdebugError,
) -> SamdebugError {
    node_error(path, document, node, error.code(), error.to_string())
}

fn location(path: &Path, document: &Document<'_>, node: Node<'_, '_>) -> XmlLocation {
    let position = document.text_pos_at(node.range().start);
    XmlLocation {
        file: path.display().to_string(),
        line: position.row,
        column: position.col,
        element: node.tag_name().name().into(),
    }
}

fn project_io(code: &str, error: &std::io::Error) -> SamdebugError {
    SamdebugError::new(ErrorCategory::Project, code, error.to_string())
}
