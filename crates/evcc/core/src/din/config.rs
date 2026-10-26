//! Configuration of the DIN SPEC 70121 session machine.

use app_protocol::AppProtocol;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Config {
    /// The protocol offered in `SupportedAppProtocolReq`: the namespace `urn:din:70121:2012:MsgDef`
    /// with version 2.0 (V2G-DC-229).
    pub offer: AppProtocol<'static>,
    /// `V2G_EVCC_Msg_Timeout`: 2 s for every message but `CurrentDemandReq` (Table 75).
    pub response_timeout_ms: u32,
    /// `V2G_EVCC_Msg_Timeout(CurrentDemandReq)`: 0.25 s (Table 75).
    pub current_demand_timeout_ms: u32,
    /// The bound on a repeated `Ongoing` exchange (contract authentication, charge parameter
    /// discovery) and on welding detection. The standard bounds the time only through the
    /// sequence timers of Table 75; this is the EVCC's own policy.
    pub ongoing_timeout_ms: u32,
    /// Pause between repeats of a request while the EVSE answers `Ongoing`, between pre-charge
    /// requests and between welding detection requests (0: repeat at once).
    pub poll_interval_ms: u32,
    /// `V2G_EVCC_CommunicationSetup_Timeout`: 20 s from the connection until `SessionSetupRes`
    /// (Table 77).
    pub communication_setup_timeout_ms: u32,
    /// `V2G_EVCC_ReadyToCharge_Timeout`: 150 s from the connection until the first
    /// `PowerDeliveryRes` (Table 77).
    pub ready_to_charge_timeout_ms: u32,
    /// `V2G_EVCC_CableCheck_Timeout`: 40 s from the first `CableCheckReq` (Table 77).
    pub cable_check_timeout_ms: u32,
    /// `V2G_EVCC_PreCharge_Timeout`: 7 s from the first `PreChargeReq` (Table 77).
    pub precharge_timeout_ms: u32,
    /// Pre-charge is done when the voltage at the inlet differs less than this from the battery
    /// voltage (IEC 61851-23: 20 V).
    pub precharge_tolerance_v: u16,
    /// Pause between a `CurrentDemandRes` and the next request.
    pub charge_loop_interval_ms: u32,
    /// `V2G_EVCC_Sequence_Performance_Time`: 59 s (Table 75); the time the EVCC may spend on the
    /// vehicle (control pilot, contactors, ramping down) before its next request.
    pub sequence_performance_ms: u32,
    /// Welding detection after the energy transfer (optional for the EVCC, V2G-DC-283).
    pub welding_detection: bool,
    /// Welding detection is over when the voltage the EVSE reports has dropped below this
    /// (IEC 61851-23: a disabled side B discharges to below 60 V).
    pub welding_detection_voltage_v: u16,
    /// Send the optional `ChargingProfile` in `PowerDeliveryReq` (a profile that follows the
    /// `PMaxSchedule` of the charge parameter discovery).
    pub send_charging_profile: bool,
    /// Send `ServiceCategory` `EVCharging` in `ServiceDiscoveryReq` (optional, V2G-DC-557).
    pub service_category: bool,
    /// Keep the limits of the charge loop within those of the charge parameter discovery
    /// (V2G-DC-641): maximums no higher. Off, the vehicle sends whatever
    /// [`crate::ev::DcEv::dc_limits`] says, for tests of a charger.
    pub enforce_cpd_limits: bool,
}

impl Config {
    /// DIN SPEC 70121, version 2.0, schema id 1, top priority.
    pub const DIN: Config = Config {
        offer: AppProtocol {
            protocol_namespace: "urn:din:70121:2012:MsgDef",
            version_number_major: 2,
            version_number_minor: 0,
            schema_id: 1,
            priority: 1,
        },
        response_timeout_ms: 2000,
        current_demand_timeout_ms: 250,
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
        service_category: false,
        enforce_cpd_limits: true,
    };
}
