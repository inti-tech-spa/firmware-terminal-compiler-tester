# ATSAM4SD32C build pipeline

`samdebug build` reloads `samdebug.toml` and the authoritative `.cproj` on every
invocation. It invokes the selected Arm GNU executables directly with argv; no
command is passed through a shell. Outputs are confined to
`.samdebug/build/<configuration>/` and source, ASF, Studio project, and Studio
`Debug`/`Release` files remain read-only.

The normalized compiler command preserves imported symbols, include paths,
optimization/debug levels, and the bounded compatibility flags used by the
supported project. Imported free-form flags are allowlisted: warning, language,
optimization, debugging, section, aliasing, macro, and the project's documented
inlining setting are accepted. Response files, compiler wrappers, specs,
plugins, assembler/preprocessor/linker forwarding, output selection, and
target/ABI overrides are rejected before any tool runs. It adds the device and
ABI settings that Microchip Studio derives from `ATSAM4SD32C` rather than storing
them as ordinary project flags:

- `-D__SAM4SD32C__`
- `-mcpu=cortex-m4`
- `-mthumb`
- `-mfloat-abi=soft`
- `-mlong-calls`

GCC 15 promotes incompatible pointer types to an error whereas the Studio 7
toolchain used for the existing project emitted a warning. The managed build
adds `-Wno-error=incompatible-pointer-types` so that this existing warning does
not prevent migration; it does not suppress the diagnostic. Source/header parent
directories are appended after explicit include directories. This makes the
repository's Release configuration usable despite its omission of newer
project-local module paths, without changing explicit include precedence.

The linker command uses the imported object set, linker flags, linker script,
library paths, libraries, garbage collection choice, and entry point. Libraries
are enclosed in a GNU linker start/end group as in Studio's generated command.

Each external tool writes only into a private per-build staging directory.
Outputs must be non-empty, single-link regular files before atomic promotion to
their final paths; existing symlink, hard-link, and non-file targets are rejected.
Each successful build produces the ELF and map plus requested BIN, Intel HEX,
S-record, EEPROM, and source/disassembly outputs. Dependency files, normalized
argv, and the compiler path/content hash drive incremental compilation. GNU size
output is stored beside the artifacts. The build fails if flash exceeds 2 MiB,
RAM exceeds 160 KiB, the ELF is not ARMv7E-M EABI5 soft-float, or its entry point
is outside ATSAM4SD32C flash (`0x00400000..0x00600000`). Ctrl-C terminates and
reaps the active finite tool process group, removes staging, and returns 130.

`samdebug artifacts` lists regular top-level artifacts with sizes.
`samdebug clean` removes only the selected configuration directory after
canonical confinement and symlink checks.
