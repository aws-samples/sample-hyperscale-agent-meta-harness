//! A fault of the machine is not a fault of the model provider.
//!
//! A session can have an engine that answers the bare error -32603 `Internal error` on every route,
//! routes it has never used included, while every small test request to every route answers healthy and Bedrock records no error and no
//! throttle. The session's file system has stopped taking writes. Route failover refuses the routes in a pass, waits, and walks them again
//! several times before it ends under the label that says the model provider failed.
//!
//! The signature is what a provider fault cannot produce: during one walk over the routes after a failure, every route whose small test
//! request was answered then refused the switch with the same bare engine error, and no route did anything else. [`Tally`] counts what the walk
//! saw; the retry loop ends the run at the first time every usable route has been walked ([`Next::AllDown`], pass 1) when
//! [`Tally::is_local_fault`] holds and the failure that began the walk was the same bare error. The cause is then
//! [`crate::direct::disk::Limits::engine_fault`], and its label says what was seen.
//!
//! [`Next::AllDown`]: crate::direct::failover::Next::AllDown

/// True when `text` is the display of the engine's bare internal error: `engine error -32603: Internal error` and nothing after it. An
/// error that names a provider (`Internal error: Service unavailable`, a status, a throttle) is not bare.
pub fn bare_internal_error(text: &str) -> bool {
    text.trim()
        .strip_prefix("engine error -32603:")
        .is_some_and(|message| message.trim().eq_ignore_ascii_case("internal error"))
}

/// What one walk over the routes saw, after one failed prompt.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Tally {
    local: u32,
    other: u32,
}

impl Tally {
    /// A route whose small test request was answered refused the switch with the bare engine error.
    pub fn local_refusal(&mut self) {
        self.local += 1;
    }

    /// Anything else the walk met: a probe that did not answer or was refused, a switch the engine took, a refusal with another text,
    /// a walk with no probe to ask.
    pub fn other(&mut self) {
        self.other += 1;
    }

    /// At least one route refused the switch with the bare error after a healthy probe, and nothing else happened.
    pub fn is_local_fault(&self) -> bool {
        self.local > 0 && self.other == 0
    }

    /// How many routes refused the switch that way.
    pub fn routes(&self) -> u32 {
        self.local
    }
}
