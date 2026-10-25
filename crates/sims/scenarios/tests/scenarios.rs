//! The end-to-end tests: every file in `scenarios/` is run in the sandbox (Linux, root; the
//! dev container's test runner provides both) and must pass. A failing scenario can be run
//! by hand with `sudo target/debug/scenario crates/sims/scenarios/scenarios/NAME.toml`.
//! The external-partner scenarios in `scenarios/interop/` are run by `cargo xtask interop`.

use std::path::{Path, PathBuf};

use scenarios::Scenario;

fn directory() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("scenarios")
}

fn files(dir: &Path) -> Vec<PathBuf> {
    let mut files: Vec<PathBuf> = std::fs::read_dir(dir)
        .unwrap()
        .map(|e| e.unwrap().path())
        .filter(|p| p.extension().is_some_and(|e| e == "toml"))
        .collect();
    files.sort();
    files
}

#[test]
fn every_scenario_file_loads() {
    let all: Vec<PathBuf> = [directory(), directory().join("interop")].iter().flat_map(|d| files(d)).collect();
    assert!(all.len() >= 22, "{all:?}");
    for file in all {
        Scenario::load(&file).unwrap_or_else(|e| panic!("{e}"));
    }
}

/// The bring-up of a charging session starts with SLAC (ISO 15118-3): every simulator scenario runs it, unless it is listed
/// here with the reason.
const WITHOUT_SLAC: &[(&str, &str)] = &[
    ("basic-charging", "the charger offers a nominal duty cycle: basic charging, no SLAC, no high level communication"),
    ("acdp-positioning-rfid", "ACDP: the link is WLAN (ISO 15118-8), there is no PLC"),
    ("acdp-positioning-optical", "ACDP: the link is WLAN (ISO 15118-8), there is no PLC"),
    ("acdp-positioning-unguided", "ACDP: the link is WLAN (ISO 15118-8), there is no PLC"),
    ("acdp-leaves-the-zone", "ACDP: the link is WLAN (ISO 15118-8), there is no PLC"),
    ("acdp-dc", "ACDP: the link is WLAN (ISO 15118-8), there is no PLC"),
    ("acdp-wlan-fades", "ACDP: the link is WLAN (ISO 15118-8), there is no PLC"),
    ("acdp-dc-bpt", "ACDP: the link is WLAN (ISO 15118-8), there is no PLC"),
    ("acdp-pantograph-stuck-down", "ACDP: the link is WLAN (ISO 15118-8), there is no PLC"),
    ("acdp-pantograph-stuck-up", "ACDP: the link is WLAN (ISO 15118-8), there is no PLC"),
];

#[test]
fn every_simulator_scenario_runs_slac_unless_it_is_exempt() {
    for file in files(&directory()) {
        let scenario = Scenario::load(&file).unwrap();
        let name = file.file_stem().unwrap().to_string_lossy().into_owned();
        let (secc, evcc) = (scenario.secc_settings(), scenario.evcc_settings());
        let exempt = WITHOUT_SLAC.iter().any(|(n, _)| *n == name);
        assert_eq!(secc.slac_responder && evcc.slac, !exempt, "{name}: SLAC on both ends (or the scenario is in WITHOUT_SLAC with a reason)");
    }
}

#[cfg(target_os = "linux")]
mod run {
    use super::*;

    fn run(name: &str) {
        run_report(name);
    }

    fn run_report(name: &str) -> scenarios::runner::Report {
        let scenario = Scenario::load(&directory().join(format!("{name}.toml"))).unwrap();
        // A scenario that hangs would hold the whole run: the runner has its own timeouts,
        // this ends the process if they fail too.
        let limit = std::time::Duration::from_secs(scenario.timeout_s + 60);
        let (done_tx, done_rx) = std::sync::mpsc::channel::<()>();
        let watchdog = std::thread::spawn(move || {
            // A dropped sender means the scenario is over.
            if done_rx.recv_timeout(limit) == Err(std::sync::mpsc::RecvTimeoutError::Timeout) {
                eprintln!("a scenario is still running after {limit:?}: ending the run");
                std::process::exit(101);
            }
        });
        let report = scenarios::runner::run(&scenario).expect("the sandbox needs root and a user (sudo)");
        drop(done_tx);
        watchdog.join().unwrap();
        assert!(report.passed(), "scenario {name} failed:\n{}", report.describe());
        let pcap = report.pcap.as_ref().unwrap_or_else(|| panic!("scenario {name}: no capture"));
        assert!(std::fs::metadata(pcap).unwrap().len() > 24, "scenario {name}: the capture is empty");
        report
    }

    /// The packets of a classic pcap file (Ethernet): the IPv6 packets' next header, the UDP/TCP
    /// ports and the payload of UDP.
    fn udp_packets(pcap: &Path) -> Vec<(u16, u16, Vec<u8>)> {
        let data = std::fs::read(pcap).unwrap();
        assert_eq!(&data[..4], &[0xd4, 0xc3, 0xb2, 0xa1], "a little-endian pcap file");
        let (mut at, mut found) = (24, Vec::new());
        while at + 16 <= data.len() {
            let len = u32::from_le_bytes(data[at + 8..at + 12].try_into().unwrap()) as usize;
            let frame = &data[at + 16..(at + 16 + len).min(data.len())];
            at += 16 + len;
            // Ethernet, IPv6 (0x86dd), next header UDP.
            if frame.len() >= 62 && frame[12..14] == [0x86, 0xdd] && frame[20] == 17 {
                let (src, dst) = (u16::from_be_bytes([frame[54], frame[55]]), u16::from_be_bytes([frame[56], frame[57]]));
                found.push((src, dst, frame[62..].to_vec()));
            }
        }
        found
    }

