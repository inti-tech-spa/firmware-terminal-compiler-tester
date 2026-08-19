use std::{
    collections::BTreeSet,
    fs::{self, OpenOptions},
    io::Write,
    path::{Path, PathBuf},
    sync::atomic::{AtomicU64, Ordering},
};

use samdebug_core::{
    CancellationToken, Configuration, ErrorCategory, SamdebugError, SamdebugResult,
    ports::{CommandOutput, CommandSpec, ProcessRunner},
};

use crate::{
    ArtifactInfo, ArtifactsReport, BuildReport, BuildToolPaths, CleanReport, MemoryUsage,
    SourceKind, import_cproj,
};

const FLASH_LIMIT: u64 = 2 * 1024 * 1024;
const RAM_LIMIT: u64 = 160 * 1024;
const FLASH_START: u64 = 0x0040_0000;
const FLASH_END: u64 = FLASH_START + FLASH_LIMIT;
const TARGET_FLAGS: [&str; 3] = ["-mcpu=cortex-m4", "-mthumb", "-mfloat-abi=soft"];
static OUTPUT_NONCE: AtomicU64 = AtomicU64::new(0);

#[allow(clippy::too_many_lines)]
pub fn build(
    project_file: &Path,
    configuration: Configuration,
    tools: &BuildToolPaths,
    runner: &dyn ProcessRunner,
    cancellation: &CancellationToken,
) -> SamdebugResult<BuildReport> {
    check_cancelled(cancellation)?;
    validate_tools(tools)?;
    let imported = import_cproj(project_file, configuration)?;
    let plan = &imported.plan;
    let root = project_file.parent().unwrap_or_else(|| Path::new("."));
    let state = ensure_state(root)?;
    let build_dir = state.join("build").join(&plan.configuration);
    ensure_output_directory(&state, &build_dir)?;
    write_plan(&state.join("import-plan.json"), plan)?;

    let object_dir = build_dir.join("obj");
    ensure_output_directory(&state, &object_dir)?;
    let mut objects = Vec::new();
    let mut compiled = 0;
    let mut reused = 0;
    for (index, source) in plan.sources.iter().enumerate() {
        check_cancelled(cancellation)?;
        let basename = Path::new(&source.path)
            .file_name()
            .and_then(|name| name.to_str())
            .unwrap_or("source");
        let object = object_dir.join(format!("{index:04}-{basename}.o"));
        let dependency = object.with_extension("d");
        let stamp = object.with_extension("command.json");
        for output in [&object, &dependency, &stamp] {
            ensure_file_target(&state, output)?;
        }
        let mut args = TARGET_FLAGS
            .iter()
            .map(ToString::to_string)
            .collect::<Vec<_>>();
        args.extend([
            "-mlong-calls".into(),
            "-D__SAM4SD32C__".into(),
            "-Wno-error=incompatible-pointer-types".into(),
        ]);
        args.extend(plan.compiler_flags.iter().cloned());
        if source.kind != SourceKind::C {
            args.extend(plan.assembler_flags.iter().cloned());
            args.push(
                match source.kind {
                    SourceKind::Assembly => "assembler",
                    SourceKind::PreprocessedAssembly => "assembler-with-cpp",
                    SourceKind::C => unreachable!(),
                }
                .into(),
            );
            let length = args.len();
            args.insert(length - 1, "-x".into());
        }
        for symbol in &plan.symbols {
            args.push(format!("-D{symbol}"));
        }
        for include in plan
            .include_directories
            .iter()
            .chain(&plan.assembler_include_directories)
        {
            args.push("-I".into());
            args.push(include.clone());
        }
        let inferred_includes: BTreeSet<_> = plan
            .sources
            .iter()
            .map(|input| &input.path)
            .chain(&plan.headers)
            .filter_map(|input| Path::new(input).parent())
            .filter(|parent| !parent.as_os_str().is_empty())
            .map(|parent| parent.to_string_lossy().into_owned())
            .collect();
        for include in inferred_includes {
            args.push("-I".into());
            args.push(include);
        }
        args.extend([
            "-MMD".into(),
            "-MP".into(),
            "-MF".into(),
            dependency.to_string_lossy().into_owned(),
            "-c".into(),
            source.path.clone(),
            "-o".into(),
            object.to_string_lossy().into_owned(),
        ]);
        let command_bytes = serde_json::to_vec(&args).expect("argv serializes");
        if is_current(
            root,
            &source.path,
            &object,
            &dependency,
            &stamp,
            &command_bytes,
        )? {
            reused += 1;
        } else {
            run_checked(runner, &tools.gcc, args, root, "COMPILE_FAILED")?;
            write_replace(&stamp, &command_bytes)?;
            compiled += 1;
        }
        objects.push(object);
    }

    check_cancelled(cancellation)?;
    let elf = build_dir.join(format!("{}{}", plan.output_name, plan.output_extension));
    let map = build_dir.join(format!("{}.map", plan.output_name));
    ensure_file_target(&state, &elf)?;
    ensure_file_target(&state, &map)?;
    let mut link_args = TARGET_FLAGS
        .iter()
        .map(ToString::to_string)
        .collect::<Vec<_>>();
    link_args.extend(
        objects
            .iter()
            .map(|path| path.to_string_lossy().into_owned()),
    );
    link_args.extend(plan.linker_flags.iter().cloned());
    if let Some(script) = &plan.linker_script {
        link_args.push(format!("-T{script}"));
    }
    link_args.push(format!("-Wl,-Map,{}", map.to_string_lossy()));
    for directory in &plan.library_search_paths {
        link_args.push(format!("-L{directory}"));
    }
    link_args.push("-Wl,--start-group".into());
    for library in &plan.libraries {
        link_args.push(format!(
            "-l{}",
            library.strip_prefix("lib").unwrap_or(library)
        ));
    }
    link_args.push("-Wl,--end-group".into());
    link_args.extend(["-o".into(), elf.to_string_lossy().into_owned()]);
    run_checked(runner, &tools.gcc, link_args, root, "LINK_FAILED")?;

    let size_output = run_checked(
        runner,
        &tools.size,
        vec![elf.to_string_lossy().into_owned()],
        root,
        "SIZE_INSPECTION_FAILED",
    )?;
    let memory = parse_size(&size_output.stdout)?;
    if memory.flash_bytes > memory.flash_limit || memory.ram_bytes > memory.ram_limit {
        return Err(SamdebugError::new(
            ErrorCategory::Project,
            "MEMORY_OVERFLOW",
            format!(
                "flash {}/{} bytes, RAM {}/{} bytes",
                memory.flash_bytes, memory.flash_limit, memory.ram_bytes, memory.ram_limit
            ),
        ));
    }
    let header = run_checked(
        runner,
        &tools.objdump,
        vec!["-f".into(), elf.to_string_lossy().into_owned()],
        root,
        "ELF_INSPECTION_FAILED",
    )?;
    let entry_point = parse_entry(&header.stdout)?;
    if !(FLASH_START..FLASH_END).contains(&entry_point) {
        return Err(SamdebugError::new(
            ErrorCategory::Project,
            "INVALID_ENTRY_POINT",
            format!("ELF entry point 0x{entry_point:08x} is outside ATSAM4SD32C flash"),
        ));
    }

    let size_path = build_dir.join(format!("{}.size", plan.output_name));
    ensure_file_target(&state, &size_path)?;
    write_replace(&size_path, &size_output.stdout)?;
    let mut generated = vec![elf.clone(), map, size_path];
    generate_artifacts(
        root,
        &state,
        &build_dir,
        plan,
        tools,
        runner,
        &elf,
        &mut generated,
    )?;
    Ok(BuildReport {
        configuration: plan.configuration.clone(),
        compiled,
        reused,
        elf: elf.display().to_string(),
        artifacts: generated
            .iter()
            .map(|path| path.display().to_string())
            .collect(),
        entry_point,
        memory,
        warnings: imported.warnings,
    })
}

