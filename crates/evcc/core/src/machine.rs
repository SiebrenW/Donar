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
}
