//! What the core needs to know about the vehicle that is instant and static enough to be a
//! plain call. Anything physical or slow (cable check, precharge, contactors) will go
//! through events and actions instead, so the core never waits inside a call.

pub trait Ev {
    /// EVCCID sent in `SessionSetupReq` (at most 255 characters, usually the MAC address).
    fn evcc_id(&self) -> &str;
    /// Value for the header `TimeStamp` (`xs:unsignedLong`) of messages the EV sends.
    ///
    /// Open question: the XSD gives no unit. EcoG's stack sends microseconds since the Unix
    /// epoch; a plain "Unix time" reading would be seconds. The core passes the value through
    /// unchanged, so the host decides until this is checked against ISO 15118-20:2022.
    fn header_timestamp(&self) -> u64;
}
