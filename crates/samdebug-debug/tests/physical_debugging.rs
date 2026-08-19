use std::{path::PathBuf, time::Duration};

use samdebug_core::CancellationToken;
use samdebug_debug::{
    FirmwareArtifact, GdbMiConfig, OpenOcdConfig, OwnedDebugSession, SessionState,
};
use samdebug_tools::MacUsbProbeProvider;

fn required(name: &str) -> String {
    std::env::var(name).unwrap_or_else(|_| panic!("{name} is required"))
}

fn configuration() -> (String, FirmwareArtifact, OpenOcdConfig, GdbMiConfig) {
    let serial = required("SAMDEBUG_PHYSICAL_PROBE_SERIAL");
    let elf = PathBuf::from(required("SAMDEBUG_PHYSICAL_ELF"));
    let build_directory = elf.parent().expect("ELF parent").to_path_buf();
    let firmware = FirmwareArtifact {
        path: elf,
        build_directory,
    };
    let mut openocd = OpenOcdConfig::new(
        PathBuf::from(required("SAMDEBUG_PHYSICAL_OPENOCD")),
        PathBuf::from(required("SAMDEBUG_PHYSICAL_OPENOCD_SCRIPTS")),
        1_000,
    );
    openocd.timeout = Duration::from_secs(20);
    let gdb = GdbMiConfig::new(PathBuf::from(required("SAMDEBUG_PHYSICAL_GDB")));
    (serial, firmware, openocd, gdb)
}

#[test]
#[ignore = "requires an externally powered ATSAM4SD32C, Atmel-ICE, audited tools, and ELF"]
fn physical_gdb_mi_non_destructive_smoke() {
    let (serial, firmware, openocd, gdb) = configuration();
    let mut session = OwnedDebugSession::launch(
        &MacUsbProbeProvider,
        &openocd,
        &gdb,
        &serial,
        &firmware,
        &CancellationToken::new(),
    )
    .expect("launch debug session");
    session
        .insert_breakpoint("main", true)
        .expect("temporary breakpoint");
    session.continue_target().expect("continue");
    let frame = session
        .wait_until_stopped()
        .expect("breakpoint stop")
        .expect("frame");
    assert_eq!(frame.function, "main");
    assert!(!session.stack_frames(0, 8).expect("stack").is_empty());
    assert_eq!(
        session
            .registers(&["pc".into()])
            .expect("program counter")
            .len(),
        1
    );
    session.stop().expect("stop");
}

#[test]
#[ignore = "requires an externally powered ATSAM4SD32C, Atmel-ICE, audited tools, ELF, and explicit firmware-load confirmation"]
fn physical_gdb_mi_break_step_inspect_load_reconnect_and_cleanup() {
    let (serial, firmware, openocd, gdb) = configuration();
    assert_eq!(
        required("SAMDEBUG_PHYSICAL_DEBUG_CONFIRM"),
        format!("firmware.load:{serial}")
    );
    let mut session = OwnedDebugSession::launch(
        &MacUsbProbeProvider,
        &openocd,
        &gdb,
        &serial,
        &firmware,
        &CancellationToken::new(),
    )
    .expect("launch debug session");
    assert_eq!(session.state(), SessionState::Halted);
    let breakpoint = session
        .insert_breakpoint("main", false)
        .expect("breakpoint main");
    session.continue_target().expect("continue");
    let frame = session
        .wait_until_stopped()
        .expect("breakpoint stop")
        .expect("current frame");
    assert_eq!(frame.function, "main");
    let frames = session.stack_frames(0, 16).expect("stack frames");
    assert!(!frames.is_empty());
    let _variables = session.variables(0).expect("variables");
    let registers = session
        .registers(&["r0".into(), "pc".into()])
        .expect("registers");
    assert_eq!(registers.len(), 2);
    let address = frame.address.expect("frame address");
    assert_eq!(
        session.read_memory(address, 4).expect("memory").bytes.len(),
        4
    );
    session.step().expect("step");
    session.next_target().expect("next");
    session.reset_halt().expect("reset halt");
    session
        .load_firmware(&format!("firmware.load:{serial}"))
        .expect("authorized load and compare-sections");
    session
        .remove_breakpoint(&breakpoint.id)
        .expect("remove breakpoint");
    session.stop().expect("stop session");
    assert_eq!(session.state(), SessionState::Idle);

    let mut reconnected = OwnedDebugSession::launch(
        &MacUsbProbeProvider,
        &openocd,
        &gdb,
        &serial,
        &firmware,
        &CancellationToken::new(),
    )
    .expect("explicit reconnect");
    reconnected.stop().expect("stop reconnected session");
}
