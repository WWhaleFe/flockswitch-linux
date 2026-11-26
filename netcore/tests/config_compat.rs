use flock_net::{NetConfig, Os, PeerConfig, Side};
use serde::{Deserialize, Serialize};

// The app embeds the same defaulted network field alongside its existing settings.
#[derive(Default, Serialize, Deserialize)]
#[serde(default)]
struct AppConfig {
    enabled: bool,
    network: NetConfig,
    edges: Edges,
}
#[derive(Default, Serialize, Deserialize)]
#[serde(default)]
struct Edges {
    on: bool,
    right: u8,
}

#[test]
fn legacy_toml_defaults_and_network_round_trip() {
    let mut config: AppConfig =
        toml::from_str("enabled = true\n[edges]\non = true\nright = 2\n").unwrap();
    assert!(config.enabled && config.edges.on);
    assert_eq!(config.edges.right, 2);
    assert_eq!(config.network, NetConfig::default());
    config.network.on = true;
    config.network.peers.push(PeerConfig {
        os: Os::Windows,
        side: Some(Side::Right),
        logitech_channel: 2,
        ..Default::default()
    });
    let serialized = toml::to_string(&config).unwrap();
    let loaded: AppConfig = toml::from_str(&serialized).unwrap();
    assert_eq!(loaded.network, config.network);
    assert_eq!(loaded.edges.right, 2);
    assert!(!serialized.contains("config_dir"));
}
