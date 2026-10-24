//! What a host adapter needs from a session machine, whichever protocol it speaks.

use crate::session::{Event, State, Step};

/// A sans-IO session machine: the host feeds it events and carries out the [`Step`]s it returns
/// (see [`crate::session`] for the host contract). Implemented by the ISO 15118-20
/// [`crate::session::Session`] and the DIN SPEC 70121 [`crate::din::Session`].
pub trait Machine {
    fn handle(&mut self, event: Event) -> Step;
    /// Free space to read the next bytes from the stream into.
    fn rx_space(&mut self) -> &mut [u8];
    /// The outgoing message buffer; valid up to [`Step::send`] after a step.
    fn tx(&self) -> &[u8];
    fn state(&self) -> State;
    /// Whether a complete frame is still buffered after a step: [`Machine::handle`] with [`Event::Received`] consumes one
    /// frame, so a host calls `handle(Event::Received(0))` for the next one before it waits for the stream again. (Frames
    /// arrive together when a side stream runs beside the main stream; they must not be handled in one go, because each
    /// has a step of its own.)
    fn frame_pending(&self) -> bool;
    /// What the control pilot is expected to do now, for the host's own fast monitor ([`control_pilot::Guard`]);
    /// the same as the last [`Step::pilot`].
    fn pilot_guard(&self) -> control_pilot::Guard;
}

/// Whether `rx[..len]` starts with a complete V2GTP frame.
pub(crate) fn frame_buffered(rx: &[u8], len: usize) -> bool {
    matches!(v2gtp::parse(&rx[..len], rx.len() - v2gtp::HEADER_LEN), Ok(v2gtp::Parse::Frame(_)))
}