    #[test]
    fn the_capture_of_a_tls_session_carries_its_secrets() {
        let report = run_report("dc-tls-pnc");
        let lines: Vec<String> = udp_packets(report.pcap.as_ref().unwrap())
            .into_iter()
            .filter(|(src, dst, _)| (49152..=65535).contains(src) && *dst == evcc_linux::tls::DISCLOSURE_PORT)
            .map(|(.., payload)| String::from_utf8(payload).unwrap())
            .collect();
        // TLS 1.3: the handshake and the first application traffic secrets, and the exporter secret.
        for name in ["CLIENT_HANDSHAKE_TRAFFIC_SECRET", "SERVER_HANDSHAKE_TRAFFIC_SECRET", "CLIENT_TRAFFIC_SECRET_0", "SERVER_TRAFFIC_SECRET_0", "EXPORTER_SECRET"] {
            assert!(lines.iter().any(|l| l.starts_with(name)), "no {name} in {lines:?}");
        }
    }

    macro_rules! scenario_tests {
        ($($test:ident => $file:literal,)*) => { $(#[test] fn $test() { run($file) })* };
    }

    scenario_tests! {
        dc_plain => "dc-plain",
        dc_bpt => "dc-bpt",
        dc_go_to_pause => "dc-go-to-pause",
        dc_pilot_emergency_by_evse => "dc-pilot-emergency-by-evse",
        dc_pilot_emergency_by_evcc => "dc-pilot-emergency-by-evcc",
        dc_din_pilot_emergency_by_evse => "dc-din-pilot-emergency-by-evse",
        dc_din_session_setup_failed => "dc-din-session-setup-failed",
        dc_welded_contactor => "dc-welded-contactor",
        ac_plain => "ac-plain",
        ac_bpt => "ac-bpt",
        ac_scheduled => "ac-scheduled",
        ac_scheduled_prices => "ac-scheduled-prices",
        ac_scheduled_energy_offer => "ac-scheduled-energy-offer",
        dc_mobility_needs_evse => "dc-mobility-needs-evse",
        dc_slac => "dc-slac",
        basic_charging => "basic-charging",
        acdp_positioning_rfid => "acdp-positioning-rfid",
        acdp_positioning_optical => "acdp-positioning-optical",
        acdp_positioning_unguided => "acdp-positioning-unguided",
        acdp_leaves_the_zone => "acdp-leaves-the-zone",
        acdp_dc => "acdp-dc",
        acdp_wlan_fades => "acdp-wlan-fades",
        acdp_dc_bpt => "acdp-dc-bpt",
        acdp_pantograph_stuck_down => "acdp-pantograph-stuck-down",
        acdp_pantograph_stuck_up => "acdp-pantograph-stuck-up",
        slac_too_far => "slac-too-far",
        slac_ac_restarts => "slac-ac-restarts",
        dc_tls_slac => "dc-tls-slac",
        dc_tls_ed448 => "dc-tls-ed448",
        dc_tls12 => "dc-tls12",
        dc_tls_pnc => "dc-tls-pnc",
        dc_tls_pnc_metering => "dc-tls-pnc-metering",
        dc_tls_profile_violation => "dc-tls-profile-violation",
        dc_delay => "dc-delay",
        dc_din => "dc-din",
        dc_iso2 => "dc-iso2",
        dc_iso2_sales_tariff => "dc-iso2-sales-tariff",
        dc_iso2_renegotiation_by_car => "dc-iso2-renegotiation-by-car",
        ac3_iso2_renegotiation_by_charger => "ac3-iso2-renegotiation-by-charger",
        dc_iso2_pause_resume => "dc-iso2-pause-resume",
        dc_iso2_value_added => "dc-iso2-value-added",
        ac3_iso2_pnc_receipt => "ac3-iso2-pnc-receipt",
        ac3_iso2 => "ac3-iso2",
        dc_iso2_pnc => "dc-iso2-pnc",
        ac3_iso2_pnc => "ac3-iso2-pnc",
        dc_din_ongoing => "dc-din-ongoing",
        dc_din_shutdown => "dc-din-shutdown",
        charger_ends_charge => "charger-ends-charge",
        standby => "standby",
        service_renegotiation => "service-renegotiation",
        service_renegotiation_by_car => "service-renegotiation-by-car",
        schedule_renegotiation => "schedule-renegotiation",
        pause_resume => "pause-resume",
        slac_validation => "slac-validation",
        slac_validation_without_pilot => "slac-validation-without-pilot",
    }

    #[test]
    fn every_scenario_file_has_a_test() {
        let source = std::fs::read_to_string(Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/scenarios.rs")).unwrap();
        for file in files(&directory()) {
            let stem = file.file_stem().unwrap().to_str().unwrap();
            assert!(source.contains(&format!("\"{stem}\"")), "no test for scenarios/{stem}.toml");
        }
    }
}
