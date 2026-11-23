//! The `forge_accounts` response stays readable in both directions while
//! Linear's capabilities ride in their own field.
//!
//! A client built before this change expects `capabilities` to name only forge
//! kinds, so that list keeps its old shape and an absent `linear` field simply
//! means an older daemon.

use h4rn_protocol::{
    ForgeCapabilities, ForgeCapabilityEntry, ForgeIssueCapabilities, ForgeKind, ResponseBody,
};
use serde::Deserialize;

/// The `forge_accounts` shape before Linear was reported: every row is a
/// forge, and there is no `linear` field.
#[derive(Debug, Deserialize)]
struct OldForgeAccounts {
    accounts: Vec<h4rn_protocol::ForgeAccount>,
    capabilities: Vec<OldCapabilityEntry>,
}

#[derive(Debug, Deserialize)]
struct OldCapabilityEntry {
    forge: ForgeKind,
    capabilities: ForgeCapabilities,
}

fn capabilities(read: bool) -> ForgeCapabilities {
    ForgeCapabilities {
        device_flow: false,
        review_threads: false,
        resolve_threads: false,
        nested_replies: false,
        inline_review_comments: false,
        checks: false,
        pipelines: false,
        diff_pagination: false,
        graphql: false,
        rerun_checks: false,
        conditional_poll: false,
        issues: ForgeIssueCapabilities {
            read,
            ..ForgeIssueCapabilities::default()
        },
    }
}

#[test]
fn an_older_client_reads_a_current_payload() {
    let response = ResponseBody::ForgeAccounts {
        accounts: Vec::new(),
        capabilities: vec![ForgeCapabilityEntry {
            forge: ForgeKind::GitHub,
            capabilities: capabilities(true),
        }],
        linear: Some(capabilities(true)),
    };
    let json = serde_json::to_value(&response).expect("serialize");
    let old: OldForgeAccounts =
        serde_json::from_value(json).expect("the previous shape reads today's payload");
    assert_eq!(old.capabilities.len(), 1);
    assert_eq!(old.capabilities[0].forge, ForgeKind::GitHub);
    assert!(old.capabilities[0].capabilities.issues.read);
    assert!(old.accounts.is_empty());
}

#[test]
fn a_payload_without_linear_parses_and_reports_it_absent() {
    let response: ResponseBody = serde_json::from_value(serde_json::json!({
        "type": "forge_accounts",
        "accounts": [],
        "capabilities": []
    }))
    .expect("an older daemon's payload parses");
    match response {
        ResponseBody::ForgeAccounts { linear, .. } => assert!(linear.is_none()),
        other => panic!("unexpected response: {other:?}"),
    }
}

#[test]
fn linear_is_omitted_when_there_is_nothing_to_report() {
    let response = ResponseBody::ForgeAccounts {
        accounts: Vec::new(),
        capabilities: Vec::new(),
        linear: None,
    };
    let json = serde_json::to_value(&response).expect("serialize");
    assert!(
        json.get("linear").is_none(),
        "an absent Linear row leaves the old payload untouched: {json}"
    );
}

#[test]
fn linear_capabilities_round_trip_in_their_own_field() {
    let response = ResponseBody::ForgeAccounts {
        accounts: Vec::new(),
        capabilities: Vec::new(),
        linear: Some(capabilities(true)),
    };
    let json = serde_json::to_value(&response).expect("serialize");
    let back: ResponseBody = serde_json::from_value(json).expect("parse");
    match back {
        ResponseBody::ForgeAccounts { linear, .. } => {
            assert!(linear.expect("a Linear row").issues.read);
        }
        other => panic!("unexpected response: {other:?}"),
    }
}
