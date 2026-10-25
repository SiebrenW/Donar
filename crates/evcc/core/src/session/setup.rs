//! Session setup, authorization and service selection.

use app_protocol::{self, SupportedAppProtocolReq};
use iso15118_20::common::*;
use iso15118_20::common_types::{MessageHeader, Processing, ResponseCode};
use iso15118_20::ac;
use iso15118_20::dc;
use v2gtp::PayloadType;

use super::{Machine, frame, is_ok};
use super::types::*;
use crate::ev::{AcDischargePowerLimits, AcPowerLimits, Ev};
use crate::log::{LogSink, MsgKind, Record};
use crate::{Timer, TimerCmd, Timers};

impl<E: Ev, L: LogSink> Machine<E, L> {
    pub(super) fn on_session_setup_res(&mut self, res: SessionSetupRes<'_>, tx: &mut [u8]) -> Step {
        self.log.record(&Record::MsgRx { msg: MsgKind::SessionSetupRes });
        self.log.record(&Record::Response { msg: MsgKind::SessionSetupRes, code: res.response_code });
        self.check_response_code(MsgKind::SessionSetupRes, res.response_code);
        if !is_ok(res.response_code) {
            return self.end(EndReason::Rejected(res.response_code));
        }
        let resuming = self.config.resume;
        self.session_id = res.header.session_id;
        self.clock_offset = i128::from(res.header.time_stamp) - i128::from(self.ev.now_micros());
        // Switching to EVSE time may move the stamps backwards: the first two
        // messages are exempt from "always increasing" (V2G20-1538).
        self.last_timestamp = 0;
        self.log.record(&Record::SessionSetUp { session_id: self.session_id, evse_id: res.evseid });
        if let Some(old) = resuming {
            // V2G20-2539: joined with the id we sent. (That it is still the same charger is
            // for the host to check: the certificate of the new TLS connection, 8.3.4.1.4.3.)
            let joined = res.response_code == ResponseCode::OkOldSessionJoined && res.header.session_id == old.session_id;
            // V2G20-2615: any other id is a new session, with new authorization (V2G20-2617).
            let new = res.response_code == ResponseCode::OkNewSessionEstablished && res.header.session_id != old.session_id;
            if joined {
                // V2G20-1844, V2G20-2098: authorization and service selection are valid; the
                // next request is the charge parameter discovery.
                self.log.record(&Record::SessionResumed { session_id: old.session_id });
                self.selected = old.selected;
                self.renegotiation_supported = old.renegotiation_supported;
                let framed = self.frame_charge_parameter_discovery_req(self.energy_payload_type(), tx);
                let mut step = self.send_then(State::AwaitChargeParameterDiscoveryRes, self.cpd_req_kind(), framed);
                if step.end.is_none() {
                    step.timers = step.timers.with(TimerCmd::Cancel(Timer::Setup));
                    step.notice = Some(Notice::SessionResumed { session_id: old.session_id });
                }
                return step;
            }
            if !new {
                // V2G20-2613, V2G20-2614: neither joined nor a clean new session.
                return self.end(EndReason::UnexpectedMessage);
            }
        }
        // ACDP: the vehicle is positioned first (8.3.4.7.5).
        let mut step = if self.config.acdp() {
            // The status stream starts with the first request (V2G20-4016).
            let step = self.send_vehicle_positioning_req(tx);
            self.start_status(step, tx)
        } else {
            self.send_authorization_setup_req(tx)
        };
        step.notice = Some(Notice::SessionSetUp { session_id: self.session_id });
        if step.end.is_none() {
            step.timers = step.timers.with(TimerCmd::Cancel(Timer::Setup));
        }
        step
    }

    pub(super) fn send_authorization_setup_req(&mut self, tx: &mut [u8]) -> Step {
        let msg = AuthorizationSetupReq { header: self.header() };
        let framed = frame(tx, PayloadType::MAINSTREAM, |out| msg.encode(out));
        self.send_then(State::AwaitAuthorizationSetupRes, MsgKind::AuthorizationSetupReq, framed)
    }

