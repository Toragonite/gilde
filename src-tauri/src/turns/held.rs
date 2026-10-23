//! The one held message of a turn-based session (contract §6.3 "Sending during
//! a turn").
//!
//! While a turn runs, `turn_send` does not start a second one: the message is
//! **held by the session** — at most one; a second is `CONFLICT{queueFull}`. It
//! starts the next turn when the running one ends with `Done`; on `Error` it
//! stays held (the session shows the error; the user cancels it or retries).
//! `turn_cancel_queued` drops it (`NOT_FOUND{message}` for another id). The text
//! is **[content]**: memory only, never persisted or logged.

use crate::contract::common::EpochMs;
use crate::contract::error::IpcError;
use crate::contract::ids::ClientMsgId;
use crate::contract::session::QueuedMessage;

/// The held message. Its text is content: it is never logged (no `Debug` of it).
#[derive(Clone, PartialEq, Eq)]
pub struct HeldMessage {
    pub client_msg_id: ClientMsgId,
    pub text: String,
    pub held_at: EpochMs,
}

impl std::fmt::Debug for HeldMessage {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("HeldMessage")
            .field("client_msg_id", &self.client_msg_id)
            .field("text_bytes", &self.text.len())
            .field("held_at", &self.held_at)
            .finish()
    }
}

impl HeldMessage {
    /// The store's shape of it (`Session.queued`, ≤ 1 for turn-based sessions).
    pub fn as_queued(&self) -> QueuedMessage {
        QueuedMessage {
            client_msg_id: self.client_msg_id.clone(),
            text: self.text.clone(),
            queued_at: self.held_at,
        }
    }
}

/// At most one held message.
#[derive(Debug, Default)]
pub struct HeldSlot {
    held: Option<HeldMessage>,
}

impl HeldSlot {
    pub fn new() -> HeldSlot {
        HeldSlot::default()
    }

    /// Hold `msg`; `CONFLICT{queueFull}` when a message is held already.
    pub fn hold(&mut self, msg: HeldMessage) -> Result<(), IpcError> {
        if self.held.is_some() {
            return Err(IpcError::conflict("queueFull"));
        }
        self.held = Some(msg);
        Ok(())
    }

    /// `turn_cancel_queued`: drop the held message with this id.
    /// `NOT_FOUND{message}` when nothing (or another message) is held.
    pub fn cancel(&mut self, id: &ClientMsgId) -> Result<bool, IpcError> {
        match &self.held {
            Some(held) if &held.client_msg_id == id => {
                self.held = None;
                Ok(true)
            }
            _ => Err(IpcError::not_found("message")),
        }
    }

    /// Take it to start the next turn.
    pub fn take(&mut self) -> Option<HeldMessage> {
        self.held.take()
    }

    /// Put it back (its turn could not start): it stays held.
    pub fn restore(&mut self, msg: HeldMessage) {
        if self.held.is_none() {
            self.held = Some(msg);
        }
    }

    /// Drop whatever is held (interrupt with `discardQueued`, stop, quit).
    pub fn clear(&mut self) -> Option<HeldMessage> {
        self.held.take()
    }

    pub fn is_held(&self) -> bool {
        self.held.is_some()
    }

    pub fn holds(&self, id: &ClientMsgId) -> bool {
        self.held.as_ref().is_some_and(|h| &h.client_msg_id == id)
    }

    pub fn peek(&self) -> Option<QueuedMessage> {
        self.held.as_ref().map(HeldMessage::as_queued)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::contract::error::ErrorCode;

    fn id(n: u8) -> ClientMsgId {
        ClientMsgId::parse(format!("1b4e28ba-2fa1-41d2-883f-0016d3cca4{n:02x}")).expect("uuid v4")
    }

    fn msg(n: u8) -> HeldMessage {
        HeldMessage {
            client_msg_id: id(n),
            text: format!("message {n}"),
            held_at: 1_000 + i64::from(n),
        }
    }

    #[test]
    fn one_message_is_held_and_a_second_is_queue_full() {
        let mut slot = HeldSlot::new();
        slot.hold(msg(1)).expect("the first is held");
        let err = slot.hold(msg(2)).expect_err("a second is refused");
        assert_eq!(err.code, ErrorCode::Conflict);
        assert_eq!(err.params.get("changed").and_then(|v| v.as_str()), Some("queueFull"));
        assert_eq!(slot.peek().map(|q| q.client_msg_id), Some(id(1)));
    }

    #[test]
    fn cancel_drops_only_the_held_id() {
        let mut slot = HeldSlot::new();
        slot.hold(msg(1)).expect("held");
        let other = slot.cancel(&id(2)).expect_err("another id");
        assert_eq!(other.code, ErrorCode::NotFound);
        assert!(slot.is_held());
        assert!(slot.cancel(&id(1)).expect("the held one"));
        assert!(!slot.is_held());
        assert_eq!(slot.cancel(&id(1)).expect_err("gone").code, ErrorCode::NotFound);
    }

    #[test]
    fn a_restored_message_stays_held_and_the_text_is_never_debug_printed() {
        let mut slot = HeldSlot::new();
        slot.hold(msg(3)).expect("held");
        let taken = slot.take().expect("taken");
        assert!(!slot.is_held());
        slot.restore(taken);
        assert!(slot.holds(&id(3)));
        let shown = format!("{:?}", slot);
        assert!(!shown.contains("message 3"), "content never reaches Debug: {shown}");
        assert_eq!(slot.clear().map(|m| m.held_at), Some(1_003));
    }
}
