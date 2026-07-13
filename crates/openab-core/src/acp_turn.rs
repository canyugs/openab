//! Typed results for a single ACP prompt turn.
//!
//! These types deliberately keep execution, canonical platform delivery, and
//! the legacy dispatcher return value separate. An ACP turn may have completed
//! successfully even when its final Discord or Slack message could not be
//! delivered, while an interrupted turn may have produced partial output
//! without providing proof that execution completed.

use crate::acp::TurnResult;
use uuid::Uuid;

/// Finalized text produced by an ACP turn.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct TurnOutput {
    /// Canonical, directive-applied text used for platform delivery.
    pub display_text: String,
    /// Directive-free text that may be considered for speech playback.
    ///
    /// Possessing this text alone does not make it eligible for playback. Use
    /// [`TurnCompletion::eligible_speech_text`] so execution and delivery state
    /// are checked as well.
    pub speech_text: String,
    /// Agent-authored, bounded spoken brief, when the originating prompt
    /// requested one through the `voice_summary` output contract.
    ///
    /// This is deliberately separate from `speech_text`: arbitrary final ACP
    /// output may contain code, logs, URLs, or other content that must never be
    /// read aloud as an implicit fallback.
    pub voice_brief: Option<String>,
}

/// The latest lifecycle state observed for an ACP tool call.
#[derive(Clone, Debug, PartialEq, Eq)]
#[non_exhaustive]
pub enum ObservedToolCallStatus {
    /// The tool call was announced, but no terminal update was observed.
    Started,
    /// ACP reported that the tool call completed.
    Completed,
    /// ACP reported that the tool call failed.
    Failed,
}

/// Evidence that an ACP tool call was observed during the turn.
///
/// This is evidence of what the client observed, not proof that a side effect
/// did or did not happen. In particular, a `Started` call followed by EOF must
/// be treated conservatively.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ObservedToolCall {
    /// ACP's stable tool-call identifier, when the agent supplied one.
    pub id: String,
    /// The latest human-readable title supplied for the tool call.
    pub title: String,
    /// The latest lifecycle state observed by OpenAB.
    pub status: ObservedToolCallStatus,
}

/// Opaque identity of one active `session/prompt` request.
///
/// Both components are required so a cancellation intended for one connection
/// cannot accidentally target a reused JSON-RPC request ID on another
/// connection. Only ACP connection code may mint tickets.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct TurnTicket {
    connection_id: Uuid,
    request_id: u64,
}

impl TurnTicket {
    /// Mint a ticket for an active request on a specific ACP connection.
    pub(crate) const fn new(connection_id: Uuid, request_id: u64) -> Self {
        Self {
            connection_id,
            request_id,
        }
    }

    /// Return the ACP connection generation that owns the request.
    pub const fn connection_id(&self) -> Uuid {
        self.connection_id
    }

    /// Return the JSON-RPC request ID within the owning connection.
    pub const fn request_id(&self) -> u64 {
        self.request_id
    }
}

/// Failure while starting `session/prompt`, classified by whether any request
/// bytes may have reached the ACP child.
#[derive(Clone, Debug, PartialEq, Eq)]
#[non_exhaustive]
pub enum TurnStartError {
    /// Validation or serialization failed before a write was attempted.
    NotStarted {
        /// Error text preserved for the legacy dispatcher path.
        error: String,
    },
    /// A stdin write or flush failed after bytes may have reached the child.
    /// The owning connection is poisoned and must not be reused.
    WriteIndeterminate {
        /// Identity reserved for the possibly-started request.
        ticket: TurnTicket,
        /// Error text preserved for the legacy dispatcher path.
        error: String,
    },
}

/// Opaque proof, supplied by a containment backend, that a failed turn could
/// not have produced any side effects.
///
/// An error response or missing final response is not such proof. The private
/// representation prevents callers outside `openab-core` from manufacturing a
/// proof and incorrectly downgrading an unknown outcome to [`ExecutionOutcome::Failed`].
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct NoEffectsProof {
    _private: (),
}

impl NoEffectsProof {
    /// Create proof after an in-crate containment backend has established that
    /// the turn could not have produced an effect.
    #[allow(dead_code)]
    pub(crate) const fn new() -> Self {
        Self { _private: () }
    }
}

