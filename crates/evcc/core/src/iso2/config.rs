//! Configuration of the ISO 15118-2 session machine.

use app_protocol::AppProtocol;

/// The energy transfer mode the vehicle asks for.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Mode {
    /// DC through the extended pins of a combined or CHAdeMO-style inlet; the vehicle's own
    /// [`crate::ev::Ev::dc_connector`] decides between `DC_core`, `DC_extended` and `DC_combo_core`
    /// if this is `Dc`.
    Dc,
    AcSinglePhase,
    AcThreePhase,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Config {
    /// The protocol offered in `SupportedAppProtocolReq`: `urn:iso:15118:2:2013:MsgDef`, 2.0.
    pub offer: AppProtocol<'static>,
    pub mode: Mode,
    /// Pay with the contract (Plug & Charge, `PaymentDetailsReq`) when the charger offers it and the
    /// vehicle has one; otherwise with `ExternalPayment`.
    pub prefer_contract: bool,
    /// `V2G_EVCC_Msg_Timeout`: 2 s for every message but `CurrentDemandReq` (0.25 s), `ChargingStatusReq`
    /// (0.25 s is not in the standard; 2 s is used) and `PaymentDetailsReq` (5 s).
    pub response_timeout_ms: u32,
    pub current_demand_timeout_ms: u32,
    pub payment_details_timeout_ms: u32,
    /// Bound on repeated `Ongoing` exchanges.
    pub ongoing_timeout_ms: u32,
    pub poll_interval_ms: u32,
    pub communication_setup_timeout_ms: u32,
    pub ready_to_charge_timeout_ms: u32,
    pub cable_check_timeout_ms: u32,
    pub precharge_timeout_ms: u32,
    pub precharge_tolerance_v: u16,
    /// Pause between a `CurrentDemandRes` or `ChargingStatusRes` and the next request.
    pub charge_loop_interval_ms: u32,
    pub sequence_performance_ms: u32,
    pub welding_detection: bool,
    pub welding_detection_voltage_v: u16,
    pub send_charging_profile: bool,
    pub enforce_cpd_limits: bool,
    /// `MaxEntriesSAScheduleTuple` of the charge parameter discovery.
    pub max_schedule_tuples: u16,
    /// Value-added services (the ids of the `ServiceList` of `ServiceDiscoveryRes`) the vehicle
    /// asks about with `ServiceDetailReq` and, when the charger offers parameters for them,
    /// selects together with the charge service (at most [`MAX_VALUE_ADDED`]).
    pub value_added: &'static [u16],
    /// Resume this paused session (`SessionSetupReq` with its id; the charger answers
    /// `OK_OldSessionJoined`) instead of starting a new one.
    pub resume: Option<crate::session::Resume>,
}

/// The most value-added services a session asks about.
pub const MAX_VALUE_ADDED: usize = 4;

impl Config {
    /// ISO 15118-2 DC, version 2.0, schema id 1.
    pub const ISO2_DC: Config = Config {
        offer: AppProtocol {
            protocol_namespace: "urn:iso:15118:2:2013:MsgDef",
            version_number_major: 2,
            version_number_minor: 0,
            schema_id: 1,
            priority: 1,
        },
        mode: Mode::Dc,
        prefer_contract: false,
        response_timeout_ms: 2000,
        current_demand_timeout_ms: 250,
        payment_details_timeout_ms: 5000,
        ongoing_timeout_ms: 60_000,
        poll_interval_ms: 200,
        communication_setup_timeout_ms: 20_000,
        ready_to_charge_timeout_ms: 150_000,
        cable_check_timeout_ms: 40_000,
        precharge_timeout_ms: 7_000,
        precharge_tolerance_v: 20,
        charge_loop_interval_ms: 100,
        sequence_performance_ms: 59_000,
        welding_detection: true,
        welding_detection_voltage_v: 60,
        send_charging_profile: true,
        enforce_cpd_limits: true,
        max_schedule_tuples: 3,
        value_added: &[],
        resume: None,
    };

    /// ISO 15118-2 AC with the given number of phases.
    pub const fn ac(three_phase: bool) -> Config {
        Config { mode: if three_phase { Mode::AcThreePhase } else { Mode::AcSinglePhase }, ..Self::ISO2_DC }
    }
}
