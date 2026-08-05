//! Clipboard-agnostic, bounded paste confirmation policy.
//!
//! Adapters resolve their own clipboard and supply the text here. Multiline
//! bodies remain private inside [`PasteGuard`] until a matching confirmation
//! is approved, which makes the same state machine suitable for native and
//! remote clients.

use std::fmt;

/// Maximum UTF-8 clipboard body accepted by the shared paste path.
pub const MAX_PASTE_BYTES: usize = 16 * 1024;
/// Maximum UTF-8 bytes exposed to a confirmation UI as a preview.
pub const MAX_PASTE_PREVIEW_BYTES: usize = 1024;
const PREVIEW_ELLIPSIS: &str = "…";

/// Exact title shown by clients for a multiline paste confirmation.
pub const MULTILINE_PASTE_WARNING_TITLE: &str = "Warning";
/// Exact warning shown before a multiline clipboard body can reach the PTY.
pub const MULTILINE_PASTE_WARNING: &str = "You are about to paste text that contains multiple lines. If you paste this text into your shell, it may result in the unexpected execution of commands. Do you wish to continue?";
/// Exact label shown immediately before the bounded clipboard preview.
pub const MULTILINE_PASTE_PREVIEW_LABEL: &str = "Clipboard contents (preview):";
/// Exact affirmative action label for a multiline paste confirmation.
pub const MULTILINE_PASTE_APPROVE_LABEL: &str = "Paste anyway";
/// Exact cancellation action label for a multiline paste confirmation.
pub const MULTILINE_PASTE_CANCEL_LABEL: &str = "Cancel";

/// Opaque identity binding one confirmation to its retained clipboard body.
#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
pub struct PasteConfirmationId(u64);

impl PasteConfirmationId {
    /// Reconstruct a non-zero identity received from a transport client.
    #[must_use]
    pub const fn from_raw(value: u64) -> Option<Self> {
        if value == 0 { None } else { Some(Self(value)) }
    }

    /// Stable numeric representation for transport adapters.
    #[must_use]
    pub const fn get(self) -> u64 {
        self.0
    }
}

/// Bounded metadata safe to present in a confirmation UI.
#[derive(Clone, Eq, PartialEq)]
pub struct PasteConfirmation {
    id: PasteConfirmationId,
    preview: String,
    preview_truncated: bool,
    body_bytes: usize,
}

impl PasteConfirmation {
    /// Identity required to approve or cancel this exact retained body.
    #[must_use]
    pub const fn id(&self) -> PasteConfirmationId {
        self.id
    }

    /// Bounded clipboard preview. Callers must not log or persist it.
    #[must_use]
    pub fn preview(&self) -> &str {
        &self.preview
    }

    /// Whether the displayed preview omits a suffix of the clipboard body.
    #[must_use]
    pub const fn preview_truncated(&self) -> bool {
        self.preview_truncated
    }

    /// Original UTF-8 body length without exposing its contents.
    #[must_use]
    pub const fn body_bytes(&self) -> usize {
        self.body_bytes
    }
}

impl fmt::Debug for PasteConfirmation {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("PasteConfirmation")
            .field("id", &self.id)
            .field("preview", &"<redacted>")
            .field("preview_truncated", &self.preview_truncated)
            .field("body_bytes", &self.body_bytes)
            .finish()
    }
}

/// A paste body released exactly once for writing by the invoking adapter.
#[derive(Eq, PartialEq)]
pub struct ApprovedPaste(String);

impl ApprovedPaste {
    /// Borrow the approved text for a synchronous PTY write.
    #[must_use]
    pub fn text(&self) -> &str {
        &self.0
    }

    /// Consume the approved body.
    #[must_use]
    pub fn into_text(self) -> String {
        self.0
    }
}

impl fmt::Debug for ApprovedPaste {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("ApprovedPaste")
            .field("text", &"<redacted>")
            .field("bytes", &self.0.len())
            .finish()
    }
}

/// Result of supplying one resolved clipboard body to [`PasteGuard`].
#[derive(Debug, Eq, PartialEq)]
pub enum PasteRequest {
    /// Single-line text may be written immediately.
    Ready(ApprovedPaste),
    /// Multiline text is retained until this confirmation is approved.
    ConfirmationRequired(PasteConfirmation),
}