pub fn clean(project_root: &Path, configuration: Configuration) -> SamdebugResult<CleanReport> {
    let state = ensure_state(project_root)?;
    let name = configuration_name(configuration);
    let path = state.join("build").join(name);
    validate_removal_target(&state, &path)?;
    let removed = path.exists();
    if removed {
        fs::remove_dir_all(&path).map_err(|error| build_io("CLEAN_FAILED", &error))?;
    }
    Ok(CleanReport {
        removed,
        path: path.display().to_string(),
    })
}

pub fn artifacts(
    project_root: &Path,
    configuration: Configuration,
) -> SamdebugResult<ArtifactsReport> {
    let state = ensure_state(project_root)?;
    let name = configuration_name(configuration);
    let path = state.join("build").join(name);
    validate_removal_target(&state, &path)?;
    let mut found = Vec::new();
    if path.is_dir() {
        for entry in
            fs::read_dir(&path).map_err(|error| build_io("ARTIFACT_READ_FAILED", &error))?
        {
            let entry = entry.map_err(|error| build_io("ARTIFACT_READ_FAILED", &error))?;
            let metadata = entry
                .metadata()
                .map_err(|error| build_io("ARTIFACT_READ_FAILED", &error))?;
            if metadata.is_file() {
                found.push(ArtifactInfo {
                    path: entry.path().display().to_string(),
                    bytes: metadata.len(),
                });
            }
        }
    }
    found.sort_by(|left, right| left.path.cmp(&right.path));
    Ok(ArtifactsReport {
        configuration: name.into(),
        artifacts: found,
    })
}