    pub(super) fn on_authorization_setup_res(&mut self, res: AuthorizationSetupRes<'_>, tx: &mut [u8]) -> Step {
        self.log.record(&Record::MsgRx { msg: MsgKind::AuthorizationSetupRes });
        self.log.record(&Record::Response { msg: MsgKind::AuthorizationSetupRes, code: res.response_code });
        self.check_response_code(MsgKind::AuthorizationSetupRes, res.response_code);
        if !is_ok(res.response_code) {
            return self.end(EndReason::Rejected(res.response_code));
        }
        self.eim_offered = res.authorization_services.contains(&Authorization::Eim);
        // Plug & Charge when asked for, offered (with its challenge, V2G20-1219) and possible.
        let challenge = match res.as_res_authorization_mode {
            AsResAuthorizationModeChoice::PnC(mode) => Some(mode.gen_challenge),
            _ => None,
        };
        match challenge {
            Some(challenge) if self.config.prefer_pnc && res.authorization_services.contains(&Authorization::PnC) && self.ev.contract().is_some() => {
                self.pnc_challenge = Some(challenge);
                self.authorization = Authorization::PnC;
            }
            _ if self.eim_offered => self.authorization = Authorization::Eim,
            _ => {
                self.log.record(&Record::NoAuthorization { wanted: Authorization::Eim, offered: res.authorization_services });
                return self.end_politely(EndReason::NoCompatibleAuthorization, tx);
            }
        }
        self.log.record(&Record::AuthorizationSelected { service: self.authorization });
        self.send_authorization_req(State::AwaitAuthorizationRes, tx)
    }

    pub(super) fn on_authorization_res(&mut self, res: AuthorizationRes, tx: &mut [u8]) -> Step {
        self.log.record(&Record::MsgRx { msg: MsgKind::AuthorizationRes });
        self.log.record(&Record::Response { msg: MsgKind::AuthorizationRes, code: res.response_code });
        self.check_response_code(MsgKind::AuthorizationRes, res.response_code);
        self.log.record(&Record::EvseProcessing { processing: res.evse_processing });
        // V2G20-1583: a warning about the contract; EIM is the way out when it is on offer.
        if self.authorization == Authorization::PnC
            && res.evse_processing == Processing::Finished
            && self.config.pnc_fallback_to_eim
            && self.eim_offered
            && is_pnc_warning(res.response_code)
        {
            self.authorization = Authorization::Eim;
            self.log.record(&Record::AuthorizationSelected { service: self.authorization });
            return self.send_authorization_req(State::AwaitAuthorizationRes, tx);
        }
        if !is_ok(res.response_code) {
            return self.end(EndReason::Rejected(res.response_code));
        }
        match res.evse_processing {
            Processing::Finished => {
                let mut step = self.send_service_discovery_req(tx);
                if step.end.is_none() {
                    self.ongoing_armed = false;
                    step.timers = step.timers.with(TimerCmd::Cancel(Timer::Ongoing));
                    step.notice = Some(Notice::Authorized);
                }
                step
            }
            // Ref: ISO 15118-20:2022 V2G20-1582: after an AuthorizationRes with
            // ResponseCode OK and EVSEProcessing Ongoing the EVCC sends another
            // unaltered AuthorizationReq (except TimeStamp) within
            // V2G_EVCC_Sequence_Performance_Time. Ongoing_WaitingForCustomerInteraction
            // is not in the 2022 text (only in the schema); it is treated the same.
            Processing::Ongoing | Processing::OngoingWaitingForCustomerInteraction => {
                self.ongoing_repeat(tx)
            }
        }
    }

    pub(super) fn send_service_discovery_req(&mut self, tx: &mut [u8]) -> Step {
        let msg = ServiceDiscoveryReq { header: self.header(), supported_service_ids: None };
        let framed = frame(tx, PayloadType::MAINSTREAM, |out| msg.encode(out));
        self.send_then(State::AwaitServiceDiscoveryRes, MsgKind::ServiceDiscoveryReq, framed)
    }

    pub(super) fn on_service_discovery_res(&mut self, res: ServiceDiscoveryRes<'_>, tx: &mut [u8]) -> Step {
        if let Some(end) = self.received(MsgKind::ServiceDiscoveryRes, res.response_code) {
            return end;
        }
        // V2G20-1480: only the first ServiceDiscoveryRes of the service session counts.
        if self.renegotiation_supported.is_none() {
            self.renegotiation_supported = Some(res.service_renegotiation_supported);
        }
        let wanted = self.config.energy_service;
        if !res.energy_transfer_service_list.service.iter().any(|s| s.service_id == wanted) {
            self.log.record(&Record::NoService { wanted, offered: res.energy_transfer_service_list.service });
            return self.end_politely(EndReason::NoCompatibleService, tx);
        }
        let msg = ServiceDetailReq { header: self.header(), service_id: wanted };
        let framed = frame(tx, PayloadType::MAINSTREAM, |out| msg.encode(out));
        self.send_then(State::AwaitServiceDetailRes, MsgKind::ServiceDetailReq, framed)
    }

