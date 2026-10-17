//! Which response codes a response message may carry (ISO 15118-20:2022 8.6, Table 224 for the
//! common messages, Table 226 for the DC messages). A response with a code outside its list is
//! a protocol violation of the sender; the codes of the AC, WPT and ACDP messages are not
//! covered.

use crate::common_types::ResponseCode;
use ResponseCode::*;

/// The response messages of a DC session.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ResponseMessage {
    SessionSetup,
    AuthorizationSetup,
    Authorization,
    ServiceDiscovery,
    ServiceDetail,
    ServiceSelection,
    ScheduleExchange,
    PowerDelivery,
    MeteringConfirmation,
    CertificateInstallation,
    SessionStop,
    DcChargeParameterDiscovery,
    DcCableCheck,
    DcPreCharge,
    DcChargeLoop,
    DcWeldingDetection,
    AcChargeParameterDiscovery,
    AcChargeLoop,
}

impl ResponseMessage {
    pub const ALL: [ResponseMessage; 18] = [
        ResponseMessage::SessionSetup,
        ResponseMessage::AuthorizationSetup,
        ResponseMessage::Authorization,
        ResponseMessage::ServiceDiscovery,
        ResponseMessage::ServiceDetail,
        ResponseMessage::ServiceSelection,
        ResponseMessage::ScheduleExchange,
        ResponseMessage::PowerDelivery,
        ResponseMessage::MeteringConfirmation,
        ResponseMessage::CertificateInstallation,
        ResponseMessage::SessionStop,
        ResponseMessage::DcChargeParameterDiscovery,
        ResponseMessage::DcCableCheck,
        ResponseMessage::DcPreCharge,
        ResponseMessage::DcChargeLoop,
        ResponseMessage::DcWeldingDetection,
        ResponseMessage::AcChargeParameterDiscovery,
        ResponseMessage::AcChargeLoop,
    ];