/// A bounded paste request could not be accepted.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum PasteRejection {
    /// The UTF-8 body exceeds the configured input limit.
    TooLarge {
        /// Observed UTF-8 byte length.
        body_bytes: usize,
        /// Maximum accepted UTF-8 byte length.
        max_bytes: usize,
    },
}

impl fmt::Display for PasteRejection {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::TooLarge {
                body_bytes,
                max_bytes,
            } => write!(
                f,
                "clipboard text is {body_bytes} bytes; paste is limited to {max_bytes} bytes"
            ),
        }
    }
}

impl std::error::Error for PasteRejection {}

/// A confirmation operation did not identify the currently retained body.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum PasteConfirmationError {
    /// No multiline body is awaiting a decision.
    NoPendingPaste,
    /// The caller supplied an identity for an older or unrelated body.
    StaleConfirmation {
        /// Identity currently awaiting a decision.
        expected: PasteConfirmationId,
        /// Identity supplied by the caller.
        actual: PasteConfirmationId,
    },
}

impl fmt::Display for PasteConfirmationError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::NoPendingPaste => f.write_str("no multiline paste is awaiting confirmation"),
            Self::StaleConfirmation { expected, actual } => write!(
                f,
                "paste confirmation {} is stale; confirmation {} is pending",
                actual.get(),
                expected.get()
            ),
        }
    }
}

impl std::error::Error for PasteConfirmationError {}

struct PendingPaste {
    id: PasteConfirmationId,
    text: String,
}

impl fmt::Debug for PendingPaste {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("PendingPaste")
            .field("id", &self.id)
            .field("text", &"<redacted>")
            .field("bytes", &self.text.len())
            .finish()
    }
}

/// Stateful bounded multiline guard shared by native and remote adapters.
pub struct PasteGuard {
    max_body_bytes: usize,
    max_preview_bytes: usize,
    next_id: u64,
    pending: Option<PendingPaste>,
}

impl Default for PasteGuard {
    fn default() -> Self {
        Self::new(MAX_PASTE_BYTES, MAX_PASTE_PREVIEW_BYTES)
    }
}

impl fmt::Debug for PasteGuard {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("PasteGuard")
            .field("max_body_bytes", &self.max_body_bytes)
            .field("max_preview_bytes", &self.max_preview_bytes)
            .field("next_id", &self.next_id)
            .field("pending", &self.pending)
            .finish()
    }
}

impl PasteGuard {
    /// Construct a guard with explicit body and preview UTF-8 byte bounds.
    ///
    /// A zero preview bound is valid and reveals no clipboard prefix.
    #[must_use]
    pub const fn new(max_body_bytes: usize, max_preview_bytes: usize) -> Self {
        Self {
            max_body_bytes,
            max_preview_bytes,
            next_id: 1,
            pending: None,
        }
    }

    /// Accept a resolved clipboard body or retain it for multiline approval.
    ///
    /// A newly accepted request supersedes any older pending request. Rejected
    /// oversized input leaves the existing confirmation untouched.
    ///
    /// # Errors
    ///
    /// Returns [`PasteRejection::TooLarge`] without retaining or exposing the
    /// new body when its UTF-8 representation exceeds the configured bound.
    pub fn request(&mut self, text: String) -> Result<PasteRequest, PasteRejection> {
        if text.len() > self.max_body_bytes {
            return Err(PasteRejection::TooLarge {
                body_bytes: text.len(),
                max_bytes: self.max_body_bytes,
            });
        }

        if !text.contains(['\r', '\n']) {
            self.pending = None;
            return Ok(PasteRequest::Ready(ApprovedPaste(text)));
        }

        let id = PasteConfirmationId(self.next_id);
        self.next_id = self.next_id.wrapping_add(1).max(1);
        let (preview, preview_truncated) = bounded_preview(&text, self.max_preview_bytes);
        let confirmation = PasteConfirmation {
            id,
            preview,
            preview_truncated,
            body_bytes: text.len(),
        };
        self.pending = Some(PendingPaste { id, text });
        Ok(PasteRequest::ConfirmationRequired(confirmation))
    }

