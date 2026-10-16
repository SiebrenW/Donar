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
    /// The energy transfer service to select (`services::service_id`).
    pub energy_service: u16,
    /// A parameter set is acceptable when it holds every `(name, value)` listed here as an
    /// integer parameter. The first acceptable set is selected.
    pub parameter_filter: &'static [(&'static str, i32)],
    /// Resume this paused session instead of starting a new one (V2G20-1541): the
    /// `SessionSetupReq` carries its id; when the charger agrees, authorization and service
    /// selection are not repeated.
    pub resume: Option<super::types::Resume>,
    /// Answer `EVSENotification` Pause (V2G20-1850) with a pause of the session
    /// (`SessionStopReq` Pause) instead of a standby period; the standard allows either.
    pub pause_on_notification: bool,
}

impl Config {
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
        resume: None,
        pause_on_notification: false,
    };
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
