# ATSAM4SD32C build pipeline

`samdebug build` reloads `samdebug.toml` and the authoritative `.cproj` on every
invocation. It invokes the selected Arm GNU executables directly with argv; no
command is passed through a shell. Outputs are confined to
`.samdebug/build/<configuration>/` and source, ASF, Studio project, and Studio
`Debug`/`Release` files remain read-only.

The normalized compiler command preserves imported symbols, include paths,
optimization/debug levels, and miscellaneous flags. It adds the device and ABI
settings that Microchip Studio derives from `ATSAM4SD32C` rather than storing as
ordinary project flags:

- `-D__SAM4SD32C__`
- `-mcpu=cortex-m4`
- `-mthumb`
- `-mfloat-abi=soft`
- `-mlong-calls`

GCC 15 promotes incompatible pointer types to an error whereas the Studio 7
toolchain used for the existing project emitted a warning. The managed build
adds `-Wno-error=incompatible-pointer-types` so that this existing warning does
not prevent migration; it does not suppress the diagnostic. Explicit `.cproj`
flags are otherwise retained. Source/header parent directories are appended
after explicit include directories. This makes the repository's Release
configuration usable despite its omission of newer project-local module paths,
without changing explicit include precedence.

The linker command uses the imported object set, linker flags, linker script,
library paths, libraries, garbage collection choice, and entry point. Libraries
are enclosed in a GNU linker start/end group as in Studio's generated command.

Each successful build produces the ELF and map plus requested BIN, Intel HEX,
S-record, EEPROM, and source/disassembly outputs. Dependency files and argv
fingerprints drive incremental compilation. GNU size output is stored beside
the artifacts. The build fails if flash exceeds 2 MiB, RAM exceeds 160 KiB, or
the ELF entry point is outside ATSAM4SD32C flash (`0x00400000..0x00600000`).

`samdebug artifacts` lists regular top-level artifacts with sizes.
`samdebug clean` removes only the selected configuration directory after
canonical confinement and symlink checks.
