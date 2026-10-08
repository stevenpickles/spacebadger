//! Typed, versioned commands and events exchanged between the Rust backend and
//! the webview interface.
//!
//! Every type here derives [`ts_rs::TS`]; `cargo test` regenerates the
//! TypeScript bindings in `ui/src/lib/protocol/`. CI fails if the committed
//! bindings differ from the generated ones.

use serde::{Deserialize, Serialize};
use ts_rs::TS;

/// Incremented whenever a command or event shape changes incompatibly.
pub const PROTOCOL_VERSION: u32 = 1;

/// Static information about the running backend.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, TS)]
#[serde(rename_all = "camelCase")]
#[ts(export)]
pub struct AppInfo {
    pub protocol_version: u32,
    pub app_version: String,
    pub os: String,
    pub arch: String,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn app_info_uses_camel_case() {
        let info = AppInfo {
            protocol_version: PROTOCOL_VERSION,
            app_version: "0.1.0".into(),
            os: "windows".into(),
            arch: "x86_64".into(),
        };
        let json = serde_json::to_value(&info).unwrap();
        assert_eq!(json["protocolVersion"], PROTOCOL_VERSION);
        assert_eq!(json["appVersion"], "0.1.0");
    }
}