    pub(super) fn on_service_detail_res(&mut self, res: ServiceDetailRes<'_>, tx: &mut [u8]) -> Step {
        if let Some(end) = self.received(MsgKind::ServiceDetailRes, res.response_code) {
            return end;
        }
        if res.service_id != self.config.energy_service {
            return self.end(EndReason::UnexpectedMessage);
        }
        let Some(parameter_set_id) = self.pick_parameter_set(res.service_parameter_list.parameter_set) else {
            self.log.record(&Record::NoParameterSet {
                connector: if self.config.ac() { self.ev.ac_connector() as i32 } else { self.ev.dc_connector() as i32 },
                filter: self.config.parameter_filter,
                offered: res.service_parameter_list.parameter_set,
            });
            return self.end_politely(EndReason::NoCompatibleParameterSet, tx);
        };
        let selected = SelectedService { service_id: res.service_id, parameter_set_id };
        self.selected = selected;
        // The connector of the chosen AC set decides how phase specific values are written.
        self.ac_three_phase = res.service_parameter_list.parameter_set.iter().find(|s| s.parameter_set_id == parameter_set_id).is_some_and(|set| {
            set.parameter.iter().any(|p| p.name == "Connector" && matches!(p.choice, ParameterChoice::ByteValue(2) | ParameterChoice::ShortValue(2) | ParameterChoice::IntValue(2)))
        });
        let msg = ServiceSelectionReq {
            header: self.header(),
            selected_energy_transfer_service: selected,
            selected_vas_list: None,
        };
        let framed = frame(tx, PayloadType::MAINSTREAM, |out| msg.encode(out));
        self.send_then(State::AwaitServiceSelectionRes, MsgKind::ServiceSelectionReq, framed)
    }

    pub(super) fn on_service_selection_res(&mut self, res: ServiceSelectionRes, tx: &mut [u8]) -> Step {
        if let Some(end) = self.received(MsgKind::ServiceSelectionRes, res.response_code) {
            return end;
        }
        let SelectedService { service_id, parameter_set_id } = self.selected;
        self.log.record(&Record::ServiceSelected { service_id, parameter_set_id });
        // V2G20-930: in AC the pilot is measured at 5 % before the request goes out. A host that reports no pilot, or a pilot
        // that is right already, does not hold it up.
        let mut step = if self.config.ac() && self.pilot.last().is_some_and(|r| !super::duty_cycle_for_hlc(r.pwm)) {
            self.goto(State::AwaitDutyCycle);
            Step { timers: Timers::one(TimerCmd::Cancel(Timer::Response)).with(self.arm_sequence()), ..Step::default() }
        } else {
            self.send_charge_parameter_discovery_req(tx)
        };
        if step.end.is_none() {
            step.notice = Some(Notice::ServiceSelected { service_id, parameter_set_id });
        }
        step
    }

    /// The first request for the EVSE's limits (after the service selection, or once the pilot is right).
    pub(super) fn send_charge_parameter_discovery_req(&mut self, tx: &mut [u8]) -> Step {
        let framed = self.frame_charge_parameter_discovery_req(self.energy_payload_type(), tx);
        self.send_then(State::AwaitChargeParameterDiscoveryRes, self.cpd_req_kind(), framed)
    }

    /// The payload type of the energy service's own messages (charge parameter discovery, charge
    /// loop).
    pub(super) fn energy_payload_type(&self) -> PayloadType {
        if self.config.ac() { PayloadType::AC_MAINSTREAM } else { PayloadType::DC_MAINSTREAM }
    }

    pub(super) fn cpd_req_kind(&self) -> MsgKind {
        if self.config.ac() { MsgKind::AcChargeParameterDiscoveryReq } else { MsgKind::DcChargeParameterDiscoveryReq }
    }

