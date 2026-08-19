use std::{
    fs,
    path::{Path, PathBuf},
};

use samdebug_core::Configuration;
use samdebug_project::{SourceKind, import_cproj, initialize_project};
use tempfile::TempDir;

fn fixture() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/valid/project.cproj")
}

fn copy_fixture() -> TempDir {
    let temp = tempfile::tempdir().expect("temp fixture");
    copy_tree(
        fixture()
            .parent()
            .expect("fixture parent")
            .parent()
            .expect("fixtures"),
        temp.path(),
    );
    temp
}

fn copy_tree(source: &Path, destination: &Path) {
    fs::create_dir_all(destination).expect("create destination");
    for entry in fs::read_dir(source).expect("read fixture") {
        let entry = entry.expect("fixture entry");
        let target = destination.join(entry.file_name());
        if entry.file_type().expect("file type").is_dir() {
            copy_tree(&entry.path(), &target);
        } else {
            fs::copy(entry.path(), target).expect("copy fixture file");
        }
    }
}

#[test]
fn imports_debug_project_without_mutation() {
    let path = fixture();
    let before = fs::read(&path).expect("read before");
    let imported = import_cproj(&path, Configuration::Debug).expect("import Debug");
    assert_eq!(fs::read(&path).expect("read after"), before);
    let plan = imported.plan;
    assert_eq!(plan.device, "ATSAM4SD32C");
    assert_eq!(plan.output_name, "fixture firmware-Debug");
    assert_eq!(plan.sources.len(), 4);
    assert_eq!(plan.headers, ["include dir/fixture.h"]);
    assert!(
        plan.sources
            .iter()
            .any(|source| source.kind == SourceKind::Assembly)
    );
    assert!(
        plan.sources
            .iter()
            .any(|source| source.kind == SourceKind::PreprocessedAssembly)
    );
    let linked = plan
        .sources
        .iter()
        .find(|source| source.external)
        .expect("linked file");
    assert_eq!(linked.link_name.as_deref(), Some("linked/linked.c"));
    assert_eq!(plan.libraries, ["libm", "libfixture"]);
    assert_eq!(plan.library_search_paths, ["libs"]);
    assert_eq!(plan.linker_script.as_deref(), Some("linker/flash.ld"));
    assert!(plan.compiler_flags.contains(&"-O1".into()));
    assert!(plan.compiler_flags.contains(&"-g3".into()));
    assert!(
        plan.compiler_flags
            .contains(&"-DSPACE_VALUE=hello world".into())
    );
    assert!(plan.linker_flags.contains(&"-Wl,--gc-sections".into()));
    assert_eq!(imported.warnings.len(), 2);
}

#[test]
fn imports_release_and_is_deterministic() {
    let first = import_cproj(&fixture(), Configuration::Release).expect("first import");
    let second = import_cproj(&fixture(), Configuration::Release).expect("second import");
    assert_eq!(first, second);
    assert_eq!(first.plan.symbols, ["NDEBUG"]);
    assert!(first.plan.compiler_flags.contains(&"-Os".into()));
    assert!(!first.plan.artifacts.bin);
}

#[test]
fn initializes_config_and_plan_without_editing_project() {
    let temp = copy_fixture();
    let path = temp.path().join("valid/project.cproj");
    let before = fs::read(&path).expect("read project");
    let report = initialize_project(&path, Configuration::Debug).expect("initialize");
    assert_eq!(fs::read(&path).expect("read project after"), before);
    let config = fs::read_to_string(temp.path().join("valid/samdebug.toml")).expect("config");
    assert!(config.contains("channel = \"pinned\""));
    assert!(config.contains("transport = \"swd\""));
    assert!(config.contains("speed_khz = 1000"));
    let plan: serde_json::Value = serde_json::from_slice(
        &fs::read(temp.path().join("valid/.samdebug/import-plan.json")).expect("plan"),
    )
    .expect("valid plan JSON");
    assert_eq!(plan["configuration"], "Debug");
    assert_eq!(report.sources, 4);
    let error = initialize_project(&path, Configuration::Debug).expect_err("do not overwrite");
    assert_eq!(error.code(), "CONFIG_ALREADY_EXISTS");
}