    /// The name of the message as in the schema, without `Res`.
    pub fn name(self) -> &'static str {
        match self {
            ResponseMessage::SessionSetup => "SessionSetup",
            ResponseMessage::AuthorizationSetup => "AuthorizationSetup",
            ResponseMessage::Authorization => "Authorization",
            ResponseMessage::ServiceDiscovery => "ServiceDiscovery",
            ResponseMessage::ServiceDetail => "ServiceDetail",
            ResponseMessage::ServiceSelection => "ServiceSelection",
            ResponseMessage::ScheduleExchange => "ScheduleExchange",
            ResponseMessage::PowerDelivery => "PowerDelivery",
            ResponseMessage::MeteringConfirmation => "MeteringConfirmation",
            ResponseMessage::CertificateInstallation => "CertificateInstallation",
            ResponseMessage::SessionStop => "SessionStop",
            ResponseMessage::DcChargeParameterDiscovery => "DC_ChargeParameterDiscovery",
            ResponseMessage::DcCableCheck => "DC_CableCheck",
            ResponseMessage::DcPreCharge => "DC_PreCharge",
            ResponseMessage::DcChargeLoop => "DC_ChargeLoop",
            ResponseMessage::DcWeldingDetection => "DC_WeldingDetection",
            ResponseMessage::AcChargeParameterDiscovery => "AC_ChargeParameterDiscovery",
            ResponseMessage::AcChargeLoop => "AC_ChargeLoop",
        }
    }

    /// Whether `code` is one the message may carry.
    pub fn allows(self, code: ResponseCode) -> bool {
        // Every response may say OK or that something failed (Tables 224 and 226); all but the
        // first one of a session may also say that the session is not known.
        if matches!(code, Ok | Failed | FailedSequenceError) {
            return true;
        }
        if code == FailedUnknownSession {
            return self != ResponseMessage::SessionSetup;
        }
        use ResponseMessage as M;
        match self {
            M::SessionSetup => matches!(code, OkNewSessionEstablished | OkOldSessionJoined),
            M::ServiceDetail => code == FailedServiceIdInvalid,
            M::ServiceSelection => matches!(code, FailedServiceSelectionInvalid | FailedNoEnergyTransferServiceSelected | FailedNoServiceRenegotiationSupported),
            M::Authorization => matches!(
                code,
                OkCertificateExpiresSoon
                    | WarningCertificateExpired
                    | WarningCertificateNotYetValid
                    | WarningCertificateRevoked
                    | WarningCertificateValidationError
                    | WarningEMspUnknown
                    | WarningGeneralPnCAuthorizationError
                    | WarningChallengeInvalid
                    | WarningAuthorizationSelectionInvalid
                    | WarningEimAuthorizationFailure
                    | FailedSignatureError
            ),
            M::PowerDelivery => matches!(
                code,
                OkPowerToleranceConfirmed
                    | WarningPowerToleranceNotConfirmed
                    | WarningStandbyNotAllowed
                    | FailedContactorError
                    | FailedEvPowerProfileInvalid
                    | FailedScheduleSelectionInvalid
                    | FailedPowerDeliveryNotApplied
                    | FailedPowerToleranceNotConfirmed
            ),
            M::MeteringConfirmation => matches!(code, FailedSignatureError | FailedMeteringSignatureNotValid),
            M::CertificateInstallation => matches!(
                code,
                WarningCertificateExpired
                    | WarningCertificateNotYetValid
                    | WarningCertificateRevoked
                    | WarningCertificateValidationError
                    | WarningEMspUnknown
                    | WarningNoCertificateAvailable
                    | WarningNoContractMatchingPcidFound
                    | FailedSignatureError
            ),
            M::SessionStop => matches!(code, FailedNoServiceRenegotiationSupported | FailedPauseNotAllowed),
            M::DcChargeParameterDiscovery | M::AcChargeParameterDiscovery => code == FailedWrongChargeParameter,
            M::DcChargeLoop | M::AcChargeLoop => matches!(
                code,
                WarningEvPowerProfileViolation | WarningScheduleRenegotiationFailed | FailedEvPowerProfileViolation | FailedScheduleRenegotiation
            ),
            M::AuthorizationSetup | M::ServiceDiscovery | M::ScheduleExchange | M::DcCableCheck | M::DcPreCharge | M::DcWeldingDetection => false,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_tables_as_read() {
        use ResponseMessage as M;
        // Table 226: the charge loop has the power profile codes, the cable check does not.
        assert!(M::DcChargeLoop.allows(WarningEvPowerProfileViolation));
        assert!(!M::DcCableCheck.allows(WarningEvPowerProfileViolation));
        // A wrong charge parameter belongs to the parameter discovery only.
        assert!(M::DcChargeParameterDiscovery.allows(FailedWrongChargeParameter));
        assert!(!M::DcChargeLoop.allows(FailedWrongChargeParameter));
        assert!(!M::ScheduleExchange.allows(FailedWrongChargeParameter));
        // The session id is not known yet in the first message.
        assert!(!M::SessionSetup.allows(FailedUnknownSession));
        assert!(M::ServiceDiscovery.allows(FailedUnknownSession));
        // Table 224: expiry soon is for the authorization, standby for the power delivery.
        assert!(M::Authorization.allows(OkCertificateExpiresSoon));
        assert!(!M::PowerDelivery.allows(OkCertificateExpiresSoon));
        assert!(M::PowerDelivery.allows(WarningStandbyNotAllowed));
        assert!(M::SessionStop.allows(FailedPauseNotAllowed));
        // OK and the two general failures are everywhere.
        for m in M::ALL {
            assert!(m.allows(Ok) && m.allows(Failed) && m.allows(FailedSequenceError), "{m:?}");
        }
    }

    #[test]
    fn every_code_is_allowed_somewhere() {
        // A code no message may carry (but the ones of AC, WPT and ACDP) would be a gap in the table.
        for code in ResponseCode::ALL {
            let used = ResponseMessage::ALL.iter().any(|m| m.allows(code));
            assert_eq!(used, !matches!(code, WarningWpt | FailedAssociationError), "{code:?}");
        }
    }
}
