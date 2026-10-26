//! What every charger machine (ISO 15118-20, ISO 15118-2, DIN SPEC 70121) shares with its host, whatever the codec.

/// The answer of a step that may take several rounds.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Progress<T> {
    /// Not yet: the machine answers `Ongoing` and the vehicle asks again.
    Ongoing,
    Done(T),
}

/// What the machine does with a request before looking at it (for faults and tests).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum Action {
    #[default]
    Pass,
    /// Do not answer.
    Silent,
    /// Close the connection without answering.
    Close,
}

/// How the vehicle stopped a session (`ChargingSession` of the `SessionStopReq`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Stop {
    Terminate,
    Pause,
    ServiceRenegotiation,
}

#[cfg(feature = "iso20")]
impl From<iso15118_20::common::ChargingSession> for Stop {
    fn from(s: iso15118_20::common::ChargingSession) -> Self {
        match s {
            iso15118_20::common::ChargingSession::Terminate => Stop::Terminate,
            iso15118_20::common::ChargingSession::Pause => Stop::Pause,
            iso15118_20::common::ChargingSession::ServiceRenegotiation => Stop::ServiceRenegotiation,
        }
    }
}

/// How a session ended its service flow, for `session_ended` of the charger trait.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Ended {
    Stopped(Stop),
    /// The connection went away.
    Lost,
    /// The vehicle sent nothing for `V2G_SECC_Sequence_Timeout` (60 s). For DIN SPEC 70121 the charger then carries out an
    /// EVSE-initiated emergency shutdown as IEC 61851-23 has it (V2G-DC-366: the oscillator off, the current down) and closes the
    /// connection, which is the host's part; ISO 15118-2 and -20 only stop the session (V2G2-537, and the sequence timer of ISO 15118-20 8.6).
    SequenceTimeout,
    /// The control pilot said emergency (the vehicle left state C/D, IEC 61851-23 CC.4.3): the session ended
    /// without a word. The host turns the oscillator off, brings the current down and opens its contactors.
    Emergency(control_pilot::Reading),
    /// The control pilot is in state E, F or A, or invalid: the vehicle or the cable is gone.
    PilotFault(control_pilot::Reading),
}