    /// Release the body matching `id`, consuming it before the caller writes.
    ///
    /// # Errors
    ///
    /// Returns [`PasteConfirmationError`] when no body is pending or `id`
    /// refers to a different retained body.
    pub fn approve(
        &mut self,
        id: PasteConfirmationId,
    ) -> Result<ApprovedPaste, PasteConfirmationError> {
        let pending_id = self
            .pending
            .as_ref()
            .map(|pending| pending.id)
            .ok_or(PasteConfirmationError::NoPendingPaste)?;
        if pending_id != id {
            return Err(PasteConfirmationError::StaleConfirmation {
                expected: pending_id,
                actual: id,
            });
        }
        let pending = self
            .pending
            .take()
            .ok_or(PasteConfirmationError::NoPendingPaste)?;
        Ok(ApprovedPaste(pending.text))
    }

    /// Cancel the body matching `id` without releasing its contents.
    ///
    /// # Errors
    ///
    /// Returns [`PasteConfirmationError`] when no body is pending or `id`
    /// refers to a different retained body.
    pub fn cancel(&mut self, id: PasteConfirmationId) -> Result<(), PasteConfirmationError> {
        let Some(pending) = self.pending.as_ref() else {
            return Err(PasteConfirmationError::NoPendingPaste);
        };
        if pending.id != id {
            return Err(PasteConfirmationError::StaleConfirmation {
                expected: pending.id,
                actual: id,
            });
        }
        self.pending = None;
        Ok(())
    }

    /// Whether a multiline body is retained awaiting a decision.
    #[must_use]
    pub const fn has_pending(&self) -> bool {
        self.pending.is_some()
    }
}