#[allow(clippy::too_many_arguments)]
fn generate_artifacts(
    root: &Path,
    state: &Path,
    build_dir: &Path,
    plan: &crate::BuildPlan,
    tools: &BuildToolPaths,
    runner: &dyn ProcessRunner,
    elf: &Path,
    generated: &mut Vec<PathBuf>,
) -> SamdebugResult<()> {
    for (enabled, extension, format) in [
        (plan.artifacts.bin, "bin", "binary"),
        (plan.artifacts.hex, "hex", "ihex"),
        (plan.artifacts.srec, "srec", "srec"),
    ] {
        if enabled {
            let output = build_dir.join(format!("{}.{}", plan.output_name, extension));
            ensure_file_target(state, &output)?;
            run_checked(
                runner,
                &tools.objcopy,
                vec![
                    "-O".into(),
                    format.into(),
                    elf.to_string_lossy().into_owned(),
                    output.to_string_lossy().into_owned(),
                ],
                root,
                "ARTIFACT_GENERATION_FAILED",
            )?;
            generated.push(output);
        }
    }
    if plan.artifacts.disassembly {
        let output = build_dir.join(format!("{}.lss", plan.output_name));
        ensure_file_target(state, &output)?;
        let result = run_checked(
            runner,
            &tools.objdump,
            vec!["-h".into(), "-S".into(), elf.to_string_lossy().into_owned()],
            root,
            "DISASSEMBLY_FAILED",
        )?;
        write_replace(&output, &result.stdout)?;
        generated.push(output);
    }
    if plan.artifacts.eeprom {
        let output = build_dir.join(format!("{}.eep", plan.output_name));
        ensure_file_target(state, &output)?;
        run_checked(
            runner,
            &tools.objcopy,
            vec![
                "-j".into(),
                ".eeprom".into(),
                "--set-section-flags=.eeprom=alloc,load".into(),
                "--change-section-lma".into(),
                ".eeprom=0".into(),
                "--no-change-warnings".into(),
                "-O".into(),
                "ihex".into(),
                elf.to_string_lossy().into_owned(),
                output.to_string_lossy().into_owned(),
            ],
            root,
            "ARTIFACT_GENERATION_FAILED",
        )?;
        generated.push(output);
    }
    Ok(())
}

