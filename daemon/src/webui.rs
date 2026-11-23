//! Embedded WebUI assets.
//!
//! Three files, no framework, no CDN. Public assets contain no API credential.
//! The console asks the user for the root-owned token and sends it in a header.

pub const INDEX_HTML: &str = include_str!("../../webui/index.html");
pub const APP_JS: &str = include_str!("../../webui/app.js");
pub const STYLE_CSS: &str = include_str!("../../webui/style.css");

/// The KernelSU WebUI entry point: a thin launcher that embeds the real console.
pub const WEBROOT_HTML: &str = include_str!("../../module/webroot/index.html");

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn assets_are_present_and_self_contained() {
        assert!(!INDEX_HTML.contains("__NEUROSCHED_TOKEN__"));
        assert!(INDEX_HTML.contains("type=\"password\""));
        assert!(INDEX_HTML.contains("ACTUATOR_ENABLED=false"));
        assert!(!INDEX_HTML.contains("data-mode=\"enabled\""));
        assert!(!APP_JS.contains("?token="));
        assert!(!APP_JS.contains("localStorage"));
        assert!(APP_JS.contains("X-NeuroSched-Token"));
        assert!(INDEX_HTML.contains("<title>OryonPilot"));
        assert!(APP_JS.contains("api(\"status\")"));
        assert!(STYLE_CSS.contains("--bg"));
        for (name, body) in [
            ("index.html", INDEX_HTML),
            ("app.js", APP_JS),
            ("style.css", STYLE_CSS),
            ("webroot", WEBROOT_HTML),
        ] {
            assert!(
                !body.contains("http://cdn") && !body.contains("https://cdn"),
                "{name} must not reference a CDN"
            );
        }
    }
}
