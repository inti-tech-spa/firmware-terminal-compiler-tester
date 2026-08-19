#![cfg(target_os = "macos")]

use std::{path::PathBuf, time::Duration};

use samdebug_core::CancellationToken;
use samdebug_debug::{
    FirmwareArtifact, OpenOcdConfig, OpenOcdProgrammer, ProgramOperation, list_probes,
};
use samdebug_tools::{MacUsbProbeProvider, SystemProcessRunner};

#[test]
#[ignore = "requires an externally powered ATSAM4SD32C, Atmel-ICE, audited tools, ELF, and explicit destructive confirmation"]
fn physical_erase_flash_verify_reset_halt_and_release() {
    let serial = required("SAMDEBUG_PHYSICAL_PROBE_SERIAL");
    let elf = PathBuf::from(required("SAMDEBUG_PHYSICAL_ELF"));
    let build_directory = elf.parent().expect("ELF build directory").to_path_buf();
    let artifact = FirmwareArtifact {
        path: elf,
        build_directory,
    };
    let openocd = PathBuf::from(required("SAMDEBUG_PHYSICAL_OPENOCD"));
    let scripts = PathBuf::from(required("SAMDEBUG_PHYSICAL_OPENOCD_SCRIPTS"));
    let expected = format!("erase:{serial},flash:{serial}");
    assert_eq!(required("SAMDEBUG_PHYSICAL_CONFIRM"), expected);

    let probes = MacUsbProbeProvider;
    let listed = list_probes(&probes).expect("discover physical Atmel-ICE");
    assert!(listed.probes.iter().any(|probe| probe.serial == serial));
    let runner = SystemProcessRunner;
    let mut config = OpenOcdConfig::new(openocd, scripts, 1_000);
    config.timeout = Duration::from_mins(1);
    let programmer = OpenOcdProgrammer::new(&probes, &runner, config);
    let cancellation = CancellationToken::new();

    let erased = programmer
        .execute(
            ProgramOperation::Erase,
            &serial,
            Some(&format!("erase:{serial}")),
            None,
            &cancellation,
        )
        .expect("erase and erase-check physical target");
    assert_eq!(erased.verification, "verified");
    assert_eq!(erased.reset, "halted");

    let flashed = programmer
        .execute(
            ProgramOperation::Flash,
            &serial,
            Some(&format!("flash:{serial}")),
            Some(&artifact),
            &cancellation,
        )
        .expect("flash, independently verify, and reset physical target");
    assert_eq!(flashed.verification, "verified");
    assert_eq!(flashed.reset, "running");

    let halted = programmer
        .execute(ProgramOperation::Halt, &serial, None, None, &cancellation)
        .expect("reopen released probe and halt target");
    assert_eq!(halted.reset, "halted");
    let reset = programmer
        .execute(ProgramOperation::Reset, &serial, None, None, &cancellation)
        .expect("reset target and release probe");
    assert_eq!(reset.reset, "running");
}

fn required(name: &str) -> String {
    std::env::var(name).unwrap_or_else(|_| panic!("{name} is required for physical acceptance"))
}