fn run_checked(
    runner: &dyn ProcessRunner,
    program: &str,
    args: Vec<String>,
    current_dir: &Path,
    code: &str,
) -> SamdebugResult<CommandOutput> {
    let output = runner.run(&CommandSpec {
        program: program.into(),
        args,
        current_dir: Some(current_dir.to_owned()),
    })?;
    if output.exit_code == Some(0) {
        Ok(output)
    } else {
        let stderr = String::from_utf8_lossy(&output.stderr);
        Err(SamdebugError::new(
            ErrorCategory::Project,
            code,
            stderr.trim(),
        ))
    }
}

fn validate_tools(tools: &BuildToolPaths) -> SamdebugResult<()> {
    for path in [&tools.gcc, &tools.objcopy, &tools.objdump, &tools.size] {
        let path = Path::new(path);
        if !path.is_absolute() || !path.is_file() {
            return Err(SamdebugError::new(
                ErrorCategory::Tool,
                "BUILD_TOOL_MISSING",
                format!("build tool is unavailable: {}", path.display()),
            ));
        }
    }
    Ok(())
}

fn ensure_state(root: &Path) -> SamdebugResult<PathBuf> {
    let canonical_root = root
        .canonicalize()
        .map_err(|error| build_io("PROJECT_ROOT_INVALID", &error))?;
    let state = root.join(".samdebug");
    if let Ok(metadata) = fs::symlink_metadata(&state)
        && (metadata.file_type().is_symlink() || !metadata.is_dir())
    {
        return Err(SamdebugError::new(
            ErrorCategory::Project,
            "UNSAFE_STATE_DIRECTORY",
            ".samdebug must be a real project-local directory",
        ));
    }
    fs::create_dir_all(&state).map_err(|error| build_io("STATE_DIRECTORY_FAILED", &error))?;
    let canonical = state
        .canonicalize()
        .map_err(|error| build_io("STATE_DIRECTORY_INVALID", &error))?;
    if !canonical.starts_with(canonical_root) {
        return Err(SamdebugError::new(
            ErrorCategory::Project,
            "STATE_DIRECTORY_ESCAPE",
            ".samdebug resolves outside the project",
        ));
    }
    Ok(canonical)
}

fn ensure_output_directory(state: &Path, path: &Path) -> SamdebugResult<()> {
    let mut ancestor = path;
    while !ancestor.exists() {
        ancestor = ancestor.parent().ok_or_else(|| {
            SamdebugError::new(
                ErrorCategory::Project,
                "BUILD_DIRECTORY_ESCAPE",
                "build directory has no existing ancestor",
            )
        })?;
    }
    let metadata = fs::symlink_metadata(ancestor)
        .map_err(|error| build_io("BUILD_DIRECTORY_INVALID", &error))?;
    if metadata.file_type().is_symlink() {
        return Err(SamdebugError::new(
            ErrorCategory::Project,
            "UNSAFE_BUILD_DIRECTORY",
            "build directory ancestor may not be a symlink",
        ));
    }
    let canonical_ancestor = ancestor
        .canonicalize()
        .map_err(|error| build_io("BUILD_DIRECTORY_INVALID", &error))?;
    if !canonical_ancestor.starts_with(state) {
        return Err(SamdebugError::new(
            ErrorCategory::Project,
            "BUILD_DIRECTORY_ESCAPE",
            "build directory resolves outside .samdebug",
        ));
    }
    fs::create_dir_all(path).map_err(|error| build_io("BUILD_DIRECTORY_FAILED", &error))?;
    let canonical = path
        .canonicalize()
        .map_err(|error| build_io("BUILD_DIRECTORY_INVALID", &error))?;
    if !canonical.starts_with(state) {
        return Err(SamdebugError::new(
            ErrorCategory::Project,
            "BUILD_DIRECTORY_ESCAPE",
            "build directory resolves outside .samdebug",
        ));
    }
    Ok(())
}

