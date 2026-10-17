//! Session configuration and the units helpers it needs.

use app_protocol::AppProtocol;


#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Config {
    /// The protocol offered in `SupportedAppProtocolReq`.
    pub offer: AppProtocol<'static>,
    /// How long to wait for a response: `V2G_EVCC_Msg_Timeout(MessageType)` is 2 s for every
    /// message we send except `ServiceDetailReq` and `CertificateInstallationReq`
    /// (ISO 15118-20:2022 Table 215). The charge loop has its own timing (added with it).
    pub response_timeout_ms: u32,
    /// `V2G_EVCC_Msg_Timeout(ServiceDetailReq)`: 5 s (Table 215).
    pub service_detail_timeout_ms: u32,
    /// `V2G_EVCC_Ongoing_Timeout`: the bound on a repeated `Ongoing` exchange such as
    /// authorization polling; 60 s (Table 215).
    pub ongoing_timeout_ms: u32,
    /// Pause between repeats of a request while the EVSE answers `Ongoing` (0: repeat at
    /// once). The standard only bounds the time (V2G20-1582); this keeps the line quiet.
    pub poll_interval_ms: u32,
    /// `V2G_EVCC_DC_CableCheck_Timeout`: 40 s from the first request (Table 223, V2G20-700).
    pub cable_check_timeout_ms: u32,
    /// `V2G_EVCC_DC_PreCharge_Timeout`: 10 s from the first request (Table 223, V2G20-704).
    pub precharge_timeout_ms: u32,
    /// Pre-charge is done when the voltage at the inlet differs less than this from the battery
    /// voltage (IEC 61851-23:2023 CC.5.1 / t9: 20 V).
    pub precharge_tolerance_v: u16,
    /// `V2G_EVCC_Msg_Timeout(DC_ChargeLoopReq)`: 0.5 s (Table 217).
    pub charge_loop_timeout_ms: u32,
    /// Pause between a `DC_ChargeLoopRes` and the next request. The next request has to go out
    /// within `V2G_EVCC_Sequence_Performance_Time(DC_ChargeLoopReq)`, 0.25 s (Table 217), so
    /// keep this well below that.
    pub charge_loop_interval_ms: u32,
    /// `V2G_EVCC_CommunicationSetup_Timeout`: 20 s from the connection until `SessionSetupRes`
    /// (Table 223, V2G20-2298, V2G20-448).
    pub communication_setup_timeout_ms: u32,
    /// Welding detection is over when the voltage the EVSE reports at its (disabled) side B,
    /// `EVSEPresentVoltage` in `DC_WeldingDetectionRes`, has dropped below this. IEC 61851-23
    /// CC.2.4 requires a disabled side B to discharge to below 60 V DC within 2 s of the EV
    /// opening its contactor; a welded contactor keeps the battery voltage there. The standards
    /// leave the criterion to the EV, so this is a policy: set it to what the vehicle needs.
    pub welding_detection_voltage_v: u16,
    /// `V2G_EVCC_Sequence_Performance_Time`: 40 s (Table 215); the time the EVCC may spend on
    /// the vehicle (closing contactors, ramping down, opening contactors) before its next request.
    pub sequence_performance_ms: u32,
    /// ISO 15118-20 runs over TLS 1.3 only (V2G20-1235, V2G20-1237): the EVCC must not offer it
    /// on a plain connection. Set this for a test setup without TLS.
    pub allow_unsecured: bool,
    /// `MaximumSupportingPoints` of the schedule exchange: how many entries of a schedule the
    /// EV can take (12..=1024).
    pub maximum_supporting_points: u16,
    /// The energy transfer service to select: [`SERVICE_ID_DC`] or [`SERVICE_ID_DC_BPT`].
    pub energy_service: u16,
    /// A parameter set is acceptable when it holds every `(name, value)` listed here as an
    /// integer parameter. The first acceptable set is selected.
    pub parameter_filter: &'static [(&'static str, i32)],
    /// Like `parameter_filter`, but a set that does not have the parameter at all is acceptable
    /// (the Python library's DC_BPT sets carry neither `BPTChannel` nor `GeneratorMode`).
    pub optional_parameters: &'static [(&'static str, i32)],
    /// Resume this paused session instead of starting a new one (V2G20-1541): the
    /// `SessionSetupReq` carries its id; when the charger agrees, authorization and service
    /// selection are not repeated.
    pub resume: Option<super::types::Resume>,
    /// Answer `EVSENotification` Pause (V2G20-1850) with a pause of the session
    /// (`SessionStopReq` Pause) instead of a standby period; the standard allows either.
    pub pause_on_notification: bool,
    /// Authorize with Plug & Charge when the charger offers it and the vehicle has a contract
    /// ([`crate::ev::Ev::contract`]); otherwise, and by default, with EIM.
    pub prefer_pnc: bool,
    /// After a `WARNING` for a Plug & Charge authorization, authorize with EIM if the charger
    /// offered it as well (V2G20-1583); otherwise the session ends with the response code.
    pub pnc_fallback_to_eim: bool,
    /// Keep the limits of the charge loop within those of the charge parameter discovery
    /// (V2G20-2198, 2199): maximums no higher, minimums no lower. Off, the vehicle sends whatever
    /// [`crate::ev::Ev::dc_limits`] says, for tests of a charger.
    pub enforce_cpd_limits: bool,
    /// Scheduled control mode (ControlMode 1): the charger offers schedules, the vehicle selects one
    /// (the first) and follows it in the charge loop. For the AC services only; the DC charge loop
    /// is Dynamic.
    pub scheduled: bool,
}

