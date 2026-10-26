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

/// The range of energy the vehicle prefers to cycle in with bidirectional power transfer (BPT), as the energy to the
/// ends of the range from the present state of charge (8.3.5.3.13, V2G20-2681, 2682).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct V2xEnergyRange {
    /// `EVMaximumV2XEnergyRequest`: energy that may be charged until the state of charge leaves the range at the top;
    /// negative when the state of charge is above the range.
    pub maximum: RationalNumber,
    /// `EVMinimumV2XEnergyRequest`: energy needed to enter the range from below; positive when the state of charge is
    /// below the range.
    pub minimum: RationalNumber,
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

/// The mobility needs the charger provided or changed (`MobilityNeedsMode` 2), as it states them in
/// `ScheduleExchangeRes` and the charge loop answers; a field is `None` when it did not name it.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct MobilityNeeds {
    /// Seconds from the time stamp of the answer that named it.
    pub departure_time: Option<u32>,
    pub minimum_soc: Option<u8>,
    pub target_soc: Option<u8>,
}

/// The rest of the vehicle's `DisplayParameters` (8.3.5.3.28), which a charger that shows the
/// charging state to the driver needs; mandatory for `MobilityNeedsMode` 2. The present, minimum and
/// target state of charge come from [`Ev::present_soc`] and the mobility needs.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct Display {
    pub maximum_soc: Option<u8>,
    pub remaining_time_to_minimum_soc: Option<u32>,
    pub remaining_time_to_target_soc: Option<u32>,
    pub remaining_time_to_maximum_soc: Option<u32>,
    pub charging_complete: Option<bool>,
    pub battery_energy_capacity: Option<RationalNumber>,
    pub inlet_hot: Option<bool>,
}

/// `EVEnergyOffer` of the Scheduled control mode (8.3.5.3.20): what the vehicle is prepared to
/// feed back, as a power schedule and the prices it asks for it. The time anchor of both is the
/// moment the message is sent. Built with [`EnergyOffer::new`], [`power`](EnergyOffer::power) and
/// [`price`](EnergyOffer::price); at most [`EnergyOffer::CAPACITY`] of each go on the wire, and each
/// price is one rule stack with a single rule from power range start 0.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct EnergyOffer {
    /// ISO 4217 currency code of the prices.
    pub currency: &'static str,
    /// For instance [`EnergyOffer::PRICE_PER_POWER`].
    pub price_algorithm: &'static str,
    power: [iso15118_20::common::EvPowerScheduleEntry; Self::CAPACITY],
    power_len: usize,
    prices: [OfferedPrice; Self::CAPACITY],
    prices_len: usize,
}

/// One price of an [`EnergyOffer`]: the fee per kWh for a duration.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct OfferedPrice {
    pub duration: u32,
    pub energy_fee: RationalNumber,
}

impl EnergyOffer {
    pub const CAPACITY: usize = 8;
    /// The price algorithm of ISO 15118-20 Table 76 that prices energy by power range.
    pub const PRICE_PER_POWER: &'static str = "urn:iso:std:iso:15118:-20:PriceAlgorithm:1-Power";

    const NONE: RationalNumber = RationalNumber { exponent: 0, value: 0 };

    pub const fn new(currency: &'static str) -> Self {
        Self {
            currency,
            price_algorithm: Self::PRICE_PER_POWER,
            power: [iso15118_20::common::EvPowerScheduleEntry { duration: 0, power: Self::NONE }; Self::CAPACITY],
            power_len: 0,
            prices: [OfferedPrice { duration: 0, energy_fee: Self::NONE }; Self::CAPACITY],
            prices_len: 0,
        }
    }

    /// Offers `power` (positive: fed back) for `duration` seconds after the previous entry.
    pub fn power(mut self, duration: u32, power: RationalNumber) -> Self {
        if self.power_len < Self::CAPACITY {
            self.power[self.power_len] = iso15118_20::common::EvPowerScheduleEntry { duration, power };
            self.power_len += 1;
        }
        self
    }