/// Why OpenAB cannot determine a prompt turn's terminal execution outcome.
#[derive(Clone, Debug, PartialEq, Eq)]
#[non_exhaustive]
pub enum UnknownReason {
    /// A legacy `DispatchTarget` completed successfully but cannot expose
    /// structured execution or delivery evidence.
    LegacyDispatchTarget,
    /// ACP returned an error after the prompt may already have started work.
    AcpError {
        /// A pre-redacted error suitable for logs and user-visible audit text.
        error: String,
    },
    /// The ACP reader observed EOF or an unexpected agent process exit before
    /// receiving the prompt's final response.
    AgentExited,
    /// A liveness check determined that the agent process had died.
    AgentDied,
    /// The prompt exceeded its hard execution ceiling.
    HardTimeout {
        /// Configured hard timeout in seconds.
        seconds: u64,
    },
    /// A matching final response arrived without a result object.
    MissingResult,
    /// The turn ended without an authoritative final output.
    MissingOutput,
    /// OpenAB cannot establish whether the complete prompt request reached the
    /// child process.
    PromptWriteIndeterminate {
        /// A pre-redacted description of the write failure.
        error: String,
    },
    /// OpenAB could not establish whether the agent received a targeted
    /// cancellation notification. The owning process tree is terminated and
    /// the connection is poisoned, but effects before termination remain
    /// unknown.
    CancelWriteIndeterminate,
    /// Delivery initialization failed after the prompt became active, leaving
    /// the execution result unconfirmed.
    DeliverySetupFailure {
        /// A pre-redacted description of the setup failure.
        error: String,
    },
}

/// The execution outcome of one ACP prompt, independent of message delivery.
#[derive(Clone, Debug, PartialEq, Eq)]
#[non_exhaustive]
pub enum ExecutionOutcome {
    /// ACP returned a successful final prompt result.
    Succeeded { turn_result: TurnResult },
    /// Execution failed and a containment backend proved no effects occurred.
    Failed {
        /// A pre-redacted execution error.
        error: String,
        /// Unforgeable-by-consumers proof that failure is safe to classify.
        no_effects: NoEffectsProof,
    },
    /// Cancellation was confirmed as the terminal result.
    Cancelled {
        /// ACP metadata from the final response that confirmed cancellation.
        turn_result: TurnResult,
        /// Tool calls observed before cancellation became terminal.
        observed_tool_calls: Vec<ObservedToolCall>,
    },
    /// Work may have happened, but no authoritative terminal result was
    /// observed.
    OutcomeUnknown {
        /// The condition that made the terminal outcome unknowable.
        reason: UnknownReason,
        /// Tool-call evidence observed before the outcome became unknown.
        observed_tool_calls: Vec<ObservedToolCall>,
    },
}

/// Canonical platform delivery state, independent of ACP execution.
#[derive(Clone, Debug, PartialEq, Eq)]
#[non_exhaustive]
pub enum DeliveryOutcome {
    /// No canonical result delivery was attempted.
    NotAttempted,
    /// The canonical final result was delivered successfully.
    Delivered,
    /// Canonical delivery failed.
    Failed {
        /// A pre-redacted platform error safe for audit output.
        error: String,
        /// Whether the platform may already contain part of the canonical text.
        partially_delivered: bool,
    },
}

/// The exact success/error disposition expected by the pre-refactor text path.
///
/// Normal text dispatch maps only this field back into its historical
/// `anyhow::Result<()>`; it must not reinterpret typed execution or delivery
/// outcomes and thereby change reactions or fallback messages.
#[derive(Clone, Debug, PartialEq, Eq)]
#[non_exhaustive]
pub enum LegacyDispatchDisposition {
    /// The legacy dispatcher would have returned `Ok(())`.
    Succeeded,
    /// The legacy dispatcher would have returned an error.
    Failed {
        /// Error text preserved for the historical dispatcher path.
        error: String,
    },
}

impl LegacyDispatchDisposition {
    /// Convert this compatibility disposition into the legacy return type.
    pub fn into_result(self) -> anyhow::Result<()> {
        match self {
            Self::Succeeded => Ok(()),
            Self::Failed { error } => Err(anyhow::anyhow!(error)),
        }
    }
}

/// Typed completion of one active ACP prompt turn.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct TurnCompletion {
    /// The connection-qualified prompt identity used for targeted cancellation.
    pub ticket: TurnTicket,
    /// What is known about ACP execution.
    pub execution: ExecutionOutcome,
    /// Final or partial text retained from the turn, when available.
    pub output: Option<TurnOutput>,
    /// Whether canonical platform result delivery succeeded.
    pub delivery: DeliveryOutcome,
    /// Compatibility result for the existing text dispatcher.
    pub legacy_dispatch: LegacyDispatchDisposition,
}

impl TurnCompletion {
    /// Return speech text only when it is safe to enqueue action playback.
    ///
    /// Speech is eligible only after ACP execution succeeded, canonical text
    /// delivery succeeded, and the finalized speech projection is non-empty.
    pub fn eligible_speech_text(&self) -> Option<&str> {
        if !matches!(self.execution, ExecutionOutcome::Succeeded { .. })
            || self.delivery != DeliveryOutcome::Delivered
        {
            return None;
        }

        let speech_text = self.output.as_ref()?.speech_text.as_str();
        (!speech_text.trim().is_empty()).then_some(speech_text)
    }