    /// The request for the EVSE's limits; also the first message of a schedule renegotiation.
    pub(super) fn frame_charge_parameter_discovery_req(&mut self, payload_type: PayloadType, tx: &mut [u8]) -> Result<usize, ()> {
        if self.config.ac() {
            return self.frame_ac_charge_parameter_discovery_req(payload_type, tx);
        }
        let limits = self.ev.dc_limits();
        self.ev_announced = Some(limits);
        let mode = if self.config.bpt() {
            let d = self.ev.dc_discharge_limits();
            self.ev_announced_discharge = Some(d);
            dc::DcCpdReqEnergyTransferModeChoice::Bpt(dc::BptDcCpdReqEnergyTransferMode {
                ev_maximum_charge_power: limits.maximum_charge_power,
                ev_minimum_charge_power: limits.minimum_charge_power,
                ev_maximum_charge_current: limits.maximum_charge_current,
                ev_minimum_charge_current: limits.minimum_charge_current,
                ev_maximum_voltage: limits.maximum_voltage,
                ev_minimum_voltage: limits.minimum_voltage,
                target_soc: limits.target_soc.map(|s| s as i8),
                ev_maximum_discharge_power: d.maximum_discharge_power,
                ev_minimum_discharge_power: d.minimum_discharge_power,
                ev_maximum_discharge_current: d.maximum_discharge_current,
                ev_minimum_discharge_current: d.minimum_discharge_current,
            })
        } else {
            dc::DcCpdReqEnergyTransferModeChoice::Base(dc::DcCpdReqEnergyTransferMode {
                ev_maximum_charge_power: limits.maximum_charge_power,
                ev_minimum_charge_power: limits.minimum_charge_power,
                ev_maximum_charge_current: limits.maximum_charge_current,
                ev_minimum_charge_current: limits.minimum_charge_current,
                ev_maximum_voltage: limits.maximum_voltage,
                ev_minimum_voltage: limits.minimum_voltage,
                target_soc: limits.target_soc.map(|s| s as i8),
            })
        };
        let msg = dc::DcChargeParameterDiscoveryReq { header: self.header(), dc_cpd_req_energy_transfer_mode: mode };
        frame(tx, payload_type, |out| msg.encode(out))
    }

    fn frame_ac_charge_parameter_discovery_req(&mut self, payload_type: PayloadType, tx: &mut [u8]) -> Result<usize, ()> {
        let l = self.ev.ac20_limits();
        let l = AcPowerLimits { maximum_charge_power: self.lines(l.maximum_charge_power), minimum_charge_power: self.lines(l.minimum_charge_power) };
        self.ev_announced_ac = Some(l);
        let mode = if self.config.bpt() {
            let d = self.ev.ac20_discharge_limits();
            let d = AcDischargePowerLimits { maximum_discharge_power: self.lines(d.maximum_discharge_power), minimum_discharge_power: self.lines(d.minimum_discharge_power) };
            self.ev_announced_ac_discharge = Some(d);
            ac::AcCpdReqEnergyTransferModeChoice::Bpt(ac::BptAcCpdReqEnergyTransferMode {
                ev_maximum_charge_power: l.maximum_charge_power.l1,
                ev_maximum_charge_power_l2: l.maximum_charge_power.l2,
                ev_maximum_charge_power_l3: l.maximum_charge_power.l3,
                ev_minimum_charge_power: l.minimum_charge_power.l1,
                ev_minimum_charge_power_l2: l.minimum_charge_power.l2,
                ev_minimum_charge_power_l3: l.minimum_charge_power.l3,
                ev_maximum_discharge_power: d.maximum_discharge_power.l1,
                ev_maximum_discharge_power_l2: d.maximum_discharge_power.l2,
                ev_maximum_discharge_power_l3: d.maximum_discharge_power.l3,
                ev_minimum_discharge_power: d.minimum_discharge_power.l1,
                ev_minimum_discharge_power_l2: d.minimum_discharge_power.l2,
                ev_minimum_discharge_power_l3: d.minimum_discharge_power.l3,
            })
        } else {
            ac::AcCpdReqEnergyTransferModeChoice::Base(ac::AcCpdReqEnergyTransferMode {
                ev_maximum_charge_power: l.maximum_charge_power.l1,
                ev_maximum_charge_power_l2: l.maximum_charge_power.l2,
                ev_maximum_charge_power_l3: l.maximum_charge_power.l3,
                ev_minimum_charge_power: l.minimum_charge_power.l1,
                ev_minimum_charge_power_l2: l.minimum_charge_power.l2,
                ev_minimum_charge_power_l3: l.minimum_charge_power.l3,
            })
        };
        let msg = ac::AcChargeParameterDiscoveryReq { header: self.header(), ac_cpd_req_energy_transfer_mode: mode };
        frame(tx, payload_type, |out| msg.encode(out))
    }