#[test]
fn rejects_bounded_unsupported_constructs_with_locations() {
    for (needle, replacement, expected) in [
        ("com.Atmel.ARMGCC.C", "vendor.cpp", "UNSUPPORTED_TOOLCHAIN"),
        (
            "<Language>C</Language>",
            "<Language>C++</Language>",
            "UNSUPPORTED_LANGUAGE",
        ),
        (
            "<Import Project=",
            "<Import Project=\"custom.targets\" /><Import Project=",
            "CUSTOM_MSBUILD_IMPORT_REJECTED",
        ),
        (
            "<ItemGroup>",
            "<Target Name=\"Injected\"><Exec Command=\"echo unsafe\" /></Target><ItemGroup>",
            "IMPORTED_BUILD_HOOK_REJECTED",
        ),
        ("src\\main.c", "src\\*.c", "WILDCARD_INPUT_REJECTED"),
        (
            " '$(Configuration)' == 'Debug' ",
            " '$(Platform)' == 'ARM' ",
            "UNRESOLVED_CONDITION",
        ),
        (
            "<Name>fixture firmware</Name>",
            "<Name>$([System.IO.File]::ReadAllText('secret'))</Name>",
            "MSBUILD_PROPERTY_FUNCTION_REJECTED",
        ),
    ] {
        let temp = copy_fixture();
        let path = temp.path().join("valid/project.cproj");
        let text =
            fs::read_to_string(&path)
                .expect("read fixture")
                .replacen(needle, replacement, 1);
        fs::write(&path, text).expect("write malformed fixture");
        let error = import_cproj(&path, Configuration::Debug).expect_err(expected);
        assert_eq!(error.code(), expected);
        let serialized = serde_json::to_value(error).expect("serialize error");
        assert!(serialized["details"]["line"].as_u64().is_some());
        assert!(serialized["details"]["column"].as_u64().is_some());
    }
}

#[test]
fn rejects_cpp_generated_missing_duplicate_and_unknown_macro_inputs() {
    for (needle, replacement, expected) in [
        (
            "src\\main.c",
            "src\\main.cpp",
            "GENERATED_INPUT_UNAVAILABLE",
        ),
        (
            "src\\main.c",
            "src\\missing.c",
            "GENERATED_INPUT_UNAVAILABLE",
        ),
        (
            "<ItemGroup>",
            "<ItemGroup><Compile Include=\"src\\main.c\" /></ItemGroup><ItemGroup>",
            "CONFLICTING_DUPLICATE_INPUT",
        ),
        (
            "../libs",
            "$(UnknownRoot)/libs",
            "UNKNOWN_MSBUILD_EXPRESSION",
        ),
    ] {
        let temp = copy_fixture();
        let path = temp.path().join("valid/project.cproj");
        if expected == "GENERATED_INPUT_UNAVAILABLE" && replacement.ends_with("main.cpp") {
            fs::rename(
                temp.path().join("valid/src/main.c"),
                temp.path().join("valid/src/main.cpp"),
            )
            .expect("rename C++ source");
        }
        let text =
            fs::read_to_string(&path)
                .expect("read fixture")
                .replacen(needle, replacement, 1);
        fs::write(&path, text).expect("write fixture");
        let error = import_cproj(&path, Configuration::Debug).expect_err(expected);
        let actual = if replacement.ends_with("main.cpp") {
            "CPP_INPUT_REJECTED"
        } else {
            expected
        };
        assert_eq!(error.code(), actual);
    }
}

#[test]
fn imports_real_project_when_explicitly_supplied() {
    let Some(path) = std::env::var_os("SAMDEBUG_REAL_CPROJ").map(PathBuf::from) else {
        return;
    };
    let before = fs::read(&path).expect("read real project");
    for configuration in [Configuration::Debug, Configuration::Release] {
        let result = import_cproj(&path, configuration).expect("import real project");
        assert_eq!(result.plan.device, "ATSAM4SD32C");
        assert!(result.plan.sources.len() > 30);
        assert!(result.plan.linker_script.is_some());
    }
    assert_eq!(fs::read(path).expect("reread real project"), before);
}
