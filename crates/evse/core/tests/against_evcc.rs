//! The charger machine against the vehicle machine of `evcc-core`, in memory: two implementations of
//! the same standard written separately, meeting over byte buffers (no sockets, no timers).

use evcc_core::ev::{Iso20Vehicle, DcConnector, DcLimits, AcEv, AcPlugEv, DcEv, DcPlugEv, Ev, InvertedPantographEv, ScheduleRequest};
use evcc_core::log::NoLog;
use evcc_core::session::{Buffers as EvBuffers, Config, EndReason, Event as EvEvent, Notice, Session as EvSession};
use evse_core::secc::{AcLoopAnswer, AcdpPositioning, AcdpPositioningRequest, DcLoopAnswer, Progress, Secc, DcSecc, AcSecc, InvertedPantographSecc};
use evse_core::{Buffers, Event, Session, State};
use iso15118_20::ac;
use iso15118_20::common::{Authorization, Parameter, ParameterChoice, ParameterSet, PowerSchedule, PowerScheduleEntry, PowerScheduleEntryList, ScheduleTuple, ChargingSchedule, Service};
use iso15118_20::common_types::{EvseNotification, EvseStatus, MeterInfo, RationalNumber, ResponseCode};
use iso15118_20::dc;

fn rat(exponent: i8, value: i16) -> evcc_core::Rational {
    evcc_core::Rational { exponent, value }
}

fn rn(exponent: i8, value: i16) -> RationalNumber {
    RationalNumber { exponent, value }
}

struct TestEv {
    clock: std::cell::Cell<u64>,
    /// Offer energy in Scheduled mode.
    offer: bool,
    /// Prefer cycling between 3 kWh below and 5 kWh above the present energy (a range that needs the clamp: the vehicle's
    /// minimum energy request is 0 and the maximum 60 kWh).
    v2x: bool,
}

impl Ev for TestEv {
    fn evcc_id(&self) -> &str {
        "WMIV1234567890ABCDEX"
    }
    fn now_micros(&self) -> u64 {
        self.clock.set(self.clock.get() + 1);
        self.clock.get()
    }
    fn schedule_request(&self) -> ScheduleRequest {
        ScheduleRequest { departure_time: 3600, minimum_soc: None, target_soc: Some(80), target_energy: rat(3, 40), maximum_energy: rat(3, 60), minimum_energy: rat(0, 0) }
    }
    fn v2x_energy_range(&self) -> Option<evcc_core::ev::V2xEnergyRange> {
        self.v2x.then(|| evcc_core::ev::V2xEnergyRange { maximum: rat(3, 5), minimum: rat(3, -3) })
    }
    fn energy_offer(&self) -> Option<evcc_core::ev::EnergyOffer> {
        self.offer.then(|| evcc_core::ev::EnergyOffer::new("EUR").power(1800, rat(3, 3)).power(1800, rat(3, 1)).price(3600, rat(-2, 30)))
    }
}

impl DcEv for TestEv {
    fn dc_limits(&self) -> DcLimits {
        DcLimits {
            maximum_charge_power: rat(3, 100),
            minimum_charge_power: rat(0, 100),
            maximum_charge_current: rat(0, 200),
            minimum_charge_current: rat(0, 1),
            maximum_voltage: rat(0, 450),
            minimum_voltage: rat(0, 200),
            target_soc: Some(80),
        }
    }
    fn battery_voltage(&self) -> evcc_core::Rational {
        rat(0, 400)
    }
    fn inlet_voltage(&self) -> evcc_core::Rational {
        rat(0, 400)
    }
}

impl DcPlugEv for TestEv {
    fn dc_connector(&self) -> DcConnector {
        DcConnector::Core
    }
}

impl AcEv for TestEv {}

impl AcPlugEv for TestEv {}

impl InvertedPantographEv for TestEv {
    fn acdp_positioning_support(&self) -> bool {
        true
    }
    fn acdp_technical_status(&self) -> iso15118_20::acdp::EvTechnicalStatus {
        // Not through `immobilized()`, which counts the rounds of the driver.
        iso15118_20::acdp::EvTechnicalStatus {
            ev_ready_to_charge: true,
            ev_immobilization_request: false,
            ev_immobilized: None,
            evwlan_strength: None,
            evcp_status: None,
            evsoc: None,
            ev_error_code: None,
            ev_timeout: None,
        }
    }
    fn immobilized(&self) -> bool {
        // The driver needs a few rounds to park.
        IMMOBILIZED_AFTER.with(|c| {
            let left = c.get();
            c.set(left.saturating_sub(1));
            left <= 1
        })
    }
}