fn ensure_file_target(state: &Path, path: &Path) -> SamdebugResult<()> {
    let parent = path.parent().ok_or_else(|| {
        SamdebugError::new(
            ErrorCategory::Project,
            "UNSAFE_OUTPUT_PATH",
            "output has no parent",
        )
    })?;
    let canonical_parent = parent
        .canonicalize()
        .map_err(|error| build_io("OUTPUT_DIRECTORY_INVALID", &error))?;
    if !canonical_parent.starts_with(state) {
        return Err(SamdebugError::new(
            ErrorCategory::Project,
            "OUTPUT_PATH_ESCAPE",
            format!("output resolves outside .samdebug: {}", path.display()),
        ));
    }
    if let Ok(metadata) = fs::symlink_metadata(path)
        && metadata.file_type().is_symlink()
    {
        return Err(SamdebugError::new(
            ErrorCategory::Project,
            "UNSAFE_OUTPUT_PATH",
            format!("output is a symlink: {}", path.display()),
        ));
    }
    Ok(())
}

fn validate_removal_target(state: &Path, path: &Path) -> SamdebugResult<()> {
    if let Ok(metadata) = fs::symlink_metadata(path)
        && metadata.file_type().is_symlink()
    {
        return Err(SamdebugError::new(
            ErrorCategory::Project,
            "UNSAFE_BUILD_DIRECTORY",
            "build directory may not be a symlink",
        ));
    }
    let parent = path.parent().ok_or_else(|| {
        SamdebugError::new(
            ErrorCategory::Project,
            "UNSAFE_BUILD_DIRECTORY",
            "build directory has no parent",
        )
    })?;
    let canonical_parent = if parent.exists() {
        parent
            .canonicalize()
            .map_err(|error| build_io("BUILD_DIRECTORY_INVALID", &error))?
    } else {
        state.to_owned()
    };
    if !canonical_parent.starts_with(state) || path.file_name().is_none() {
        return Err(SamdebugError::new(
            ErrorCategory::Project,
            "BUILD_DIRECTORY_ESCAPE",
            "build directory resolves outside .samdebug",
        ));
    }
    Ok(())
}

fn write_plan(path: &Path, plan: &crate::BuildPlan) -> SamdebugResult<()> {
    let bytes = serde_json::to_vec_pretty(plan).expect("plan serializes");
    write_replace(path, &bytes)
}

fn write_replace(path: &Path, bytes: &[u8]) -> SamdebugResult<()> {
    if let Ok(metadata) = fs::symlink_metadata(path)
        && metadata.file_type().is_symlink()
    {
        return Err(SamdebugError::new(
            ErrorCategory::Project,
            "UNSAFE_OUTPUT_PATH",
            format!("output is a symlink: {}", path.display()),
        ));
    }
    let temporary = path.with_extension(format!(
        "samdebug-tmp-{}-{}",
        std::process::id(),
        OUTPUT_NONCE.fetch_add(1, Ordering::Relaxed)
    ));
    let mut file = OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(&temporary)
        .map_err(|error| build_io("OUTPUT_CREATE_FAILED", &error))?;
    if let Err(error) = file.write_all(bytes).and_then(|()| file.sync_all()) {
        let _ = fs::remove_file(&temporary);
        return Err(build_io("OUTPUT_WRITE_FAILED", &error));
    }
    let result =
        fs::rename(&temporary, path).map_err(|error| build_io("OUTPUT_PROMOTION_FAILED", &error));
    if result.is_err() {
        let _ = fs::remove_file(temporary);
    }
    result
}

