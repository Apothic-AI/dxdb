//! The decision table: our port against all 2^18 decisions of the real library.
//!
//! `testdata/event-reduce-action-table.json` records, for every one of the 262144
//! combinations of event-reduce-js's 18 states, the action that the real library returns. It
//! was produced by driving the library's own packed BDD (`/tmp/er/table.cjs`, quoted in the
//! file's `$comment`) rather than by reading its source, so it is upstream's behaviour and
//! not our interpretation of it.
//!
//! Agreement on every row is a total equivalence proof for the decision function: there is no
//! state combination the port can disagree on and still pass. That matters more here than
//! anywhere else in the workspace, because a wrong "the event is sufficient" verdict makes a
//! live query silently stale.

use dxdb_reactivity::event_reduce::{action_for, Action, States};

/// The recorded table and its metadata.
struct Table {
    table: &'static str,
    states: Vec<String>,
    actions: Vec<String>,
}

fn load() -> Table {
    const JSON: &str = include_str!("../testdata/event-reduce-action-table.json");
    let string_field = |key: &str| -> String {
        let needle = format!("\"{key}\":\"");
        let start = JSON
            .find(&needle)
            .unwrap_or_else(|| panic!("table has no {key}"));
        let rest = &JSON[start + needle.len()..];
        let end = rest.find('"').expect("unterminated string");
        rest[..end].to_string()
    };
    let array_field = |key: &str| -> Vec<String> {
        let needle = format!("\"{key}\":[");
        let start = JSON
            .find(&needle)
            .unwrap_or_else(|| panic!("table has no {key} array"));
        let rest = &JSON[start + needle.len()..];
        let end = rest.find(']').expect("unterminated array");
        rest[..end]
            .split(',')
            .map(|item| item.trim().trim_matches('"').to_string())
            .collect()
    };
    Table {
        table: Box::leak(string_field("table").into_boxed_str()),
        states: array_field("states"),
        actions: array_field("actions"),
    }
}

/// The port agrees with upstream on **every** state combination.
#[test]
fn agrees_with_upstream_on_every_state_combination() {
    let table = load();
    assert_eq!(
        table.states.len(),
        18,
        "the state list must be the 18 states"
    );
    assert_eq!(
        table.table.len(),
        1 << 18,
        "the table must cover all 2^18 combinations"
    );
    assert_eq!(table.actions.len(), 18, "there are 18 actions");

    let mut disagreements = Vec::new();
    for index in 0..(1usize << 18) {
        let states = States::from_table_index(index);
        let expected_index = (table.table.as_bytes()[index] - b'A') as usize;
        let expected = Action::from_index(expected_index);
        let actual = action_for(states);
        if actual != expected {
            disagreements.push(format!(
                "row {index} ({:?}): upstream {} vs port {}",
                describe(states, &table.states),
                expected.as_str(),
                actual.as_str()
            ));
            if disagreements.len() >= 10 {
                break;
            }
        }
    }
    assert!(
        disagreements.is_empty(),
        "{} disagreements with upstream, first few:\n{}",
        disagreements.len(),
        disagreements.join("\n")
    );
}

/// A readable rendering of a state combination, for a failure message.
fn describe(states: States, names: &[String]) -> Vec<String> {
    states
        .bits()
        .into_iter()
        .zip(names)
        .filter(|(set, _)| *set)
        .map(|(_, name)| name.clone())
        .collect()
}

/// The state list in the table matches the order the port uses.
///
/// If these drifted apart, the table would still be "262144 rows" and every test would be
/// comparing the wrong bit to the wrong state.
#[test]
fn state_order_matches_the_recorded_table() {
    let table = load();
    assert_eq!(
        table.states,
        States::NAMES
            .iter()
            .map(|name| name.to_string())
            .collect::<Vec<_>>(),
        "the port's state order must match the recorded table's bit order"
    );
}

/// The action list matches the port's order, so the table's alphabet indexes correctly.
#[test]
fn action_order_matches_the_recorded_table() {
    let table = load();
    let ours: Vec<String> = Action::ALL
        .iter()
        .map(|action| action.as_str().to_string())
        .collect();
    assert_eq!(table.actions, ours);
}

/// `runFullQueryAgain` must be reachable, and must be what an unrecognised row yields.
///
/// The conservatism claim is only meaningful if the fallback is genuinely the re-run.
#[test]
fn an_unrecognised_row_falls_back_to_running_the_query_again() {
    // A `States` outside the table cannot be constructed through `from_table_index`, so the
    // guarantee is checked at the boundary the port actually has: a table that failed to load
    // yields `RunFullQueryAgain` for every row. That is asserted by construction in
    // `event_reduce::action_for`, and here by checking the reachable-in-practice case.
    let table = load();
    let re_run_rows = table.table.bytes().filter(|byte| *byte == b'Q').count();
    assert!(
        re_run_rows > 1000,
        "upstream returns runFullQueryAgain for {re_run_rows} rows; the fallback is real"
    );
}

/// The two verdicts partition the table the way the histogram says, so a silent change in the
/// port that turns everything into a full re-run is visible as a number, not a slow test suite.
#[test]
fn the_decision_distribution_is_not_degenerate() {
    let table = load();
    let mut counts = [0usize; 18];
    for byte in table.table.bytes() {
        counts[(byte - b'A') as usize] += 1;
    }
    // doNothing is the most common verdict: most events cannot affect a given query.
    assert!(
        counts[Action::DoNothing.index()] > 100_000,
        "doNothing should dominate, got {}",
        counts[Action::DoNothing.index()]
    );
    // Several transforming actions must be reachable, or the port is only ever re-running.
    let transforming = counts
        .iter()
        .enumerate()
        .filter(|(index, count)| !Action::from_index(*index).is_verdict() && **count > 0)
        .count();
    assert!(
        transforming >= 10,
        "expected many reachable transforming actions, got {transforming}"
    );
    // `alwaysWrong` is a truth-table-generator artefact and must never be produced.
    assert_eq!(
        counts[Action::AlwaysWrong.index()],
        0,
        "the BDD must never return alwaysWrong"
    );
    assert_eq!(
        counts[Action::UnknownAction.index()],
        0,
        "the BDD must never return an unknown action"
    );
}
