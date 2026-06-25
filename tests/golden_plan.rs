use std::collections::BTreeSet;
use std::path::Path;

use evm_contract_exporter::config;
use evm_contract_exporter::planner;
use prometheus::Registry;

#[test]
fn golden_plan_is_stable() {
    unsafe { std::env::set_var("GOLDEN_RPC_URL", "http://localhost:8545") };
    let cfg = config::load(Path::new("tests/fixtures/golden.yaml")).expect("load");
    let reg = Registry::new();
    let plan = planner::build(&cfg, &reg).expect("plan");

    // 1 decimals + 1 latestRoundData + 2 balanceOf calls = 4 unique calls.
    assert_eq!(plan.calls.len(), 4);
    assert_eq!(plan.entries.len(), 4);

    let names: BTreeSet<String> = plan.entries.iter().map(|e| e.metric_name.clone()).collect();
    // Multi-output function with an explicit output `name` appends to the
    // function-derived base (Go InferMetricNames §4.3), so the answer series is
    // `feed_latest_round_data_answer`, not `feed_answer`.
    let expected: BTreeSet<String> = [
        "feed_decimals",
        "feed_latest_round_data_answer",
        "feed_balance_of",
    ]
    .iter()
    .map(|s| s.to_string())
    .collect();
    assert_eq!(names, expected);

    let bo: Vec<_> = plan
        .entries
        .iter()
        .filter(|e| e.metric_name == "feed_balance_of")
        .collect();
    assert_eq!(bo.len(), 2);
    for e in bo {
        assert_eq!(
            e.gauge_label_keys(),
            vec!["chain", "holder", "pair", "address", "chain_id"]
        );
    }
}