    /// Return the explicit Voice Brief only after successful execution and
    /// canonical text delivery. Full ACP output is never used as a fallback.
    pub fn eligible_voice_brief(&self) -> Option<&str> {
        if !matches!(self.execution, ExecutionOutcome::Succeeded { .. })
            || self.delivery != DeliveryOutcome::Delivered
        {
            return None;
        }

        let brief = self.output.as_ref()?.voice_brief.as_deref()?;
        (!brief.trim().is_empty()).then_some(brief)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ticket() -> TurnTicket {
        TurnTicket::new(
            Uuid::parse_str("75d08a62-5142-4e11-8773-a2a3ac310dd3").unwrap(),
            42,
        )
    }

    fn turn_result() -> TurnResult {
        TurnResult {
            stop_reason: Some("end_turn".to_string()),
            input_tokens: Some(7),
            output_tokens: Some(11),
            total_tokens: Some(18),
        }
    }

    fn successful_completion(speech_text: &str) -> TurnCompletion {
        TurnCompletion {
            ticket: ticket(),
            execution: ExecutionOutcome::Succeeded {
                turn_result: turn_result(),
            },
            output: Some(TurnOutput {
                display_text: "canonical result".to_string(),
                speech_text: speech_text.to_string(),
                voice_brief: Some("Short spoken result.".to_string()),
            }),
            delivery: DeliveryOutcome::Delivered,
            legacy_dispatch: LegacyDispatchDisposition::Succeeded,
        }
    }

    #[test]
    fn turn_ticket_identity_includes_connection_and_request() {
        let first = ticket();
        let other_connection = TurnTicket::new(Uuid::new_v4(), first.request_id());
        let other_request = TurnTicket::new(first.connection_id(), first.request_id() + 1);

        assert_eq!(
            first.connection_id(),
            Uuid::parse_str("75d08a62-5142-4e11-8773-a2a3ac310dd3").unwrap()
        );
        assert_eq!(first.request_id(), 42);
        assert_ne!(first, other_connection);
        assert_ne!(first, other_request);
    }

    #[test]
    fn legacy_disposition_preserves_old_result_shape() {
        assert!(LegacyDispatchDisposition::Succeeded.into_result().is_ok());

        let error = LegacyDispatchDisposition::Failed {
            error: "historical failure".to_string(),
        }
        .into_result()
        .unwrap_err();
        assert_eq!(error.to_string(), "historical failure");
    }

    #[test]
    fn succeeded_and_delivered_output_is_speech_eligible() {
        let completion = successful_completion("Task completed.");
        assert_eq!(completion.eligible_speech_text(), Some("Task completed."));
    }

    #[test]
    fn speech_requires_successful_execution() {
        let mut completion = successful_completion("Do not speak this.");
        completion.execution = ExecutionOutcome::OutcomeUnknown {
            reason: UnknownReason::AgentExited,
            observed_tool_calls: vec![ObservedToolCall {
                id: "tool-1".to_string(),
                title: "Edit file".to_string(),
                status: ObservedToolCallStatus::Started,
            }],
        };

        assert_eq!(completion.eligible_speech_text(), None);
    }

    #[test]
    fn confirmed_cancellation_retains_turn_metadata() {
        let result = turn_result();
        let outcome = ExecutionOutcome::Cancelled {
            turn_result: result.clone(),
            observed_tool_calls: Vec::new(),
        };

        assert_eq!(
            outcome,
            ExecutionOutcome::Cancelled {
                turn_result: result,
                observed_tool_calls: Vec::new(),
            }
        );
    }

    #[test]
    fn speech_requires_canonical_delivery() {
        let mut completion = successful_completion("Do not speak this.");
        completion.delivery = DeliveryOutcome::Failed {
            error: "platform unavailable".to_string(),
            partially_delivered: true,
        };

        assert_eq!(completion.eligible_speech_text(), None);
        assert!(matches!(
            completion.execution,
            ExecutionOutcome::Succeeded { .. }
        ));
    }

    #[test]
    fn empty_or_missing_speech_is_not_eligible() {
        assert_eq!(successful_completion("  \n").eligible_speech_text(), None);

        let mut completion = successful_completion("unused");
        completion.output = None;
        assert_eq!(completion.eligible_speech_text(), None);
    }

    #[test]
    fn voice_brief_is_explicit_and_never_falls_back_to_full_speech_text() {
        let mut completion = successful_completion("A very long full answer with code.");
        assert_eq!(
            completion.eligible_voice_brief(),
            Some("Short spoken result.")
        );

        completion.output.as_mut().unwrap().voice_brief = None;
        assert_eq!(completion.eligible_voice_brief(), None);
        assert_eq!(
            completion.eligible_speech_text(),
            Some("A very long full answer with code.")
        );
    }

    #[test]
    fn failed_execution_requires_no_effects_proof() {
        let outcome = ExecutionOutcome::Failed {
            error: "sandbox rejected spawn".to_string(),
            no_effects: NoEffectsProof::new(),
        };

        assert!(matches!(outcome, ExecutionOutcome::Failed { .. }));
    }
}
