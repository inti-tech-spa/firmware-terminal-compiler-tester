# Microchip Studio project support

## Supported v1 subset

Version 1 accepts Microchip Studio 7 `.cproj` files only when all of the
following hold:

- `ToolchainName` is `com.Atmel.ARMGCC.C`.
- `Language` is `C`, `OutputType` is `Executable`, and `avrdevice` is
  `ATSAM4SD32C`.
- Exactly one requested `Debug` or `Release` property group can be resolved.
- Inputs are explicit `Compile` items using C, preprocessed assembly, or
  assembly extensions.
- Include paths, symbols, optimization/debug settings, compiler/assembler
  flags, libraries, library paths, linker flags/script, output name, and
  requested derivative artifacts are literal or use supported project macros.

Supported macros are `$(MSBuildProjectName)`, `$(MSBuildProjectDirectory)`, and
`$(Configuration)`. Windows separators are normalized after macro expansion.
The Microchip Studio Arm toolchain path is mapped to the managed Arm toolchain.
Relative project and ASF paths remain relative to the `.cproj` directory.

Absolute Microchip Studio CMSIS or SAM4S DFP include paths are not mapped to a
bundled pack. They are omitted with a structured warning only when every header
used by the imported compilation resolves through a later project-relative
include path. Otherwise import fails with `MISSING_VENDOR_PACK` and instructs
the user to add the required vendor files to the project. This matches the v1
decision not to download or redistribute Microchip packs.

## Explicit rejection

The importer rejects with file and XML-location diagnostics:

- C++ or non-ARM-GCC projects, libraries, and devices other than ATSAM4SD32C.
- Custom MSBuild targets/imports, external Makefiles, wildcards, item transforms,
  unknown macros, unresolved conditions, or arbitrary property functions.
- Generated inputs not already present at import time.
- Pre-build, post-build, custom-build, or shell command hooks.
- Conflicting duplicate settings whose precedence cannot be represented.

Unsupported constructs are never ignored. The importer does not execute any
content found in XML.

## Non-mutation guarantee

The `.cproj`, `.atsln`, source tree, ASF tree, linked external sources, and
Studio `Debug`/`Release` directories are never written. Import metadata,
normalized plans, objects, and artifacts are confined to the canonical
project-local `.samdebug/`. Symlink or canonical-path escapes are rejected.
This permits reopening the unchanged project in Microchip Studio.

Compatibility evidence compares normalized compile/link inputs and ELF
properties with the existing `smd-motherboard-v2-firmware` project and its
known Microchip Studio outputs.