fn is_current(
    root: &Path,
    source: &str,
    object: &Path,
    dependency: &Path,
    stamp: &Path,
    command: &[u8],
) -> SamdebugResult<bool> {
    if !object.is_file()
        || !dependency.is_file()
        || fs::read(stamp).ok().as_deref() != Some(command)
    {
        return Ok(false);
    }
    let object_time = object
        .metadata()
        .and_then(|metadata| metadata.modified())
        .map_err(|error| build_io("INCREMENTAL_CHECK_FAILED", &error))?;
    let mut inputs = BTreeSet::from([root.join(source)]);
    let dependency_text = fs::read_to_string(dependency)
        .map_err(|error| build_io("DEPENDENCY_READ_FAILED", &error))?;
    let logical = dependency_text.replace("\\\n", " ");
    if let Some((_, values)) = logical
        .split("\n\n")
        .next()
        .unwrap_or_default()
        .split_once(':')
    {
        for value in parse_make_words(values)
            .into_iter()
            .take_while(|value| !value.ends_with(':'))
        {
            inputs.insert(root.join(value));
        }
    }
    for input in inputs {
        let modified = input
            .metadata()
            .and_then(|metadata| metadata.modified())
            .map_err(|error| {
                SamdebugError::new(
                    ErrorCategory::Project,
                    "DEPENDENCY_MISSING",
                    format!("{}: {error}", input.display()),
                )
            })?;
        if modified > object_time {
            return Ok(false);
        }
    }
    Ok(true)
}

fn parse_make_words(value: &str) -> Vec<String> {
    let mut words = Vec::new();
    let mut word = String::new();
    let mut escaped = false;
    for character in value.chars() {
        if escaped {
            word.push(character);
            escaped = false;
        } else if character == '\\' {
            escaped = true;
        } else if character.is_whitespace() {
            if !word.is_empty() {
                words.push(std::mem::take(&mut word));
            }
        } else {
            word.push(character);
        }
    }
    if !word.is_empty() {
        words.push(word);
    }
    words
}

fn parse_size(bytes: &[u8]) -> SamdebugResult<MemoryUsage> {
    let text = String::from_utf8_lossy(bytes);
    let line = text
        .lines()
        .rev()
        .find(|line| {
            let values: Vec<_> = line.split_whitespace().collect();
            values.len() >= 4
                && values[0].parse::<u64>().is_ok()
                && values[1].parse::<u64>().is_ok()
                && values[2].parse::<u64>().is_ok()
        })
        .ok_or_else(|| {
            SamdebugError::new(
                ErrorCategory::Project,
                "SIZE_OUTPUT_INVALID",
                "could not parse GNU size output",
            )
        })?;
    let values: Vec<_> = line.split_whitespace().collect();
    let text_bytes = values[0].parse::<u64>().expect("validated");
    let data = values[1].parse::<u64>().expect("validated");
    let bss = values[2].parse::<u64>().expect("validated");
    Ok(MemoryUsage {
        flash_bytes: text_bytes + data,
        flash_limit: FLASH_LIMIT,
        ram_bytes: data + bss,
        ram_limit: RAM_LIMIT,
    })
}

fn parse_entry(bytes: &[u8]) -> SamdebugResult<u64> {
    let text = String::from_utf8_lossy(bytes);
    let raw = text
        .lines()
        .find_map(|line| line.trim().strip_prefix("start address "))
        .ok_or_else(|| {
            SamdebugError::new(
                ErrorCategory::Project,
                "ELF_ENTRY_MISSING",
                "objdump did not report an entry point",
            )
        })?;
    u64::from_str_radix(raw.trim_start_matches("0x"), 16).map_err(|_| {
        SamdebugError::new(
            ErrorCategory::Project,
            "ELF_ENTRY_INVALID",
            format!("invalid entry point: {raw}"),
        )
    })
}

fn check_cancelled(cancellation: &CancellationToken) -> SamdebugResult<()> {
    if cancellation.is_cancelled() {
        Err(SamdebugError::new(
            ErrorCategory::Interrupted,
            "INTERRUPTED",
            "operation interrupted",
        ))
    } else {
        Ok(())
    }
}

const fn configuration_name(configuration: Configuration) -> &'static str {
    match configuration {
        Configuration::Debug => "Debug",
        Configuration::Release => "Release",
    }
}

fn build_io(code: &str, error: &std::io::Error) -> SamdebugError {
    SamdebugError::new(ErrorCategory::Project, code, error.to_string())
}
