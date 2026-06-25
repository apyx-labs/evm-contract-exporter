use evm_contract_exporter::config;
use std::path::Path;

#[test]
fn example_config_loads_and_validates_offline() {
    unsafe { std::env::set_var("ETH_RPC_URL", "http://localhost:8545") };
    let cfg =
        config::load(Path::new("examples/chainlink-eth-usd.yaml")).expect("load+validate offline");
    assert_eq!(cfg.chain.chain_id, 1);
    assert_eq!(cfg.contracts.len(), 1);
    assert_eq!(cfg.contracts[0].name, "chainlink_eth_usd");
}