    /// Asks `energy_fee` per kWh for `duration` seconds after the previous price.
    pub fn price(mut self, duration: u32, energy_fee: RationalNumber) -> Self {
        if self.prices_len < Self::CAPACITY {
            self.prices[self.prices_len] = OfferedPrice { duration, energy_fee };
            self.prices_len += 1;
        }
        self
    }

    pub fn power_entries(&self) -> &[iso15118_20::common::EvPowerScheduleEntry] {
        &self.power[..self.power_len]
    }

    pub fn prices(&self) -> &[OfferedPrice] {
        &self.prices[..self.prices_len]
    }
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
/// Room for the largest digest (SHAKE256 as ISO 15118-20 Amd 1 uses it: 114 octets, V2G20-3012).
pub const MAX_DIGEST_LEN: usize = 114;
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

/// What every vehicle has, whatever it charges with or through: identity, clock, and what it asks of the schedule and shows to the
/// driver. The rest of the vehicle is split by capability, so that a host implements only what its hardware does: [`DcEv`], [`AcEv`] and,
/// per way of connecting, [`DcPlugEv`], [`AcPlugEv`] and [`InvertedPantographEv`]. The session machines ask for the combination they
/// need ([`Iso20Vehicle`], [`Iso2Vehicle`], [`DinVehicle`]).
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
    /// The range the vehicle prefers to cycle in with BPT; sent (in a BPT session) next to the energy requests, and the charger
    /// may try to stay within it. Kept between the minimum and maximum energy request by the session (V2G20-2682).
    fn v2x_energy_range(&self) -> Option<V2xEnergyRange> {
        None
    }
    /// The other display parameters ([`Display`]), sent next to the present state of charge.
    fn display(&self) -> Display {
        Display::default()
    }
    /// The energy the vehicle offers in Scheduled control mode (`EVEnergyOffer`); `None` offers none.
    fn energy_offer(&self) -> Option<EnergyOffer> {
        None
    }
}

/// A vehicle that charges with DC (a plug or a pantograph): its limits and the voltages the cable check and the pre-charge read.
pub trait DcEv: Ev {
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
    /// The voltage of the vehicle's battery system (read when the cable check is done and
    /// during pre-charge).
    fn battery_voltage(&self) -> RationalNumber;
    /// The voltage the vehicle measures at its inlet (side B); during pre-charge it follows the
    /// EVSE's output. Read each time a `DC_PreChargeRes` arrives.
    fn inlet_voltage(&self) -> RationalNumber;
}

/// A vehicle with a DC plug (CCS, CHAdeMO-style inlets): which inlet it has.
pub trait DcPlugEv: DcEv {
    /// Which DC inlet the vehicle has; selects the matching `Connector` service parameter set.
    fn dc_connector(&self) -> DcConnector;
}