impl Config {
    /// Bidirectional power transfer: the service `DC_BPT` with a unified channel and grid-following
    /// generator mode (Table 207: `BPTChannel` 1, `GeneratorMode` 1).
    pub const DC_BPT_20: Config = Config {
        energy_service: SERVICE_ID_DC_BPT,
        parameter_filter: &[("ControlMode", 2), ("MobilityNeedsMode", 1)],
        optional_parameters: &[("BPTChannel", 1), ("GeneratorMode", 1)],
        ..Config::DC_20
    };

    /// Whether the session is a BPT one.
    pub fn bpt(&self) -> bool {
        matches!(self.energy_service, SERVICE_ID_DC_BPT | SERVICE_ID_AC_BPT)
    }

    /// Whether the session is an AC one (no cable check, pre-charge, contactors or welding
    /// detection; the charge loop is `AC_ChargeLoop`).
    pub fn ac(&self) -> bool {
        matches!(self.energy_service, SERVICE_ID_AC | SERVICE_ID_AC_BPT)
    }

    /// ISO 15118-20 AC, version 1.0, schema id 2. A parameter set of the `AC` service is
    /// acceptable with dynamic control and the mobility needs from the EVCC (Table 205).
    pub const AC_20: Config = Config {
        offer: AppProtocol {
            protocol_namespace: "urn:iso:std:iso:15118:-20:AC",
            version_number_major: 1,
            version_number_minor: 0,
            schema_id: 2,
            priority: 1,
        },
        energy_service: SERVICE_ID_AC,
        ..Config::DC_20
    };

    /// AC bidirectional power transfer: the service `AC_BPT`. Its parameter sets carry the grid
    /// code parameter `GridCodeIslandingDetectionMethod`, which the vehicle does not select on.
    pub const AC_BPT_20: Config = Config { energy_service: SERVICE_ID_AC_BPT, ..Config::AC_20 };

    /// ISO 15118-20 DC, version 1.0, schema id 1, top priority.
    pub const DC_20: Config = Config {
        offer: AppProtocol {
            protocol_namespace: "urn:iso:std:iso:15118:-20:DC",
            version_number_major: 1,
            version_number_minor: 0,
            schema_id: 1,
            priority: 1,
        },
        response_timeout_ms: 2000,
        service_detail_timeout_ms: 5000,
        ongoing_timeout_ms: 60_000,
        poll_interval_ms: 200,
        cable_check_timeout_ms: 40_000,
        precharge_timeout_ms: 10_000,
        precharge_tolerance_v: 20,
        charge_loop_timeout_ms: 500,
        charge_loop_interval_ms: 100,
        communication_setup_timeout_ms: 20_000,
        welding_detection_voltage_v: 60,
        sequence_performance_ms: 40_000,
        allow_unsecured: false,
        maximum_supporting_points: 12,
        energy_service: SERVICE_ID_DC,
        // ISO 15118-20:2022 Table 207: ControlMode 2 = Dynamic, MobilityNeedsMode 1 = mobility
        // needs provided by the EVCC. `Connector` follows the vehicle's inlet
        // ([`Ev::dc_connector`]) and is matched separately.
        parameter_filter: &[("ControlMode", 2), ("MobilityNeedsMode", 1)],
        optional_parameters: &[],
        resume: None,
        pause_on_notification: false,
        prefer_pnc: false,
        pnc_fallback_to_eim: true,
        enforce_cpd_limits: true,
        scheduled: false,
    };

    /// The same session in Scheduled control mode (ISO 15118-20:2022 Table 205: `ControlMode` 1
    /// with the mobility needs from the EVCC).
    pub const fn scheduled(self) -> Config {
        Config { scheduled: true, parameter_filter: &[("ControlMode", 1), ("MobilityNeedsMode", 1)], ..self }
    }
}

/// A `RationalNumber` (value * 10^exponent) in volts, for comparisons.
pub(crate) fn rational_to_volts(r: iso15118_20::common_types::RationalNumber) -> f32 {
    f32::from(r.value) * pow10(r.exponent)
}

pub(crate) fn pow10(exp: i8) -> f32 {
    let mut v = 1.0f32;
    for _ in 0..exp.unsigned_abs() {
        v = if exp < 0 { v / 10.0 } else { v * 10.0 };
    }
    v
}


/// Service id of DC energy transfer (ISO 15118-20:2022 Table 204, V2G20-1355).
pub const SERVICE_ID_DC: u16 = 2;
/// Service id of DC bidirectional power transfer (Table 204).
pub const SERVICE_ID_DC_BPT: u16 = 6;
/// Service id of AC energy transfer (Table 204).
pub const SERVICE_ID_AC: u16 = 1;
/// Service id of AC bidirectional power transfer (Table 204).
pub const SERVICE_ID_AC_BPT: u16 = 5;