static DC_PARAMS: [Parameter<'static>; 4] = [
    Parameter { name: "Connector", choice: ParameterChoice::IntValue(1) },
    Parameter { name: "ControlMode", choice: ParameterChoice::IntValue(2) },
    Parameter { name: "MobilityNeedsMode", choice: ParameterChoice::IntValue(1) },
    Parameter { name: "Pricing", choice: ParameterChoice::IntValue(0) },
];
static AC_PARAMS: [Parameter<'static>; 5] = [
    Parameter { name: "Connector", choice: ParameterChoice::IntValue(2) },
    Parameter { name: "EVSENominalVoltage", choice: ParameterChoice::IntValue(400) },
    Parameter { name: "ControlMode", choice: ParameterChoice::IntValue(2) },
    Parameter { name: "MobilityNeedsMode", choice: ParameterChoice::IntValue(1) },
    Parameter { name: "Pricing", choice: ParameterChoice::IntValue(0) },
];
static AC_SCHEDULED_PARAMS: [Parameter<'static>; 5] = [
    Parameter { name: "Connector", choice: ParameterChoice::IntValue(2) },
    Parameter { name: "EVSENominalVoltage", choice: ParameterChoice::IntValue(400) },
    Parameter { name: "ControlMode", choice: ParameterChoice::IntValue(1) },
    Parameter { name: "MobilityNeedsMode", choice: ParameterChoice::IntValue(1) },
    Parameter { name: "Pricing", choice: ParameterChoice::IntValue(0) },
];
static DC_BY_SECC_PARAMS: [Parameter<'static>; 4] = [
    Parameter { name: "Connector", choice: ParameterChoice::IntValue(1) },
    Parameter { name: "ControlMode", choice: ParameterChoice::IntValue(2) },
    Parameter { name: "MobilityNeedsMode", choice: ParameterChoice::IntValue(2) },
    Parameter { name: "Pricing", choice: ParameterChoice::IntValue(0) },
];
static DC_SETS: [ParameterSet<'static>; 2] = [ParameterSet { parameter_set_id: 1, parameter: &DC_PARAMS }, ParameterSet { parameter_set_id: 2, parameter: &DC_BY_SECC_PARAMS }];
static AC_SETS: [ParameterSet<'static>; 2] = [ParameterSet { parameter_set_id: 1, parameter: &AC_PARAMS }, ParameterSet { parameter_set_id: 2, parameter: &AC_SCHEDULED_PARAMS }];
// ACDP (Tables 210, 211): no `Connector`; the BPT service has the BPT parameters.
static ACDP_PARAMS: [Parameter<'static>; 3] = [
    Parameter { name: "ControlMode", choice: ParameterChoice::IntValue(2) },
    Parameter { name: "MobilityNeedsMode", choice: ParameterChoice::IntValue(1) },
    Parameter { name: "Pricing", choice: ParameterChoice::IntValue(0) },
];
static ACDP_BPT_PARAMS: [Parameter<'static>; 5] = [
    Parameter { name: "ControlMode", choice: ParameterChoice::IntValue(2) },
    Parameter { name: "MobilityNeedsMode", choice: ParameterChoice::IntValue(1) },
    Parameter { name: "BPTChannel", choice: ParameterChoice::IntValue(1) },
    Parameter { name: "GeneratorMode", choice: ParameterChoice::IntValue(1) },
    Parameter { name: "Pricing", choice: ParameterChoice::IntValue(0) },
];
static ACDP_SETS: [ParameterSet<'static>; 1] = [ParameterSet { parameter_set_id: 1, parameter: &ACDP_PARAMS }];
static ACDP_BPT_SETS: [ParameterSet<'static>; 1] = [ParameterSet { parameter_set_id: 1, parameter: &ACDP_BPT_PARAMS }];
static SERVICES: [Service; 6] = [
    Service { service_id: 1, free_service: false },
    Service { service_id: 2, free_service: false },
    Service { service_id: 4, free_service: false },
    Service { service_id: 5, free_service: false },
    Service { service_id: 6, free_service: false },
    Service { service_id: 7, free_service: false },
];
static SCHEDULE_ENTRIES: [PowerScheduleEntry; 1] = [PowerScheduleEntry { duration: 3600, power: RationalNumber { exponent: 3, value: 11 }, power_l2: None, power_l3: None }];
static TUPLES: [ScheduleTuple<'static>; 1] = [ScheduleTuple {
    schedule_tuple_id: 1,
    charging_schedule: ChargingSchedule {
        power_schedule: PowerSchedule { time_anchor: 0, available_energy: None, power_tolerance: None, power_schedule_entries: PowerScheduleEntryList { power_schedule_entry: &SCHEDULE_ENTRIES } },
        schedule: None,
    },
    discharging_schedule: None,
}];

/// A charger with fixed limits that delivers what the vehicle asks for.
#[derive(Default)]
struct TestCharger {
    loops: u32,
    cable_checks: u32,
    delivering: bool,
    confirmations: u32,
    ended: Option<evse_core::secc::Ended>,
    /// The charger has no power: it says `GoToPause`.
    no_power: bool,
    meter_id: String,
    /// The parameter set with MobilityNeedsMode 2 was selected.
    by_secc: bool,
    /// What the charger saw of the vehicle: the energy offer (entries, price entries, the first fee) and the
    /// display parameters of the last charge loop request.
    offer: Option<(usize, usize, RationalNumber)>,
    display: Option<iso15118_20::common_types::DisplayParameters>,
    /// The preferred BPT range (maximum, minimum) of the schedule exchange and of the last charge loop request.
    v2x_schedule: Option<(RationalNumber, RationalNumber)>,
    v2x_loop: Option<(RationalNumber, RationalNumber)>,
    /// ACDP: the positioning requests the charger saw, the charger's PPD losing the vehicle at the n-th, a charger without ACDP.
    positioning: Vec<AcdpPositioningRequest>,
    /// ACDP: the connect and disconnect requests the charger saw, with the pilot state the vehicle reported.
    pantograph: Vec<(&'static str, iso15118_20::acdp::ElectricalChargingDeviceStatus)>,
    /// ACDP: how many status requests the charger answered.
    status_requests: u32,
    lose_association_at: Option<usize>,
    no_acdp: bool,
}

thread_local! {
    /// ACDP: the answers to the status requests the vehicle's host was told of.
    static STATUS_NOTICES: std::cell::Cell<u32> = const { std::cell::Cell::new(0) };
}

thread_local! {
    /// ACDP: the vehicle is immobilized when `immobilized()` is called this many times.
    static IMMOBILIZED_AFTER: std::cell::Cell<u32> = const { std::cell::Cell::new(1) };
}

impl Secc for TestCharger {
    fn evse_id(&self) -> &str {
        "DE*TST*E1"
    }
    fn supports_protocol(&self, namespace: &str, major: u32) -> bool {
        major == 1 && (matches!(namespace, "urn:iso:std:iso:15118:-20:DC" | "urn:iso:std:iso:15118:-20:AC") || (!self.no_acdp && namespace == "urn:iso:std:iso:15118:-20:ACDP"))
    }
    fn now_micros(&self) -> u64 {
        1_700_000_000_000_000
    }
    fn new_session_id(&mut self) -> [u8; 8] {
        [1, 2, 3, 4, 5, 6, 7, 8]
    }
    fn gen_challenge(&mut self) -> [u8; 16] {
        [9; 16]
    }
    fn authorization_services(&self) -> &[Authorization] {
        &[Authorization::Eim, Authorization::PnC]
    }
    fn energy_services(&self) -> &[Service] {
        &SERVICES
    }
    fn parameter_sets(&self, service_id: u16) -> &[ParameterSet<'_>] {
        match service_id {
            1 | 5 => &AC_SETS,
            2 | 6 => &DC_SETS,
            4 => &ACDP_SETS,
            7 => &ACDP_BPT_SETS,
            _ => &[],
        }
    }
    fn authorize(&mut self, request: evse_core::secc::Authorize<'_, '_>) -> Progress<ResponseCode> {
        // A real charger verifies the chain and the signature here (see `evse-sim`'s `pnc`).
        if let evse_core::secc::Authorize::PnC { header, fragment, challenge, mode } = request {
            assert!(header.signature.is_some() && !fragment.is_empty());
            assert_eq!(mode.gen_challenge, challenge);
        }
        Progress::Done(ResponseCode::Ok)
    }
    fn service_selected(&mut self, selection: &iso15118_20::common::ServiceSelectionReq<'_>) {
        self.by_secc = selection.selected_energy_transfer_service.service_id == 2 && selection.selected_energy_transfer_service.parameter_set_id == 2;
    }
    fn schedule_requested(&mut self, request: &iso15118_20::common::ScheduleExchangeReq<'_>) -> evse_core::MobilityNeeds {
        if let iso15118_20::common::SeReqControlModeChoice::Dynamic(m) = &request.se_req_control_mode {
            self.v2x_schedule = m.ev_maximum_v2x_energy_request.zip(m.ev_minimum_v2x_energy_request);
        }
        if let iso15118_20::common::SeReqControlModeChoice::Scheduled(m) = &request.se_req_control_mode {
            self.offer = m.ev_energy_offer.as_ref().map(|o| {
                let stacks = o.ev_absolute_price_schedule.ev_price_rule_stacks.ev_price_rule_stack;
                (o.ev_power_schedule.ev_power_schedule_entries.ev_power_schedule_entry.len(), stacks.len(), stacks[0].ev_price_rule[0].energy_fee)
            });
        }
        if self.by_secc {
            evse_core::MobilityNeeds { departure_time: Some(7200), minimum_soc: Some(20), target_soc: Some(90) }
        } else {
            evse_core::MobilityNeeds::default()
        }
    }
    fn schedule_tuples(&self) -> &[ScheduleTuple<'_>] {
        &TUPLES
    }
    fn no_power(&self) -> bool {
        self.no_power
    }
    fn power_delivery(&mut self, request: &iso15118_20::common::PowerDeliveryReq<'_>) -> ResponseCode {
        self.delivering = request.charge_progress == iso15118_20::common::ChargeProgress::Start;
        ResponseCode::Ok
    }
    fn meter_info(&self, wanted: bool) -> Option<MeterInfo<'_>> {
        wanted.then(|| MeterInfo {
            meter_id: &self.meter_id,
            charged_energy_reading_wh: 1234 + u64::from(self.loops),
            bpt_discharged_energy_reading_wh: None,
            capacitive_energy_reading_va_rh: None,
            bpt_inductive_energy_reading_va_rh: None,
            meter_signature: None,
            meter_status: None,
            meter_timestamp: None,
        })
    }
    fn confirm_metering(&mut self, _request: &iso15118_20::common::MeteringConfirmationReq<'_>, signed: evse_core::secc::Signed<'_, '_>) -> ResponseCode {
        // The signature is there and covers a stream.
        assert!(signed.header.signature.is_some() && !signed.fragment.is_empty());
        self.confirmations += 1;
        ResponseCode::Ok
    }
    fn note(&mut self, text: core::fmt::Arguments<'_>) {
        if std::env::var_os("EVSE_TRACE").is_some() {
            eprintln!("[evse] {text}");
        }
    }
    fn session_ended(&mut self, how: evse_core::secc::Ended) {
        self.ended = Some(how);
    }
}

impl DcSecc for TestCharger {
    fn dc_charge_parameters(&mut self, bpt: bool, _request: &dc::DcChargeParameterDiscoveryReq<'_>) -> Result<dc::DcCpdResEnergyTransferModeChoice, ResponseCode> {
        let base = dc::DcCpdResEnergyTransferMode {
            evse_maximum_charge_power: rn(3, 150),
            evse_minimum_charge_power: rn(0, 1000),
            evse_maximum_charge_current: rn(0, 300),
            evse_minimum_charge_current: rn(0, 2),
            evse_maximum_voltage: rn(0, 920),
            evse_minimum_voltage: rn(0, 150),
            evse_power_ramp_limitation: None,
        };
        Ok(if bpt {
            dc::DcCpdResEnergyTransferModeChoice::Bpt(dc::BptDcCpdResEnergyTransferMode {
                evse_maximum_charge_power: base.evse_maximum_charge_power,
                evse_minimum_charge_power: base.evse_minimum_charge_power,
                evse_maximum_charge_current: base.evse_maximum_charge_current,
                evse_minimum_charge_current: base.evse_minimum_charge_current,
                evse_maximum_voltage: base.evse_maximum_voltage,
                evse_minimum_voltage: base.evse_minimum_voltage,
                evse_power_ramp_limitation: None,
                evse_maximum_discharge_power: rn(3, 11),
                evse_minimum_discharge_power: rn(0, 100),
                evse_maximum_discharge_current: rn(0, 30),
                evse_minimum_discharge_current: rn(0, 1),
            })
        } else {
            dc::DcCpdResEnergyTransferModeChoice::Base(base)
        })
    }
    fn cable_check(&mut self) -> Progress<ResponseCode> {
        self.cable_checks += 1;
        // One round of Ongoing, to see the polling work.
        if self.cable_checks < 2 { Progress::Ongoing } else { Progress::Done(ResponseCode::Ok) }
    }
    fn dc_loop(&mut self, request: &dc::DcChargeLoopReq<'_>, bpt: bool) -> Option<DcLoopAnswer> {
        self.loops += 1;
        self.display = request.display_parameters;
        if let dc::ClReqControlModeChoice::BptDynamicDc(m) = &request.cl_req_control_mode {
            self.v2x_loop = m.ev_maximum_v2x_energy_request.zip(m.ev_minimum_v2x_energy_request);
        }
        let _ = bpt;
        let (maximum_current, mode) = match &request.cl_req_control_mode {
            dc::ClReqControlModeChoice::DynamicDc(m) => (m.ev_maximum_charge_current, dc::ClResControlModeChoice::DynamicDc(dc::DynamicDcClResControlMode {
                departure_time: None,
                minimum_soc: None,
                target_soc: None,
                ack_max_delay: None,
                evse_maximum_charge_power: rn(3, 150),
                evse_minimum_charge_power: rn(0, 1000),
                evse_maximum_charge_current: rn(0, 300),
                evse_maximum_voltage: rn(0, 920),
            })),
            dc::ClReqControlModeChoice::BptDynamicDc(m) => (m.ev_maximum_charge_current, dc::ClResControlModeChoice::BptDynamicDc(dc::BptDynamicDcClResControlMode {
                departure_time: None,
                minimum_soc: None,
                target_soc: None,
                ack_max_delay: None,
                evse_maximum_charge_power: rn(3, 150),
                evse_minimum_charge_power: rn(0, 1000),
                evse_maximum_charge_current: rn(0, 300),
                evse_maximum_voltage: rn(0, 920),
                evse_maximum_discharge_power: rn(3, 11),
                evse_minimum_discharge_power: rn(0, 100),
                evse_maximum_discharge_current: rn(0, 30),
                evse_minimum_voltage: rn(0, 150),
            })),
            _ => return None,
        };
        Some(DcLoopAnswer {
            response_code: ResponseCode::Ok,
            present_current: maximum_current,
            present_voltage: request.ev_present_voltage,
            power_limit_achieved: false,
            current_limit_achieved: true,
            voltage_limit_achieved: false,
            control_mode: mode,
            status: (self.loops == 2).then_some(EvseStatus { notification_max_delay: 10, evse_notification: EvseNotification::MeteringConfirmation }),
        })
    }
}

impl AcSecc for TestCharger {
    fn ac_charge_parameters(&mut self, bpt: bool, _request: &ac::AcChargeParameterDiscoveryReq<'_>) -> Result<ac::AcCpdResEnergyTransferModeChoice, ResponseCode> {
        let base = ac::AcCpdResEnergyTransferMode {
            evse_maximum_charge_power: rn(3, 4),
            evse_maximum_charge_power_l2: Some(rn(3, 4)),
            evse_maximum_charge_power_l3: Some(rn(3, 4)),
            evse_minimum_charge_power: rn(0, 1380),
            evse_minimum_charge_power_l2: Some(rn(0, 1380)),
            evse_minimum_charge_power_l3: Some(rn(0, 1380)),
            evse_nominal_frequency: rn(0, 50),
            maximum_power_asymmetry: None,
            evse_power_ramp_limitation: None,
            evse_present_active_power: None,
            evse_present_active_power_l2: None,
            evse_present_active_power_l3: None,
        };
        Ok(if bpt {
            ac::AcCpdResEnergyTransferModeChoice::Bpt(ac::BptAcCpdResEnergyTransferMode {
                evse_maximum_charge_power: base.evse_maximum_charge_power,
                evse_maximum_charge_power_l2: base.evse_maximum_charge_power_l2,
                evse_maximum_charge_power_l3: base.evse_maximum_charge_power_l3,
                evse_minimum_charge_power: base.evse_minimum_charge_power,
                evse_minimum_charge_power_l2: base.evse_minimum_charge_power_l2,
                evse_minimum_charge_power_l3: base.evse_minimum_charge_power_l3,
                evse_nominal_frequency: base.evse_nominal_frequency,
                maximum_power_asymmetry: None,
                evse_power_ramp_limitation: None,
                evse_present_active_power: None,
                evse_present_active_power_l2: None,
                evse_present_active_power_l3: None,
                evse_maximum_discharge_power: rn(3, 4),
                evse_maximum_discharge_power_l2: Some(rn(3, 4)),
                evse_maximum_discharge_power_l3: Some(rn(3, 4)),
                evse_minimum_discharge_power: rn(0, 500),
                evse_minimum_discharge_power_l2: Some(rn(0, 500)),
                evse_minimum_discharge_power_l3: Some(rn(0, 500)),
            })
        } else {
            ac::AcCpdResEnergyTransferModeChoice::Base(base)
        })
    }
    fn ac_loop(&mut self, request: &ac::AcChargeLoopReq<'_>, _bpt: bool) -> Option<AcLoopAnswer> {
        self.loops += 1;
        let power = rn(3, 4);
        let mode = match &request.cl_req_control_mode {
            ac::ClReqControlModeChoice::DynamicAc(_) => ac::ClResControlModeChoice::DynamicAc(ac::DynamicAcClResControlMode {
                departure_time: None,
                minimum_soc: None,
                target_soc: None,
                ack_max_delay: None,
                evse_target_active_power: power,
                evse_target_active_power_l2: Some(power),
                evse_target_active_power_l3: Some(power),
                evse_target_reactive_power: None,
                evse_target_reactive_power_l2: None,
                evse_target_reactive_power_l3: None,
                evse_present_active_power: None,
                evse_present_active_power_l2: None,
                evse_present_active_power_l3: None,
            }),
            ac::ClReqControlModeChoice::ScheduledAc(_) => ac::ClResControlModeChoice::ScheduledAc(ac::ScheduledAcClResControlMode {
                evse_target_active_power: None,
                evse_target_active_power_l2: None,
                evse_target_active_power_l3: None,
                evse_target_reactive_power: None,
                evse_target_reactive_power_l2: None,
                evse_target_reactive_power_l3: None,
                evse_present_active_power: Some(power),
                evse_present_active_power_l2: None,
                evse_present_active_power_l3: None,
            }),
            _ => return None,
        };
        Some(AcLoopAnswer { response_code: ResponseCode::Ok, target_frequency: None, control_mode: mode, status: None })
    }
}

impl InvertedPantographSecc for TestCharger {
    fn acdp_system_status(&mut self, _vehicle: &iso15118_20::acdp::EvTechnicalStatus) -> evse_core::secc::AcdpSystemStatus {
        self.status_requests += 1;
        evse_core::secc::AcdpSystemStatus::HEALTHY
    }
    fn acdp_connect(&mut self, electrical: iso15118_20::acdp::ElectricalChargingDeviceStatus) -> evse_core::secc::AcdpConnection {
        use iso15118_20::acdp::{ElectricalChargingDeviceStatus as E, MechanicalChargingDeviceStatus as M};
        self.pantograph.push(("connect", electrical));
        // The first request finds the pantograph moving, the second down.
        let down = self.pantograph.iter().filter(|(k, _)| *k == "connect").count() >= 2;
        evse_core::secc::AcdpConnection { finished: down, electrical: if down { E::StateB } else { E::StateA }, mechanical: if down { M::EndPosition } else { M::Moving } }
    }
    fn acdp_disconnect(&mut self, electrical: iso15118_20::acdp::ElectricalChargingDeviceStatus) -> evse_core::secc::AcdpConnection {
        use iso15118_20::acdp::{ElectricalChargingDeviceStatus as E, MechanicalChargingDeviceStatus as M};
        self.pantograph.push(("disconnect", electrical));
        let home = self.pantograph.iter().filter(|(k, _)| *k == "disconnect").count() >= 2;
        evse_core::secc::AcdpConnection { finished: home, electrical: E::StateA, mechanical: if home { M::Home } else { M::Moving } }
    }
    fn acdp_positioning(&mut self, request: AcdpPositioningRequest) -> AcdpPositioning {
        self.positioning.push(request);
        let n = self.positioning.len();
        AcdpPositioning {
            association: self.lose_association_at != Some(n),
            positioning_support: request.positioning_support,
            // The bus creeps towards the pantograph.
            x_deviation_mm: 400 - 100 * n as i16,
            y_deviation_mm: 30,
            window_x_mm: 150,
            window_y_mm: 100,
            in_charge_position: n >= 3,
        }
    }
}

/// A vehicle's contract that signs with nothing: the charger under test accepts anything.
struct FakeContract;

impl evcc_core::ev::Contract for FakeContract {
    fn certificate(&self) -> &[u8] {
        &[1, 2, 3]
    }
    fn sub_certificate(&self, index: usize) -> Option<&[u8]> {
        // The chain has at least one sub-CA certificate (the list has a minimum of one).
        (index == 0).then_some(&[4, 5, 6][..])
    }
    fn signature_algorithm(&self) -> &str {
        iso15118_20::signature::ECDSA_SHA512
    }
    fn digest_algorithm(&self) -> &str {
        iso15118_20::signature::DIGEST_SHA512
    }
    fn digest(&self, _data: &[u8], out: &mut [u8]) -> Option<usize> {
        out[..64].fill(7);
        Some(64)
    }
    fn sign(&self, _data: &[u8], out: &mut [u8]) -> Option<usize> {
        out[..132].fill(5);
        Some(132)
    }
}

struct PnCEv(TestEv, FakeContract);

impl Ev for PnCEv {
    fn evcc_id(&self) -> &str {
        self.0.evcc_id()
    }
    fn now_micros(&self) -> u64 {
        self.0.now_micros()
    }
    fn schedule_request(&self) -> ScheduleRequest {
        self.0.schedule_request()
    }
    fn contract(&self) -> Option<&dyn evcc_core::ev::Contract> {
        Some(&self.1)
    }
}

impl DcEv for PnCEv {
    fn dc_limits(&self) -> DcLimits {
        self.0.dc_limits()
    }
    fn battery_voltage(&self) -> evcc_core::Rational {
        self.0.battery_voltage()
    }
    fn inlet_voltage(&self) -> evcc_core::Rational {
        self.0.inlet_voltage()
    }
}

impl DcPlugEv for PnCEv {
    fn dc_connector(&self) -> DcConnector {
        self.0.dc_connector()
    }
}

impl AcEv for PnCEv {}

impl AcPlugEv for PnCEv {}

impl InvertedPantographEv for PnCEv {}

thread_local! {
    /// The control pilot guards the machines published, in order: the vehicle's and the charger's.
    static VEHICLE_GUARDS: std::cell::RefCell<Vec<control_pilot::Guard>> = const { std::cell::RefCell::new(Vec::new()) };
    static CHARGER_GUARDS: std::cell::RefCell<Vec<control_pilot::Guard>> = const { std::cell::RefCell::new(Vec::new()) };
}

/// The two machines and what passes between them.
struct Rig<'a, E: Iso20Vehicle> {
    vehicle: EvSession<'a, E, NoLog>,
    charger: Session<TestCharger>,
    charger_buffers: Buffers<'a>,
    ended: Option<EndReason>,
    loops: u32,
    stop_after: u32,
    closed: bool,
    /// The vehicle's next request is held back when the charger is in this state.
    halt_at: Option<State>,
    halted: bool,
    host: PilotHost,
}

/// What the host tells the charger about the control pilot.
#[derive(Clone, Copy, Default)]
struct PilotHost {
    /// Report the pilot the vehicle was told to set (C before the cable check, B after the contactors opened).
    report: bool,
    /// ... but not the change to C / to B (the charger still sees the old state).
    skip_c: bool,
    skip_b: bool,
    /// A reading of its own once the n-th charge loop answer is out.
    inject: Option<(u32, control_pilot::Reading)>,
}

impl<E: Iso20Vehicle> Rig<'_, E> {
    /// Carries out what a vehicle step asks for: frames to the charger, its answers back, the
    /// notices the host answers.
    fn process(&mut self, step: evcc_core::session::Step) {
        if let Some(end) = step.end {
            self.ended = Some(end);
        }
        if let Some(guard) = step.pilot {
            VEHICLE_GUARDS.with(|g| g.borrow_mut().push(guard));
        }
        if let Some(n) = step.notice {
            let event = match n {
                Notice::CloseContactors => Some(EvEvent::ContactorsClosed),
                Notice::StopEnergyTransfer { .. } => Some(EvEvent::EnergyTransferStopped),
                Notice::OpenContactors => Some(EvEvent::ContactorsOpened),
                Notice::ControlPilot(p) => {
                    let (state, skip) = match p {
                        evcc_core::session::ControlPilot::B => (control_pilot::State::B, self.host.skip_b),
                        evcc_core::session::ControlPilot::C => (control_pilot::State::C, self.host.skip_c),
                    };
                    let reading = control_pilot::Reading { state, pwm: control_pilot::Pwm::Duty(5) };
                    if self.host.report && !skip {
                        let step = self.charger.handle(Event::Pilot(reading), &mut self.charger_buffers);
                        if let Some(guard) = step.pilot {
                            CHARGER_GUARDS.with(|g| g.borrow_mut().push(guard));
                        }
                        self.closed |= step.close;
                    }
                    // The vehicle measures what it set; that is what it waits for.
                    Some(EvEvent::Pilot(reading))
                }
                Notice::AcdpSystemStatus(_) => {
                    STATUS_NOTICES.with(|c| c.set(c.get() + 1));
                    None
                }
                Notice::ChargeLoop(_) | Notice::AcChargeLoop(_) => {
                    self.loops += 1;
                    // Time passes in the charge loop: the status timer of an ACDP vehicle (nothing for the others).
                    let tick = self.vehicle.handle(EvEvent::TimerExpired(evcc_core::Timer::Status));
                    self.process(tick);
                    if let Some((n, reading)) = self.host.inject.filter(|(n, _)| *n == self.loops) {
                        let _ = n;
                        let step = self.charger.handle(Event::Pilot(reading), &mut self.charger_buffers);
                        if let Some(guard) = step.pilot {
                            CHARGER_GUARDS.with(|g| g.borrow_mut().push(guard));
                        }
                        self.closed |= step.close;
                    }
                    (self.loops == self.stop_after).then_some(EvEvent::StopRequested)
                }
                _ => None,
            };
            if let Some(e) = event {
                let s = self.vehicle.handle(e);
                self.process(s);
            }
        }
        if let Some(n) = step.send {
            if self.halt_at == Some(self.charger.state()) {
                self.halted = true;
                return;
            }
            let frames = self.vehicle.tx()[..n].to_vec();
            let space = self.charger.rx_space(self.charger_buffers.rx);
            space[..frames.len()].copy_from_slice(&frames);
            let reply = self.charger.handle(Event::Received(frames.len()), &mut self.charger_buffers);
            if let Some(guard) = reply.pilot {
                CHARGER_GUARDS.with(|g| g.borrow_mut().push(guard));
            }
            let bytes = self.charger_buffers.tx[..reply.send].to_vec();
            // One frame at a time, as a stream would deliver them.
            let mut at = 0;
            while at < bytes.len() {
                let len = 8 + u32::from_be_bytes(bytes[at + 4..at + 8].try_into().unwrap()) as usize;
                self.vehicle.rx_space()[..len].copy_from_slice(&bytes[at..at + len]);
                let s = self.vehicle.handle(EvEvent::Received(len));
                self.process(s);
                at += len;
            }
            self.closed |= reply.close;
            // A connection the charger closed is known to the vehicle's host (the hang-up): a session that had announced its end
            // with a SessionStopReq ends with that reason.
            if reply.close {
                let s = self.vehicle.handle(EvEvent::ConnectionClosed);
                self.process(s);
            }
        }
    }
}

/// Runs the vehicle against the charger until one of them is done; `stop_after` charge loop answers.
fn run<E: Iso20Vehicle>(config: Config, ev: E, stop_after: u32) -> (EndReason, TestCharger, State) {
    run_with(TestCharger::default(), config, ev, stop_after)
}

fn run_with<E: Iso20Vehicle>(secc: TestCharger, config: Config, ev: E, stop_after: u32) -> (EndReason, TestCharger, State) {
    let (mut rx, mut tx, mut arena) = (vec![0u8; 8192], vec![0u8; 8192], vec![0u8; 32768]);
    let vehicle = EvSession::new(config, ev, NoLog, EvBuffers { rx: &mut rx, tx: &mut tx, arena: &mut arena });
    let (mut srx, mut stx, mut sarena, mut sscratch) = (vec![0u8; 8192], vec![0u8; 8192], vec![0u8; 32768], vec![0u8; 8192]);
    let mut charger_buffers = Buffers { rx: &mut srx, tx: &mut stx, arena: &mut sarena, scratch: &mut sscratch };
    let mut charger = Session::new(secc);
    charger.handle(Event::Connected { secured: true }, &mut charger_buffers);
    let mut rig = Rig { vehicle, charger, charger_buffers, ended: None, loops: 0, stop_after, closed: false, halt_at: None, halted: false, host: PilotHost::default() };
    let start = rig.vehicle.handle(EvEvent::Start { secured: true });
    rig.process(start);
    for _ in 0..200 {
        if rig.ended.is_some() || rig.closed {
            break;
        }
        // Nothing in flight: the vehicle waits for its poll pause or loop interval.
        let s = rig.vehicle.handle(EvEvent::TimerExpired(evcc_core::Timer::Poll));
        rig.process(s);
        // The status timer of an ACDP vehicle (nothing for the others).
        let s = rig.vehicle.handle(EvEvent::TimerExpired(evcc_core::Timer::Status));
        rig.process(s);
    }
    let state = rig.charger.state();
    let ended = rig.ended.expect("the vehicle ended its session");
    (ended, std::mem::take(rig.charger.secc_mut()), state)
}

fn config(base: Config) -> Config {
    Config { poll_interval_ms: 0, charge_loop_interval_ms: 1, ..base }
}

#[test]
fn a_dc_session_runs_to_the_stop() {
    let (end, charger, state) = run(config(Config::DC_20), TestEv { clock: 1.into(), offer: false, v2x: false }, 3);
    assert_eq!(end, EndReason::Completed);
    assert_eq!(state, State::Stopped);
    assert_eq!(charger.loops, 3);
    assert_eq!(charger.cable_checks, 2, "the cable check was polled once");
    assert!(!charger.delivering);
}

#[test]
fn a_bidirectional_dc_session() {
    let (end, charger, state) = run(config(Config::DC_BPT_20), TestEv { clock: 1.into(), offer: false, v2x: false }, 2);
    assert_eq!((end, state), (EndReason::Completed, State::Stopped));
    assert_eq!(charger.loops, 2);
}

#[test]
fn an_ac_session_skips_the_dc_steps() {
    let (end, charger, state) = run(config(Config::AC_20), TestEv { clock: 1.into(), offer: false, v2x: false }, 3);
    assert_eq!((end, state), (EndReason::Completed, State::Stopped));
    assert_eq!((charger.loops, charger.cable_checks), (3, 0));
}

#[test]
fn a_scheduled_ac_session_selects_the_offered_tuple() {
    let (end, charger, state) = run(config(Config::AC_20.scheduled()), TestEv { clock: 1.into(), offer: false, v2x: false }, 2);
    assert_eq!((end, state), (EndReason::Completed, State::Stopped));
    assert_eq!(charger.loops, 2);
}

#[test]
fn a_plug_and_charge_session_confirms_the_metering() {
    let cfg = Config { prefer_pnc: true, request_meter_info: true, ..config(Config::DC_20) };
    let (end, charger, state) = run(cfg, PnCEv(TestEv { clock: 1.into(), offer: false, v2x: false }, FakeContract), 4);
    assert_eq!((end, state), (EndReason::Completed, State::Stopped));
    // The notification came with the second answer; the vehicle signed and the charger took it.
    assert_eq!(charger.confirmations, 1);
}

#[test]
fn the_vehicle_offers_energy_in_the_scheduled_mode() {
    let (end, charger, state) = run(config(Config::AC_20.scheduled()), TestEv { clock: 1.into(), offer: true, v2x: false }, 2);
    assert_eq!((end, state), (EndReason::Completed, State::Stopped));
    // Two power entries, one price: 0.30.
    assert_eq!(charger.offer, Some((2, 1, rn(-2, 30))));
}

#[test]
fn without_an_offer_the_vehicle_sends_none() {
    let (_, charger, _) = run(config(Config::AC_20.scheduled()), TestEv { clock: 1.into(), offer: false, v2x: false }, 2);
    assert_eq!(charger.offer, None);
}

#[test]
fn the_charger_provides_the_mobility_needs_and_the_vehicle_takes_them() {
    let (end, charger, state) = run(config(Config::DC_20.mobility_needs_by_secc()), TestEv { clock: 1.into(), offer: false, v2x: false }, 2);
    assert_eq!((end, state), (EndReason::Completed, State::Stopped));
    assert!(charger.by_secc);
    // What the charger named is what the vehicle reports as its limits in the display parameters.
    let display = charger.display.expect("display parameters in mode 2");
    assert_eq!((display.minimum_soc, display.target_soc), (Some(20), Some(90)));
}

#[test]
fn with_mobility_needs_from_the_vehicle_nothing_is_taken_from_the_charger() {
    let (_, charger, _) = run(config(Config::DC_20), TestEv { clock: 1.into(), offer: false, v2x: false }, 2);
    assert!(!charger.by_secc);
    assert_eq!(charger.display, None);
}

#[test]
fn the_preferred_bpt_range_is_sent_in_order_in_a_bpt_session_only() {
    // The range is between the vehicle's minimum (0) and maximum (60 kWh) energy request: the minimum V2X request of -3 kWh is
    // raised to the minimum (V2G20-2682).
    let (end, charger, _) = run(config(Config::DC_BPT_20), TestEv { clock: 1.into(), offer: false, v2x: true }, 2);
    assert_eq!(end, EndReason::Completed);
    assert_eq!(charger.v2x_schedule, Some((rn(3, 5), rn(0, 0))));
    assert_eq!(charger.v2x_loop, Some((rn(3, 5), rn(0, 0))));
    let (_, charger, _) = run(config(Config::DC_20), TestEv { clock: 1.into(), offer: false, v2x: true }, 2);
    assert_eq!((charger.v2x_schedule, charger.v2x_loop), (None, None));
}

#[test]
fn a_charger_without_power_sends_the_vehicle_to_a_pause_before_the_cable_check() {
    // V2G20-2121 to 2123: GoToPause; no cable check, no pre-charge, the power delivery is a stop, the session pauses.
    for base in [Config::DC_20, Config::AC_20] {
        let (end, charger, state) = run_with(TestCharger { no_power: true, ..Default::default() }, config(base), TestEv { clock: 1.into(), offer: false, v2x: false }, 2);
        assert!(matches!(end, EndReason::Paused(_)), "{end:?}");
        assert_eq!(state, State::Paused);
        assert_eq!((charger.cable_checks, charger.loops), (0, 0));
        assert!(!charger.delivering);
    }
    // A vehicle that does not follow it asks for the cable check anyway, which the charger does not expect now.
    let (end, _, _) = run_with(TestCharger { no_power: true, ..Default::default() }, Config { follow_go_to_pause: false, ..config(Config::DC_20) }, TestEv { clock: 1.into(), offer: false, v2x: false }, 2);
    assert!(matches!(end, EndReason::Rejected(_)), "{end:?}");
}

/// Runs the vehicle until the charger is in `state` and holds back its next request; then a `SessionStopReq`
/// of the given kind (with the session's id, or another one) goes to the charger. Returns the charger's answer
/// as response code and whether it closes, the charger's state, and how the session ended for the host.
fn stop_request_at(state: State, kind: iso15118_20::common::ChargingSession, session_id: [u8; 8]) -> (Option<ResponseCode>, bool, State, Option<evse_core::secc::Ended>) {
    use iso15118_20::common_types::MessageHeader;
    let (mut rx, mut tx, mut arena) = (vec![0u8; 8192], vec![0u8; 8192], vec![0u8; 32768]);
    let ev = TestEv { clock: 1.into(), offer: false, v2x: false };
    let vehicle = EvSession::new(config(Config::DC_20), ev, NoLog, EvBuffers { rx: &mut rx, tx: &mut tx, arena: &mut arena });
    let (mut srx, mut stx, mut sarena, mut sscratch) = (vec![0u8; 8192], vec![0u8; 8192], vec![0u8; 32768], vec![0u8; 8192]);
    let mut charger_buffers = Buffers { rx: &mut srx, tx: &mut stx, arena: &mut sarena, scratch: &mut sscratch };
    let mut charger = Session::new(TestCharger::default());
    charger.handle(Event::Connected { secured: true }, &mut charger_buffers);
    let mut rig = Rig { vehicle, charger, charger_buffers, ended: None, loops: 0, stop_after: 1, closed: false, halt_at: Some(state), halted: false, host: PilotHost::default() };
    let start = rig.vehicle.handle(EvEvent::Start { secured: true });
    rig.process(start);
    for _ in 0..200 {
        if rig.halted || rig.ended.is_some() || rig.closed {
            break;
        }
        let s = rig.vehicle.handle(EvEvent::TimerExpired(evcc_core::Timer::Poll));
        rig.process(s);
    }
    assert!(rig.halted, "the charger never reached {state:?}");
    // The request, framed as the vehicle would send it.
    let mut payload = [0u8; 64];
    let n = iso15118_20::common::SessionStopReq { header: MessageHeader::new(session_id, 1), charging_session: kind, ev_termination_code: None, ev_termination_explanation: None }.encode(&mut payload).unwrap();
    let mut frame = vec![0x01, 0xFE, 0x80, 0x02];
    frame.extend_from_slice(&(n as u32).to_be_bytes());
    frame.extend_from_slice(&payload[..n]);
    let space = rig.charger.rx_space(rig.charger_buffers.rx);
    space[..frame.len()].copy_from_slice(&frame);
    let reply = rig.charger.handle(Event::Received(frame.len()), &mut rig.charger_buffers);
    let code = (reply.send > 8).then(|| {
        let arena_mem = &mut [0u8; 4096];
        let arena = exi::Arena::new(arena_mem);
        match iso15118_20::common::decode(&rig.charger_buffers.tx[8..reply.send], &arena).unwrap() {
            iso15118_20::common::Message::SessionStopRes(r) => r.response_code,
            // The failing response of another type (a request out of turn).
            other => panic!("{other:?}"),
        }
    });
    let ended = rig.charger.secc_mut().ended;
    (code, reply.close, rig.charger.state(), ended)
}

#[test]
fn a_session_stop_is_taken_wherever_the_power_is_not_on() {
    use iso15118_20::common::ChargingSession::Terminate;
    // V2G20-2645 and the stopping states (V2G20-920 to 922): every state after the setup but the charge loop.
    for state in [
        State::AwaitAuthorizationSetupReq,
        State::AwaitAuthorizationReq,
        State::AwaitServiceDiscoveryReq,
        State::AwaitServiceSelectionReq,
        State::AwaitChargeParameterDiscoveryReq,
        State::AwaitScheduleExchangeReq,
        State::AwaitCableCheckReq,
        State::AwaitPreChargeReq,
        State::AwaitPowerDeliveryReq,
        State::AwaitWeldingReq,
    ] {
        let (code, close, now, ended) = stop_request_at(state, Terminate, [1, 2, 3, 4, 5, 6, 7, 8]);
        assert_eq!(code, Some(ResponseCode::Ok), "{state:?}");
        assert!(close, "{state:?}");
        assert_eq!(now, State::Stopped, "{state:?}");
        assert_eq!(ended, Some(evse_core::secc::Ended::Stopped(evse_core::common::Stop::Terminate)), "{state:?}");
    }
}

#[test]
fn an_early_session_stop_can_pause_but_not_renegotiate_the_services() {
    use iso15118_20::common::ChargingSession::{Pause, ServiceRenegotiation};
    let (code, close, now, _) = stop_request_at(State::AwaitCableCheckReq, Pause, [1, 2, 3, 4, 5, 6, 7, 8]);
    assert_eq!((code, close, now), (Some(ResponseCode::Ok), true, State::Paused));
    // A renegotiation follows the orderly stop only: elsewhere it is a request out of turn.
    let (code, close, now, ended) = stop_request_at(State::AwaitCableCheckReq, ServiceRenegotiation, [1, 2, 3, 4, 5, 6, 7, 8]);
    assert_eq!((code, close, now, ended), (Some(ResponseCode::FailedSequenceError), true, State::Closed, None));
}

#[test]
fn a_session_stop_with_another_session_id_is_refused_and_during_the_charge_loop_it_is_out_of_turn() {
    use iso15118_20::common::ChargingSession::Terminate;
    let (code, close, now, ended) = stop_request_at(State::AwaitPreChargeReq, Terminate, [9; 8]);
    assert_eq!((code, close, ended), (Some(ResponseCode::FailedUnknownSession), true, None));
    assert_eq!(now, State::Closed);
    // The power is on: the power delivery has to stop first (V2G20-1546 ff.).
    let (code, close, now, ended) = stop_request_at(State::ChargeLoop, Terminate, [1, 2, 3, 4, 5, 6, 7, 8]);
    assert_eq!((code, close, now, ended), (Some(ResponseCode::FailedSequenceError), true, State::Closed, None));
}

/// Runs the vehicle (asking for the pilot) against the charger, with a host that reports the pilot as `host` says.
/// Returns how the vehicle ended (if it did), whether the charger closed the connection, the charger's state and its record.
fn pilot_run(host: PilotHost, stop_after: u32) -> (Option<EndReason>, bool, State, TestCharger) {
    let (mut rx, mut tx, mut arena) = (vec![0u8; 8192], vec![0u8; 8192], vec![0u8; 32768]);
    let ev = TestEv { clock: 1.into(), offer: false, v2x: false };
    let vehicle = EvSession::new(Config { control_pilot: true, ..config(Config::DC_20) }, ev, NoLog, EvBuffers { rx: &mut rx, tx: &mut tx, arena: &mut arena });
    let (mut srx, mut stx, mut sarena, mut sscratch) = (vec![0u8; 8192], vec![0u8; 8192], vec![0u8; 32768], vec![0u8; 8192]);
    let mut charger_buffers = Buffers { rx: &mut srx, tx: &mut stx, arena: &mut sarena, scratch: &mut sscratch };
    let mut charger = Session::new(TestCharger::default());
    if let Some(guard) = charger.handle(Event::Connected { secured: true }, &mut charger_buffers).pilot {
        CHARGER_GUARDS.with(|g| g.borrow_mut().push(guard));
    }
    let mut rig = Rig { vehicle, charger, charger_buffers, ended: None, loops: 0, stop_after, closed: false, halt_at: None, halted: false, host };
    if host.report {
        // Plugged in: B with the oscillator on.
        rig.charger.handle(Event::Pilot(control_pilot::Reading::CONNECTED), &mut rig.charger_buffers);
    }
    let start = rig.vehicle.handle(EvEvent::Start { secured: true });
    rig.process(start);
    for _ in 0..200 {
        if rig.ended.is_some() || rig.closed {
            break;
        }
        let s = rig.vehicle.handle(EvEvent::TimerExpired(evcc_core::Timer::Poll));
        rig.process(s);
    }
    let state = rig.charger.state();
    (rig.ended, rig.closed, state, std::mem::take(rig.charger.secc_mut()))
}

#[test]
fn a_session_with_the_pilot_reported_as_the_standards_have_it_runs_to_the_end() {
    let (end, _, state, charger) = pilot_run(PilotHost { report: true, ..Default::default() }, 3);
    assert_eq!(end, Some(EndReason::Completed));
    assert_eq!(state, State::Stopped);
    assert_eq!(charger.loops, 3);
    assert_eq!(charger.cable_checks, 2);
}

#[test]
fn a_cable_check_without_c_or_d_is_answered_failed() {
    // V2G20-918: the vehicle was not (measured to be) in C when the request came.
    let (end, closed, _, charger) = pilot_run(PilotHost { report: true, skip_c: true, ..Default::default() }, 3);
    assert!(matches!(end, Some(EndReason::Rejected(ResponseCode::Failed))), "{end:?}");
    assert!(closed);
    assert_eq!(charger.cable_checks, 0, "the charger's own check was not even asked");
}

#[test]
fn a_welding_detection_without_b_is_answered_failed() {
    // V2G20-922: after the stop the charger measures no B.
    let (end, _, _, _) = pilot_run(PilotHost { report: true, skip_b: true, ..Default::default() }, 3);
    assert!(matches!(end, Some(EndReason::Rejected(ResponseCode::Failed))), "{end:?}");
}

#[test]
fn the_vehicle_leaving_c_during_the_charge_is_an_emergency_for_the_charger() {
    use control_pilot::{Pwm, Reading, State as Cp};
    for reading in [Reading::CONNECTED, Reading { state: Cp::B, pwm: Pwm::Off }, Reading { state: Cp::Invalid, pwm: Pwm::Duty(5) }] {
        let (end, closed, state, charger) = pilot_run(PilotHost { report: true, inject: Some((2, reading)), ..Default::default() }, 100);
        assert_eq!(end, None, "the vehicle gets nothing more: {end:?}");
        assert!(closed);
        assert_eq!(state, State::Closed);
        assert_eq!(charger.ended, Some(evse_core::secc::Ended::Emergency(reading)));
        assert!(!charger.delivering || charger.loops == 2, "no more loop answers after the emergency");
    }
    // State E, F and A: the connection to the vehicle is gone.
    let e = Reading { state: Cp::E, pwm: Pwm::Off };
    let (_, closed, state, charger) = pilot_run(PilotHost { report: true, inject: Some((2, e)), ..Default::default() }, 100);
    assert!(closed);
    assert_eq!(state, State::Closed);
    assert_eq!(charger.ended, Some(evse_core::secc::Ended::Emergency(e)), "after C an E is an emergency too");
}

#[test]
fn the_pilot_going_to_b_after_the_stop_is_no_emergency() {
    // The vehicle's own B after PowerDeliveryRes(Stop), reported while the charger waits for the welding detection.
    let (end, closed, _, charger) = pilot_run(PilotHost { report: true, ..Default::default() }, 2);
    assert_eq!(end, Some(EndReason::Completed));
    assert!(closed);
    assert!(matches!(charger.ended, Some(evse_core::secc::Ended::Stopped(_))), "{:?}", charger.ended);
}

#[test]
fn the_machines_tell_the_host_what_to_watch_as_the_session_goes() {
    use control_pilot::{Guard, Side};
    VEHICLE_GUARDS.with(|g| g.borrow_mut().clear());
    CHARGER_GUARDS.with(|g| g.borrow_mut().clear());
    let (end, _, _, _) = pilot_run(PilotHost { report: true, ..Default::default() }, 3);
    assert_eq!(end, Some(EndReason::Completed));
    let vehicle = VEHICLE_GUARDS.with(|g| g.borrow().clone());
    let charger = CHARGER_GUARDS.with(|g| g.borrow().clone());
    let vg = |hold_ready: bool, hold_five_percent: bool, close_on_fault: bool| Guard { side: Side::Vehicle, hold_ready, hold_five_percent, static_stops_supply: false, close_on_fault };
    // Vehicle: armed for the faults when the session starts; the duty cycle is to be 5 % once the charge parameters are in;
    // C is held from its own change; the shutdown makes both free; the end disarms.
    assert_eq!(vehicle, [vg(false, false, true), vg(false, true, true), vg(true, true, true), vg(false, false, true), Guard::disarmed(Side::Vehicle)], "{vehicle:?}");
    let cg = |hold_ready: bool, close_on_fault: bool| Guard { side: Side::Charger, hold_ready, hold_five_percent: false, static_stops_supply: false, close_on_fault };
    // Charger: armed for the faults when the connection is accepted; C is held once the vehicle was seen in C; the stop makes B
    // meant (and with it nothing is judged any more, which the end does not change).
    assert_eq!(charger, [cg(false, true), cg(true, true), Guard::disarmed(Side::Charger)], "{charger:?}");
}

#[test]
fn a_vehicle_that_goes_quiet_for_the_sequence_timeout_ends_the_session_as_such() {
    // Not a lost connection: for DIN the host has an emergency shutdown to carry out (V2G-DC-366).
    let (mut srx, mut stx, mut sarena, mut sscratch) = (vec![0u8; 8192], vec![0u8; 8192], vec![0u8; 32768], vec![0u8; 8192]);
    let mut buffers = Buffers { rx: &mut srx, tx: &mut stx, arena: &mut sarena, scratch: &mut sscratch };
    let mut charger = Session::new(TestCharger::default());
    charger.handle(Event::Connected { secured: true }, &mut buffers);
    let step = charger.handle(Event::TimerExpired(evse_core::Timer::Sequence), &mut buffers);
    assert!(step.close);
    assert_eq!(charger.secc_mut().ended, Some(evse_core::secc::Ended::SequenceTimeout));
    assert_eq!(charger.state(), State::Closed);
}

#[test]
fn a_frame_with_a_wrong_v2gtp_header_is_ignored() {
    // V2G20-800 (ISO/PAS 15118-23 TC23_SECC_V2GTPDCMessages_003..006): no answer, no close, the state stays.
    let junk: [&[u8]; 5] = [
        &[0xFF, 0x00, 0x80, 0x01, 0, 0, 0, 1, 0],       // version
        &[0x01, 0xFF, 0x80, 0x01, 0, 0, 0, 1, 0],       // inverse version
        &[0x01, 0xFE, 0x70, 0x01, 0, 0, 0, 1, 0],       // payload type
        &[0x01, 0xFE, 0x80, 0x01, 0, 0, 0, 0],          // payload length 0
        &[0x01, 0xFE, 0x80, 0x01, 0, 0, 0, 3, 1, 2, 3], // a payload that is no EXI
    ];
    for (i, bytes) in junk.iter().enumerate() {
        let (mut srx, mut stx, mut sarena, mut sscratch) = (vec![0u8; 8192], vec![0u8; 8192], vec![0u8; 32768], vec![0u8; 8192]);
        let mut buffers = Buffers { rx: &mut srx, tx: &mut stx, arena: &mut sarena, scratch: &mut sscratch };
        let mut charger = Session::new(TestCharger::default());
        charger.handle(Event::Connected { secured: true }, &mut buffers);
        let before = charger.state();
        charger.rx_space(buffers.rx)[..bytes.len()].copy_from_slice(bytes);
        let step = charger.handle(Event::Received(bytes.len()), &mut buffers);
        if i == 4 {
            // Undecodable EXI is another matter (closed before the application protocol is negotiated).
            continue;
        }
        assert_eq!(step.send, 0, "case {i}");
        assert!(!step.close, "case {i}");
        assert_eq!(charger.state(), before, "case {i}");
    }
}

// ---------------------------------------------------------------- ACDP

fn acdp_ev() -> TestEv {
    TestEv { clock: 1.into(), offer: false, v2x: false }
}

#[test]
fn an_acdp_vehicle_is_positioned_by_the_charger_before_anything_else() {
    // Three rounds until the driver has parked and braked; the session ends after the positioning.
    IMMOBILIZED_AFTER.with(|c| c.set(3));
    let (end, charger, state) = run(config(Config::ACDP_20.positioning_only()), acdp_ev(), 0);
    assert_eq!(end, EndReason::Completed);
    assert_eq!(state, State::Stopped);
    let seen: Vec<(bool, bool)> = charger.positioning.iter().map(|r| (r.immobilized, r.positioning_support)).collect();
    assert_eq!(seen, [(false, true), (false, true), (true, true)], "mobilized twice, then immobilized");
}

#[test]
fn a_vehicle_the_ppd_loses_is_answered_with_an_association_error() {
    IMMOBILIZED_AFTER.with(|c| c.set(5));
    let secc = TestCharger { lose_association_at: Some(2), ..TestCharger::default() };
    let (end, charger, state) = run_with(secc, config(Config::ACDP_20), acdp_ev(), 0);
    assert_eq!(end, EndReason::Rejected(ResponseCode::FailedAssociationError));
    // V2G20-4025, 4031: the connection stays open for the session stop.
    assert_eq!(state, State::Stopped);
    assert_eq!(charger.positioning.len(), 2);
}

#[test]
fn a_charger_without_acdp_refuses_the_protocol() {
    let secc = TestCharger { no_acdp: true, ..TestCharger::default() };
    let (end, charger, _) = run_with(secc, config(Config::ACDP_20), acdp_ev(), 0);
    assert_eq!(end, EndReason::NoCommonProtocol);
    assert!(charger.positioning.is_empty());
}

#[test]
fn an_acdp_session_connects_the_pantograph_after_the_schedule_exchange_and_disconnects_it_before_the_stop() {
    IMMOBILIZED_AFTER.with(|c| c.set(1));
    let (end, charger, state) = run(config(Config::ACDP_20), acdp_ev(), 3);
    assert_eq!(end, EndReason::Completed);
    assert_eq!(state, State::Stopped);
    let order: Vec<&str> = charger.pantograph.iter().map(|(k, _)| *k).collect();
    assert_eq!(order, ["connect", "connect", "disconnect", "disconnect"], "moving, down; moving, home");
    assert_eq!(charger.loops, 3);
    assert_eq!(charger.cable_checks, 2, "the cable check follows the connect");
    assert!(!charger.delivering);
}

#[test]
fn an_acdp_bpt_session_runs_the_same_way() {
    IMMOBILIZED_AFTER.with(|c| c.set(1));
    let (end, charger, state) = run(config(Config::ACDP_BPT_20), acdp_ev(), 3);
    assert_eq!((end, state), (EndReason::Completed, State::Stopped));
    assert_eq!(charger.pantograph.len(), 4);
}

#[test]
fn a_vehicle_that_positions_itself_although_it_negotiated_the_dc_schema_is_out_of_sequence() {
    // The vehicle behaves as an ACDP one (service 4) but offered the DC namespace: the charger expects the authorization.
    let config = Config { energy_service: evcc_core::session::SERVICE_ID_DC_ACDP, ..config(Config::DC_20) };
    let (end, charger, _) = run(config, acdp_ev(), 3);
    assert_eq!(end, EndReason::Rejected(ResponseCode::FailedSequenceError));
    assert!(charger.positioning.is_empty());
}

#[test]
fn the_status_stream_runs_beside_an_acdp_session_and_is_answered_throughout() {
    IMMOBILIZED_AFTER.with(|c| c.set(1));
    STATUS_NOTICES.with(|c| c.set(0));
    let (end, charger, state) = run(config(Config::ACDP_20), acdp_ev(), 3);
    assert_eq!((end, state), (EndReason::Completed, State::Stopped));
    assert!(charger.status_requests >= 4, "{} requests: one with the first request and one for every charge loop answer (a tick of the timer)", charger.status_requests);
    assert_eq!(STATUS_NOTICES.with(std::cell::Cell::get), charger.status_requests, "every answer reached the vehicle's host");
}

#[test]
fn a_status_answer_does_not_restart_the_sequence_timer_of_the_main_stream() {
    use app_protocol::{AppProtocol, SupportedAppProtocolReq};
    use evse_core::TimerCmd;
    use iso15118_20::common::SessionSetupReq;
    use iso15118_20::common_types::MessageHeader;
    let (mut rx, mut tx, mut arena, mut scratch) = (vec![0u8; 4096], vec![0u8; 4096], vec![0u8; 16384], vec![0u8; 4096]);
    let mut buffers = Buffers { rx: &mut rx, tx: &mut tx, arena: &mut arena, scratch: &mut scratch };
    let mut charger = Session::new(TestCharger::default());
    charger.handle(Event::Connected { secured: true }, &mut buffers);
    let feed = |charger: &mut Session<TestCharger>, buffers: &mut Buffers<'_>, payload_type: v2gtp::PayloadType, encode: &dyn Fn(&mut [u8]) -> usize| {
        let mut f = vec![0u8; 512];
        let n = encode(&mut f[8..]);
        v2gtp::write_header(&mut f, payload_type, n).unwrap();
        let space = charger.rx_space(buffers.rx);
        space[..8 + n].copy_from_slice(&f[..8 + n]);
        charger.handle(Event::Received(8 + n), buffers)
    };
    let offer = [AppProtocol { protocol_namespace: "urn:iso:std:iso:15118:-20:ACDP", version_number_major: 1, version_number_minor: 0, schema_id: 3, priority: 1 }];
    feed(&mut charger, &mut buffers, v2gtp::PayloadType::SAP, &|o| SupportedAppProtocolReq { app_protocol: &offer }.encode(o).unwrap());
    let setup = feed(&mut charger, &mut buffers, v2gtp::PayloadType::MAINSTREAM, &|o| SessionSetupReq { header: MessageHeader::new([0; 8], 1), evccid: "WMIV1234567890ABCDEX" }.encode(o).unwrap());
    assert!(matches!(setup.timer, Some(TimerCmd::Arm { .. })), "a main stream answer restarts the timer");
    let status = |o: &mut [u8]| {
        iso15118_20::acdp::AcdpSystemStatusReq {
            header: MessageHeader::new([1, 2, 3, 4, 5, 6, 7, 8], 2),
            ev_technical_status: iso15118_20::acdp::EvTechnicalStatus {
                ev_ready_to_charge: true,
                ev_immobilization_request: false,
                ev_immobilized: None,
                evwlan_strength: None,
                evcp_status: None,
                evsoc: None,
                ev_error_code: None,
                ev_timeout: None,
            },
        }
        .encode(o)
        .unwrap()
    };
    let answer = feed(&mut charger, &mut buffers, v2gtp::PayloadType::ACDP_SYSTEM_STATUS, &status);
    assert!(answer.send > 0, "answered");
    assert_eq!(answer.timer, None, "the sequence timer is for the main stream");
    assert_eq!(charger.secc().status_requests, 1);
}
