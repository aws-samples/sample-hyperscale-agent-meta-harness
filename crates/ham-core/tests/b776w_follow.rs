//! follow-up (limits audit row D09): the words of the refusal that twenty waiting messages cause name the number that the protocol
//! constant holds.

#![allow(clippy::unwrap_used)]

use ham_core::messages::explain_refusal;
use harness_protocol::messages::MAX_OUTSTANDING_MESSAGES;

#[test]
fn b776w_the_too_many_pending_refusal_names_the_number_of_the_constant() {
    assert_eq!(MAX_OUTSTANDING_MESSAGES, 20);
    let said = explain_refusal(409, "too-many-pending", "the server's words");
    assert_eq!(
        said.text,
        "20 messages are waiting for Cline already; wait for one to be delivered or withdraw one"
    );
}
