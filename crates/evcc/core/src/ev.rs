//! What the core needs to know about the vehicle that is instant and static enough to be a
//! plain call. Anything physical or slow (cable check, precharge, contactors) will go
//! through events and actions instead, so the core never waits inside a call.

use iso15118_20::common_types::RationalNumber;

/// The vehicle's DC charging limits, sent in `DC_ChargeParameterDiscoveryReq`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct DcLimits {
    pub maximum_charge_power: RationalNumber,
    pub minimum_charge_power: RationalNumber,
    pub maximum_charge_current: RationalNumber,
    pub minimum_charge_current: RationalNumber,
    pub maximum_voltage: RationalNumber,
    pub minimum_voltage: RationalNumber,
    /// Target state of charge in percent (0..=100), if the vehicle states one.
    pub target_soc: Option<u8>,
}

/// What the vehicle tells the EVSE about its energy needs (Dynamic control mode).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ScheduleRequest {
    /// `DepartureTime`: seconds from the moment of sending the message (the `TimeStamp` in its
    /// header), always larger than zero (V2G20-2103, V2G20-2104).
    pub departure_time: u32,
    pub minimum_soc: Option<u8>,
    pub target_soc: Option<u8>,
    pub target_energy: RationalNumber,
    pub maximum_energy: RationalNumber,
    pub minimum_energy: RationalNumber,
}

/// Value of the DC service parameter `Connector` (ISO 15118-20:2022 Table 207).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(u8)]
pub enum DcConnector {
    Core = 1,
    Extended = 2,
    Dual2 = 3,
    Dual4 = 4,
}

/// The most sub-CA certificates a contract certificate chain carries (`SubCertificates`).
pub const MAX_SUB_CERTIFICATES: usize = 4;
/// Room for the largest digest (SHAKE256 as ISO 15118-20 uses it: 64 octets).
pub const MAX_DIGEST_LEN: usize = 64;
/// Room for the largest signature value (ECDSA secp521r1: r and s in 66 octets each).
pub const MAX_SIGNATURE_LEN: usize = 144;

/// Plug & Charge: the vehicle's contract certificate chain and the means to sign with the
/// contract's private key, which stays with the host (a secure element, a file). The session
/// builds the fragments and the signature structure (ISO 15118-20 7.9, V2G20-1063) and asks for
/// the two primitives: the digest of a fragment and the signature over the `SignedInfo`
/// fragment.
pub trait Contract {
    /// The contract certificate, DER.
    fn certificate(&self) -> &[u8];
    /// The sub-CA certificates, DER, the issuer of the contract certificate first; `None` past
    /// the last (at most [`MAX_SUB_CERTIFICATES`] are sent).
    fn sub_certificate(&self, index: usize) -> Option<&[u8]>;
    /// The `xmldsig` URI of the signature method (`iso15118_20::signature::ECDSA_SHA512`, `ED448`).
    fn signature_algorithm(&self) -> &str;
    /// The `xmlenc` URI of the digest method (`DIGEST_SHA512`, `DIGEST_SHAKE256`).
    fn digest_algorithm(&self) -> &str;
    /// The digest of `data` into `out`; its length, or `None` when it cannot be made.
    fn digest(&self, data: &[u8], out: &mut [u8]) -> Option<usize>;
    /// The signature over `data` into `out`; its length, or `None` when it cannot be made.
    fn sign(&self, data: &[u8], out: &mut [u8]) -> Option<usize>;
}

pub trait Ev {
    /// EVCCID sent in `SessionSetupReq` (at most 255 characters, usually the MAC address).
    fn evcc_id(&self) -> &str;
    /// The vehicle's own clock in microseconds since the Unix epoch, or 0 if it has no idea of
    /// the time (ISO 15118-20:2022 V2G20-1535). The session machine turns it into the header
    /// `TimeStamp`, which must be in the EVSE's time (V2G20-1534): it learns the offset from
    /// `SessionSetupRes` and keeps the stamps increasing (V2G20-1537), so a clock that is only
    /// approximately right, or even a plain counter, works.
    fn now_micros(&self) -> u64;
    /// Which DC inlet the vehicle has; selects the matching `Connector` service parameter set.
    fn dc_connector(&self) -> DcConnector;
    /// The vehicle's DC limits for this session. Static facts, so a plain call; live values
    /// (present voltage, target current) will come through events once the charge loop exists.
    fn dc_limits(&self) -> DcLimits;
    /// The voltage of the vehicle's battery system (read when the cable check is done and
    /// during pre-charge).
    fn battery_voltage(&self) -> RationalNumber;
    /// The voltage the vehicle measures at its inlet (side B); during pre-charge it follows the
    /// EVSE's output. Read each time a `DC_PreChargeRes` arrives.
    fn inlet_voltage(&self) -> RationalNumber;
    /// The vehicle's energy needs for the schedule exchange.
    fn schedule_request(&self) -> ScheduleRequest;
    /// The vehicle's contract certificate chain for Plug & Charge, if it has one. Used when
    /// [`crate::session::Config::prefer_pnc`] is set and the charger offers PnC.
    fn contract(&self) -> Option<&dyn Contract> {
        None
    }
    /// The present state of charge in percent, reported in the charge loop (`DisplayParameters`)
    /// when the vehicle knows it.
    fn present_soc(&self) -> Option<u8> {
        None
    }
}
