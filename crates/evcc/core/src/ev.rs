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

/// What the vehicle can feed back into the grid or a load (bidirectional power transfer, BPT).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct DischargeLimits {
    pub maximum_discharge_power: RationalNumber,
    pub minimum_discharge_power: RationalNumber,
    pub maximum_discharge_current: RationalNumber,
    pub minimum_discharge_current: RationalNumber,
}

/// The vehicle's AC charging parameters (ISO 15118-2 `AC_EVChargeParameter`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct AcLimits {
    /// `EAmount`: the energy the vehicle wants.
    pub energy_amount: RationalNumber,
    pub maximum_voltage: RationalNumber,
    pub maximum_current: RationalNumber,
    pub minimum_current: RationalNumber,
}

/// A quantity per phase: line 1, and lines 2 and 3 of a three-phase connection (ISO 15118-20 AC
/// messages carry `_L2` and `_L3` variants of every power).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Phases {
    pub l1: RationalNumber,
    pub l2: Option<RationalNumber>,
    pub l3: Option<RationalNumber>,
}

impl Phases {
    /// The same on one phase only.
    pub const fn single(l1: RationalNumber) -> Self {
        Self { l1, l2: None, l3: None }
    }
    /// The same on all three phases.
    pub const fn three(each: RationalNumber) -> Self {
        Self { l1: each, l2: Some(each), l3: Some(each) }
    }
}

/// The vehicle's AC charging limits of ISO 15118-20 (`AC_CPDReqEnergyTransferMode`), per phase.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct AcPowerLimits {
    pub maximum_charge_power: Phases,
    pub minimum_charge_power: Phases,
}

/// What the vehicle can feed back, per phase (AC bidirectional power transfer).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct AcDischargePowerLimits {
    pub maximum_discharge_power: Phases,
    pub minimum_discharge_power: Phases,
}

/// What the on-board charger draws (or feeds back) right now, reported in every AC charge loop
/// request.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct AcPresentPower {
    pub active: Phases,
    pub reactive: Phases,
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

/// Value of the AC service parameter `Connector` (ISO 15118-20:2022 Table 205).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(u8)]
pub enum AcConnector {
    SinglePhase = 1,
    ThreePhase = 2,
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
    /// The eMAID of the contract (ISO 15118-2 `PaymentDetailsReq`), if the contract has one.
    fn emaid(&self) -> &str {
        ""
    }
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
    /// The MAC address of the EVCC: the EVCCID of DIN SPEC 70121 (six octets).
    fn evcc_mac(&self) -> [u8; 6] {
        [0; 6]
    }
    /// Which DC inlet the vehicle has; selects the matching `Connector` service parameter set.
    fn dc_connector(&self) -> DcConnector;
    /// The vehicle's DC limits for this session. Static facts, so a plain call; live values
    /// (present voltage, target current) will come through events once the charge loop exists.
    fn dc_limits(&self) -> DcLimits;
    /// The vehicle's discharge limits for BPT sessions ([`crate::session::Config::energy_service`]
    /// `SERVICE_ID_DC_BPT`). The default is a small vehicle-to-load capability.
    fn dc_discharge_limits(&self) -> DischargeLimits {
        DischargeLimits {
            maximum_discharge_power: RationalNumber { exponent: 3, value: 11 },
            minimum_discharge_power: RationalNumber { exponent: 0, value: 100 },
            maximum_discharge_current: RationalNumber { exponent: 0, value: 30 },
            minimum_discharge_current: RationalNumber { exponent: 0, value: 1 },
        }
    }
    /// The vehicle's AC limits (ISO 15118-2 AC charging).
    fn ac_limits(&self) -> AcLimits {
        AcLimits {
            energy_amount: RationalNumber { exponent: 3, value: 20 },
            maximum_voltage: RationalNumber { exponent: 0, value: 400 },
            maximum_current: RationalNumber { exponent: 0, value: 32 },
            minimum_current: RationalNumber { exponent: 0, value: 6 },
        }
    }
    /// Which AC connection the vehicle has (ISO 15118-20 AC); selects the `Connector` service
    /// parameter set. A three-phase charger also serves a single-phase vehicle.
    fn ac_connector(&self) -> AcConnector {
        AcConnector::ThreePhase
    }
    /// The vehicle's AC power limits per phase (ISO 15118-20 AC), sent in
    /// `AC_ChargeParameterDiscoveryReq` and held to in the charge loop.
    fn ac20_limits(&self) -> AcPowerLimits {
        let kw = |v| RationalNumber { exponent: 3, value: v };
        AcPowerLimits { maximum_charge_power: Phases::three(kw(4)), minimum_charge_power: Phases::single(RationalNumber { exponent: 0, value: 1400 }) }
    }
    /// The vehicle's AC discharge limits for BPT sessions
    /// ([`crate::session::SERVICE_ID_AC_BPT`]).
    fn ac20_discharge_limits(&self) -> AcDischargePowerLimits {
        AcDischargePowerLimits {
            maximum_discharge_power: Phases::three(RationalNumber { exponent: 3, value: 4 }),
            minimum_discharge_power: Phases::single(RationalNumber { exponent: 0, value: 500 }),
        }
    }
    /// The power the on-board charger draws now, for the AC charge loop request. The default is
    /// "nothing", for a host that does not measure.
    fn ac_present_power(&self) -> AcPresentPower {
        let zero = Phases::three(RationalNumber { exponent: 0, value: 0 });
        AcPresentPower { active: zero, reactive: zero }
    }
    // The limits above are the vehicle's *capabilities*, sent once in the charge parameter
    // discovery. The ones below are what it can take *right now*, read before every charge loop
    // request (`DC_ChargeLoopReq`, `AC_ChargeLoopReq`, `CurrentDemandReq`, `ChargingStatusReq`):
    // they follow temperature, state of charge, battery derating. By default they are the
    // capabilities. With `Config::enforce_cpd_limits` the session still keeps them inside the
    // announced envelope (maximums no higher, minimums no lower).

    /// DC limits for the next charge loop request.
    fn dc_loop_limits(&self) -> DcLimits {
        self.dc_limits()
    }
    /// Discharge limits (BPT) for the next charge loop request.
    fn dc_discharge_loop_limits(&self) -> DischargeLimits {
        self.dc_discharge_limits()
    }
    /// ISO 15118-20 AC power limits for the next charge loop request.
    fn ac20_loop_limits(&self) -> AcPowerLimits {
        self.ac20_limits()
    }
    /// ISO 15118-20 AC discharge limits for the next charge loop request.
    fn ac20_discharge_loop_limits(&self) -> AcDischargePowerLimits {
        self.ac20_discharge_limits()
    }
    /// ISO 15118-2 AC limits for the next charging status exchange.
    fn ac_loop_limits(&self) -> AcLimits {
        self.ac_limits()
    }
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