fn bounded_preview(text: &str, max_bytes: usize) -> (String, bool) {
    if text.len() <= max_bytes {
        return (text.to_owned(), false);
    }
    if max_bytes == 0 {
        return (String::new(), true);
    }

    let prefix_limit = max_bytes.saturating_sub(PREVIEW_ELLIPSIS.len());
    let mut prefix_end = prefix_limit.min(text.len());
    while prefix_end > 0 && !text.is_char_boundary(prefix_end) {
        prefix_end -= 1;
    }
    let mut preview = text[..prefix_end].to_owned();
    if max_bytes >= PREVIEW_ELLIPSIS.len() {
        preview.push_str(PREVIEW_ELLIPSIS);
    }
    (preview, true)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn confirmation_for(guard: &mut PasteGuard, text: &str) -> PasteConfirmation {
        match guard
            .request(text.to_owned())
            .expect("bounded multiline paste should be accepted")
        {
            PasteRequest::ConfirmationRequired(confirmation) => confirmation,
            PasteRequest::Ready(_) => panic!("multiline paste must require confirmation"),
        }
    }

    #[test]
    fn cr_lf_and_crlf_all_require_confirmation() {
        for body in ["99\r100", "99\n100", "99\r\n100"] {
            let mut guard = PasteGuard::default();
            let confirmation = confirmation_for(&mut guard, body);
            assert_eq!(confirmation.preview(), body);
            assert!(guard.has_pending());
        }
    }

    #[test]
    fn canonical_preview_preserves_99_newline_100() {
        let mut guard = PasteGuard::default();
        let confirmation = confirmation_for(&mut guard, "99\n100");
        assert_eq!(confirmation.preview(), "99\n100");
        assert!(!confirmation.preview_truncated());
        assert_eq!(confirmation.body_bytes(), 6);
        assert_eq!(
            PasteConfirmationId::from_raw(confirmation.id().get()),
            Some(confirmation.id())
        );
        assert_eq!(PasteConfirmationId::from_raw(0), None);
    }

    #[test]
    fn multiline_request_writes_nothing_before_exact_once_approval() {
        let mut guard = PasteGuard::default();
        let confirmation = confirmation_for(&mut guard, "99\n100");
        let mut writes = Vec::new();
        assert!(writes.is_empty(), "requesting confirmation must not write");

        let approved = guard
            .approve(confirmation.id())
            .expect("matching confirmation should approve");
        writes.push(approved.into_text());
        assert_eq!(writes, ["99\n100"]);
        assert_eq!(
            guard.approve(confirmation.id()),
            Err(PasteConfirmationError::NoPendingPaste)
        );
        assert_eq!(writes, ["99\n100"], "approval cannot write twice");
    }

    #[test]
    fn cancellation_discards_the_retained_body() {
        let mut guard = PasteGuard::default();
        let confirmation = confirmation_for(&mut guard, "do\nnot paste");
        let writes: Vec<String> = Vec::new();
        guard
            .cancel(confirmation.id())
            .expect("matching confirmation should cancel");
        assert!(!guard.has_pending());
        assert!(writes.is_empty(), "cancellation must not release PTY input");
        assert_eq!(
            guard.approve(confirmation.id()),
            Err(PasteConfirmationError::NoPendingPaste)
        );
    }

    #[test]
    fn stale_confirmation_cannot_release_a_newer_clipboard_body() {
        let mut guard = PasteGuard::default();
        let stale = confirmation_for(&mut guard, "old\nbody");
        let current = confirmation_for(&mut guard, "new\nbody");
        assert_eq!(
            guard.approve(stale.id()),
            Err(PasteConfirmationError::StaleConfirmation {
                expected: current.id(),
                actual: stale.id()
            })
        );
        assert!(guard.has_pending());
        assert_eq!(
            guard
                .approve(current.id())
                .expect("current identity should release its retained body")
                .text(),
            "new\nbody"
        );
    }

    #[test]
    fn clipboard_body_and_preview_are_independently_bounded_on_utf8_boundaries() {
        let mut guard = PasteGuard::new(12, 6);
        let confirmation = confirmation_for(&mut guard, "éé\néé");
        assert!(confirmation.preview().len() <= 6);
        assert!(
            confirmation
                .preview()
                .is_char_boundary(confirmation.preview().len())
        );
        assert!(confirmation.preview_truncated());

        let rejection = guard
            .request("private\nbody!".to_owned())
            .expect_err("oversized clipboard text should be rejected");
        assert_eq!(
            rejection,
            PasteRejection::TooLarge {
                body_bytes: 13,
                max_bytes: 12
            }
        );
        assert!(
            guard.has_pending(),
            "rejection must retain the prior prompt"
        );
    }

    #[test]
    fn diagnostics_redact_clipboard_body_and_preview() {
        const SECRET: &str = "never-log-this\nor-this";
        let mut guard = PasteGuard::default();
        let confirmation = confirmation_for(&mut guard, SECRET);
        let confirmation_debug = format!("{confirmation:?}");
        let guard_debug = format!("{guard:?}");
        assert!(!confirmation_debug.contains("never-log-this"));
        assert!(!guard_debug.contains("never-log-this"));
        assert!(confirmation_debug.contains("<redacted>"));
        assert!(guard_debug.contains("<redacted>"));
    }

    #[test]
    fn default_clipboard_bound_is_enforced_without_disclosing_the_body() {
        let mut guard = PasteGuard::default();
        let secret = format!("do-not-log\n{}", "x".repeat(MAX_PASTE_BYTES));
        let rejection = guard
            .request(secret)
            .expect_err("body over the default clipboard bound must be rejected");
        assert_eq!(
            rejection,
            PasteRejection::TooLarge {
                body_bytes: MAX_PASTE_BYTES + 11,
                max_bytes: MAX_PASTE_BYTES
            }
        );
        assert!(!rejection.to_string().contains("do-not-log"));
        assert!(!format!("{rejection:?}").contains("do-not-log"));
        assert!(!guard.has_pending());
    }

    #[test]
    fn single_line_body_is_immediately_ready_and_redacted_in_debug() {
        let mut guard = PasteGuard::default();
        let request = guard
            .request("private single line".to_owned())
            .expect("bounded single line should be accepted");
        let debug = format!("{request:?}");
        assert!(!debug.contains("private single line"));
        match request {
            PasteRequest::Ready(approved) => assert_eq!(approved.text(), "private single line"),
            PasteRequest::ConfirmationRequired(_) => {
                panic!("single-line paste must not require confirmation")
            }
        }
    }
}