    pub(super) fn send_supported_app_protocol_req(&mut self, tx: &mut [u8]) -> Step {
        let offer = [self.config.offer];
        let msg = SupportedAppProtocolReq { app_protocol: &offer };
        let framed = frame(tx, PayloadType::SAP, |out| msg.encode(out));
        self.send_then(State::AwaitSupportedAppProtocolRes, MsgKind::SupportedAppProtocolReq, framed)
    }
    pub(super) fn on_supported_app_protocol_res(&mut self, res: app_protocol::SupportedAppProtocolRes, tx: &mut [u8]) -> Step {
        if res.response_code == app_protocol::ResponseCode::FailedNoNegotiation {
            return self.end(EndReason::NoCommonProtocol);
        }
        // The EVSE must pick the schema id we offered.
        if res.schema_id != Some(self.config.offer.schema_id) {
            return self.end(EndReason::NoCommonProtocol);
        }
        let msg = SessionSetupReq {
            // V2G20-1243: zeroes for a new session, V2G20-1541: the old id to resume one.
            header: MessageHeader::new(self.config.resume.map_or([0; 8], |r| r.session_id), self.timestamp()),
            evccid: self.ev.evcc_id(),
        };
        let framed = frame(tx, PayloadType::MAINSTREAM, |out| msg.encode(out));
        self.send_then(State::AwaitSessionSetupRes, MsgKind::SessionSetupReq, framed)
    }
    pub(super) fn send_authorization_req(&mut self, next: State, tx: &mut [u8]) -> Step {
        let framed = if self.authorization == Authorization::PnC {
            let (session_id, time_stamp, challenge) = (self.session_id, self.timestamp(), self.pnc_challenge.unwrap_or([0; 16]));
            match self.ev.contract() {
                Some(contract) => frame_pnc_authorization_req(contract, challenge, session_id, time_stamp, tx),
                None => return self.end(EndReason::UnexpectedMessage),
            }
        } else {
            // EIM carries no data beyond the header; a repeat is identical but for its timestamp.
            let msg = AuthorizationReq {
                header: self.header(),
                selected_authorization_service: Authorization::Eim,
                a_req_authorization_mode: AReqAuthorizationModeChoice::Eim(EimAReqAuthorizationMode {}),
            };
            frame(tx, PayloadType::MAINSTREAM, |out| msg.encode(out))
        };
        if self.state == next {
            // A repeat: stay in the same state without logging a transition.
            return self.sent(MsgKind::AuthorizationReq, framed);
        }
        self.send_then(next, MsgKind::AuthorizationReq, framed)
    }
    pub(super) fn pick_parameter_set(&self, sets: &[ParameterSet<'_>]) -> Option<u16> {
        let as_int = |v: ParameterChoice<'_>| match v {
            ParameterChoice::ByteValue(v) => Some(i32::from(v)),
            ParameterChoice::ShortValue(v) => Some(i32::from(v)),
            ParameterChoice::IntValue(v) => Some(v),
            _ => None,
        };
        // A three-phase charger also serves a single-phase vehicle.
        let ac_connector = self.ev.ac_connector() as i32;
        let connector = self.ev.dc_connector() as i32;
        let is_ac = self.config.ac();
        let has = |set: &ParameterSet<'_>, name: &str, want: i32| {
            set.parameter.iter().any(|p| p.name == name && as_int(p.choice) == Some(want))
        };
        sets.iter()
            .find(|set| {
                (if is_ac { has(set, "Connector", ac_connector) || has(set, "Connector", 2) } else if self.config.acdp() { true } else { has(set, "Connector", connector) })
                    && self.config.parameter_filter.iter().all(|&(name, want)| has(set, name, want))
                    && self.config.optional_parameters.iter().all(|&(name, want)| has(set, name, want) || !set.parameter.iter().any(|p| p.name == name))
            })
            .map(|set| set.parameter_set_id)
    }}

/// `Id` of the `PnC_AReqAuthorizationMode` element the signature refers to.
const PNC_ID: &str = "id1";
const PNC_URI: &str = "#id1";

