//! The settings file into `DesktopConfig::from_json_str`; a config that parses reads back equal
//! after it is written.
#![no_main]

use libfuzzer_sys::fuzz_target;
use thalvi_desktop::config::DesktopConfig;

fuzz_target!(|data: &[u8]| {
    let Ok(json) = std::str::from_utf8(data) else {
        return;
    };
    let Ok(config) = DesktopConfig::from_json_str(json) else {
        return;
    };
    let written = serde_json::to_string(&config).expect("a parsed config serializes");
    let reread = DesktopConfig::from_json_str(&written).expect("a written config parses");
    assert_eq!(reread, config, "{written}");
});
