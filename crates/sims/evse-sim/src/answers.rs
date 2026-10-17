//! The message and response code of a frame the charger sent, for the log and for the check against the
//! tables of response codes (Tables 224 and 226).

use exi::Arena;
use iso15118_20::ResponseMessage;
use iso15118_20::ac;
use iso15118_20::common;
use iso15118_20::common_types::ResponseCode;
use iso15118_20::dc;
use v2gtp::{Parse, PayloadType, parse};

/// The message and response code of a frame the charger built (a response), and its name.
pub fn response_of(bytes: &[u8]) -> Option<(&'static str, ResponseMessage, ResponseCode)> {
    let Ok(Parse::Frame(f)) = parse(bytes, 1 << 20) else { return None };
    let mut mem = vec![0u8; 16384];
    let arena = Arena::new(&mut mem);
    use ResponseMessage as M;
    if f.payload_type == PayloadType::AC_MAINSTREAM {
        return Some(match ac::decode(f.payload, &arena).ok()? {
            ac::Message::AcChargeParameterDiscoveryRes(r) => ("AC_ChargeParameterDiscoveryRes", M::AcChargeParameterDiscovery, r.response_code),
            ac::Message::AcChargeLoopRes(r) => ("AC_ChargeLoopRes", M::AcChargeLoop, r.response_code),
            _ => return None,
        });
    }
    if f.payload_type != PayloadType::DC_MAINSTREAM
        && let Ok(m) = common::decode(f.payload, &arena) {
            return Some(match m {
                common::Message::SessionSetupRes(r) => ("SessionSetupRes", M::SessionSetup, r.response_code),
                common::Message::AuthorizationSetupRes(r) => ("AuthorizationSetupRes", M::AuthorizationSetup, r.response_code),
                common::Message::AuthorizationRes(r) => ("AuthorizationRes", M::Authorization, r.response_code),
                common::Message::ServiceDiscoveryRes(r) => ("ServiceDiscoveryRes", M::ServiceDiscovery, r.response_code),
                common::Message::ServiceDetailRes(r) => ("ServiceDetailRes", M::ServiceDetail, r.response_code),
                common::Message::ServiceSelectionRes(r) => ("ServiceSelectionRes", M::ServiceSelection, r.response_code),
                common::Message::ScheduleExchangeRes(r) => ("ScheduleExchangeRes", M::ScheduleExchange, r.response_code),
                common::Message::PowerDeliveryRes(r) => ("PowerDeliveryRes", M::PowerDelivery, r.response_code),
                common::Message::SessionStopRes(r) => ("SessionStopRes", M::SessionStop, r.response_code),
                _ => return None,
            });
        }
    Some(match dc::decode(f.payload, &arena).ok()? {
        dc::Message::DcChargeParameterDiscoveryRes(r) => ("DC_ChargeParameterDiscoveryRes", M::DcChargeParameterDiscovery, r.response_code),
        dc::Message::DcCableCheckRes(r) => ("DC_CableCheckRes", M::DcCableCheck, r.response_code),
        dc::Message::DcPreChargeRes(r) => ("DC_PreChargeRes", M::DcPreCharge, r.response_code),
        dc::Message::DcChargeLoopRes(r) => ("DC_ChargeLoopRes", M::DcChargeLoop, r.response_code),
        dc::Message::DcWeldingDetectionRes(r) => ("DC_WeldingDetectionRes", M::DcWeldingDetection, r.response_code),
        _ => return None,
    })
}
