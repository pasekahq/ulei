// Port: THIRD_PARTY_NOTICES.md#ref-agent
//! The end of a turn at the step limit: one request without tools for a final summary, sent
//! again once when its reply is empty.

use ulei_contracts::generated::c4g::Role;

use super::{Agent, Ended, Kind, Live, Sleeper, Step, text, text_message, think};
use crate::tools::{Interrupt, py_strip};

/// The user message asking for the summary.
const MAX_ITERATIONS_SUMMARY_REQUEST: &str = "You've reached the maximum number of tool-calling iterations allowed. Please provide a final response summarizing what you've found and accomplished so far, without calling any more tools.";
/// The answer when the summary has no text.
const NO_SUMMARY: &str = "I reached the iteration limit and couldn't generate a summary.";
/// Requests for the summary when the first reply is empty.
const SUMMARY_TRIES: usize = 2;

impl Agent<'_> {
    /// Appends the summary request and the answer: the summary, or the reference text for a
    /// summary without text or a failed request. An interrupt or an exhausted quota ends the
    /// turn as on any other step.
    pub(super) fn summarize(
        &self,
        live: &mut Live,
        steps: &mut Vec<Step>,
        request_id: &dyn Fn(usize) -> String,
        interrupt: &Interrupt,
        sleeper: &mut dyn Sleeper,
        max_steps: usize,
    ) -> Ended {
        live.push(
            text_message(Role::User, MAX_ITERATIONS_SUMMARY_REQUEST),
            Kind::Kept,
        );
        let mut request = live.sent().into_owned();
        request.tools = None;
        let mut answer = NO_SUMMARY.to_owned();
        for _ in 0..SUMMARY_TRIES {
            request.request_id = request_id(steps.len());
            let mut step = Step::new(request.request_id.clone());
            let reply = self.answer(&request, interrupt, sleeper, &mut step);
            steps.push(step);
            match reply {
                Ok(reply) => {
                    let content = text(&reply.message);
                    let content = py_strip(&content);
                    if content.is_empty() {
                        continue;
                    }
                    let summary = think::strip_think_pairs(content);
                    let summary = py_strip(&summary);
                    if !summary.is_empty() {
                        summary.clone_into(&mut answer);
                    }
                    break;
                }
                Err(ended @ (Ended::Interrupted | Ended::QuotaExhausted { .. })) => return ended,
                Err(ended) => {
                    answer = format!(
                        "I reached the maximum iterations ({max_steps}) but couldn't summarize. Error: {ended}"
                    );
                    break;
                }
            }
        }
        live.push(text_message(Role::Assistant, &answer), Kind::Kept);
        Ended::MaxSteps
    }
}