/// The warnings of V2G20-2212 to 2217 about the contract, after which V2G20-1583 lets the
/// vehicle try something else.
fn is_pnc_warning(code: ResponseCode) -> bool {
    matches!(
        code,
        ResponseCode::WarningCertificateExpired
            | ResponseCode::WarningCertificateNotYetValid
            | ResponseCode::WarningCertificateRevoked
            | ResponseCode::WarningCertificateValidationError
            | ResponseCode::WarningChallengeInvalid
            | ResponseCode::WarningGeneralPnCAuthorizationError
            | ResponseCode::WarningNoCertificateAvailable
            | ResponseCode::WarningNoContractMatchingPcidFound
            | ResponseCode::WarningEMspUnknown
    )
}

/// Signs the fragment `scratch[..length]` (the stream of the element that `uri` refers to) with the
/// contract key (V2G20-1063): its digest in a `SignedInfo`, the signature over the `SignedInfo`
/// fragment, which is written into `scratch` after the digest was taken. `build` frames the message
/// that carries the signature in its header.
pub(super) fn sign_fragment(
    contract: &dyn crate::ev::Contract,
    uri: &str,
    scratch: &mut [u8],
    length: usize,
    build: impl FnOnce(iso15118_20::signature::Signature<'_>) -> Result<usize, ()>,
) -> Result<usize, ()> {
    use crate::ev::{MAX_DIGEST_LEN, MAX_SIGNATURE_LEN};
    use iso15118_20::signature::{CANONICAL_EXI, Reference, Signature, SignatureValue, SignedInfo};
    let mut digest = [0u8; MAX_DIGEST_LEN];
    let digest_len = contract.digest(&scratch[..length], &mut digest).ok_or(())?;
    let references = [Reference {
        id: None,
        uri: Some(uri),
        transform_algorithm: Some(CANONICAL_EXI),
        digest_algorithm: contract.digest_algorithm(),
        digest_value: &digest[..digest_len],
    }];
    let info = SignedInfo { id: None, canonicalization_algorithm: CANONICAL_EXI, signature_algorithm: contract.signature_algorithm(), reference: &references };
    let length = info.encode_fragment(scratch).map_err(|_| ())?;
    let mut value = [0u8; MAX_SIGNATURE_LEN];
    let value_len = contract.sign(&scratch[..length], &mut value).ok_or(())?;
    build(Signature { id: None, signed_info: info, signature_value: SignatureValue { id: None, value: &value[..value_len] } })
}

/// The `AuthorizationReq` for Plug & Charge, framed into `tx`: the `PnC_AReqAuthorizationMode`
/// with the challenge and the contract chain, and in the header the signature over it with the
/// contract key (V2G20-1063). The second half of `tx` is scratch for the two fragments that are
/// digested and signed; the frame is built in the first.
fn frame_pnc_authorization_req(contract: &dyn crate::ev::Contract, challenge: [u8; 16], session_id: [u8; 8], time_stamp: u64, tx: &mut [u8]) -> Result<usize, ()> {
    use crate::ev::MAX_SUB_CERTIFICATES;
    use iso15118_20::signature::encode_fragment;
    let (out, scratch) = tx.split_at_mut(tx.len() / 2);
    let mut subs: [&[u8]; MAX_SUB_CERTIFICATES] = [&[]; MAX_SUB_CERTIFICATES];
    let mut count = 0;
    while count < MAX_SUB_CERTIFICATES {
        let Some(c) = contract.sub_certificate(count) else { break };
        subs[count] = c;
        count += 1;
    }
    let mode = PnCAReqAuthorizationMode {
        id: PNC_ID,
        gen_challenge: challenge,
        contract_certificate_chain: ContractCertificateChain { certificate: contract.certificate(), sub_certificates: SubCertificates { certificate: &subs[..count] } },
    };
    let length = encode_fragment(scratch, iso15118_20::signature::fragment::PNC_A_REQ_AUTHORIZATION_MODE, |e| mode.encode_content(e)).map_err(|_| ())?;
    sign_fragment(contract, PNC_URI, scratch, length, |signature| {
        let msg = AuthorizationReq {
            header: MessageHeader { session_id, time_stamp, signature: Some(signature) },
            selected_authorization_service: Authorization::PnC,
            a_req_authorization_mode: AReqAuthorizationModeChoice::PnC(mode),
        };
        frame(out, PayloadType::MAINSTREAM, |o| msg.encode(o))
    })
}