/// A vehicle that charges with AC: its limits (ISO 15118-2 and ISO 15118-20) and the power its on-board charger draws.
pub trait AcEv: Ev {
    /// The vehicle's AC limits (ISO 15118-2 AC charging).
    fn ac_limits(&self) -> AcLimits {
        AcLimits {
            energy_amount: RationalNumber { exponent: 3, value: 20 },
            maximum_voltage: RationalNumber { exponent: 0, value: 400 },
            maximum_current: RationalNumber { exponent: 0, value: 32 },
            minimum_current: RationalNumber { exponent: 0, value: 6 },
        }
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
}

/// A vehicle with an AC plug: which AC connection it has.
pub trait AcPlugEv: AcEv {
    /// Which AC connection the vehicle has (ISO 15118-20 AC); selects the `Connector` service
    /// parameter set. A three-phase charger also serves a single-phase vehicle.
    fn ac_connector(&self) -> AcConnector {
        AcConnector::ThreePhase
    }
}

/// A vehicle that charges through the inverted pantograph, the charger-mounted one that lowers onto the roof rails (ACDP of ISO 15118-20,
/// OppCharge): its identity for the wireless pairing, how it is positioned and what it sees through the contacts. It has no AC and no plug.
pub trait InvertedPantographEv: Ev {
    /// ACDP (and WPT): the EVID of the wireless SDP request, a temporary identifier of the vehicle that the charger's pairing and
    /// positioning device reads in the vehicle's proximity (at most 20 letters and digits; ISO 15118-20 Table 25). The
    /// default is what a vehicle without one sends (V2G20-2281).
    fn evid(&self) -> &str {
        "ZZ00000"
    }
    /// ACDP: whether the vehicle can be guided into position by the charger (`EVPositioningSupport`); without, it positions
    /// itself (the driver with visual aids, sensors).
    fn acdp_positioning_support(&self) -> bool {
        false
    }
    /// ACDP: whether the vehicle is positioned and immobilized (brake applied, drive locked) and asks the pantograph to
    /// come down. The session keeps asking the charger with `EVMobilityStatus` mobilized until this is true. The default is
    /// true: a host that has nothing to wait for. The host decides when the vehicle is in position, from the guidance of
    /// [`crate::session::Notice::AcdpPosition`] or by its own means.
    fn immobilized(&self) -> bool {
        true
    }
    /// ACDP: how the vehicle is, as `ACDP_SystemStatusReq` tells the charger (Table 87, 8.3.4.7.8): ready to charge (false after an error of its
    /// own, V2G20-4013), the immobilization the driver wants (false when the driver releases the hand brake), the radio's signal strength (dBm,
    /// `EVWLANStrength`), the pilot state, the state of charge, an error code and a timeout. The default builds it from the other
    /// methods of the ACDP part and says nothing about the radio, the charge and errors.
    fn acdp_technical_status(&self) -> iso15118_20::acdp::EvTechnicalStatus {
        use iso15118_20::acdp::{CpStatus, ElectricalChargingDeviceStatus as E};
        let immobilized = self.immobilized();
        iso15118_20::acdp::EvTechnicalStatus {
            ev_ready_to_charge: true,
            ev_immobilization_request: immobilized,
            ev_immobilized: Some(immobilized),
            evwlan_strength: None,
            evcp_status: Some(match self.acdp_electrical_status() {
                E::StateA => CpStatus::StateA,
                E::StateB => CpStatus::StateB,
                E::StateC => CpStatus::StateC,
                E::StateD => CpStatus::StateD,
            }),
            evsoc: None,
            ev_error_code: None,
            ev_timeout: Some(false),
        }
    }
    /// ACDP: the control pilot state the vehicle sees through the pantograph's contacts, as `ACDP_Connect` and `ACDP_Disconnect` report it: A
    /// while the pantograph is up (no connection), B, C or D once it is down (Tables 83, 85). The default is A.
    fn acdp_electrical_status(&self) -> iso15118_20::acdp::ElectricalChargingDeviceStatus {
        iso15118_20::acdp::ElectricalChargingDeviceStatus::StateA
    }
}

/// What the ISO 15118-20 machine needs of a vehicle: every capability it can run (DC and AC through a plug, DC through the pantograph).
pub trait Iso20Vehicle: DcPlugEv + AcPlugEv + InvertedPantographEv {}
impl<T: DcPlugEv + AcPlugEv + InvertedPantographEv> Iso20Vehicle for T {}

/// What the ISO 15118-2 machine needs of a vehicle: DC and AC through a plug.
pub trait Iso2Vehicle: DcPlugEv + AcPlugEv {}
impl<T: DcPlugEv + AcPlugEv> Iso2Vehicle for T {}

/// What the DIN SPEC 70121 machine needs of a vehicle: DC through a plug.
pub trait DinVehicle: DcPlugEv {}
impl<T: DcPlugEv> DinVehicle for T {}
