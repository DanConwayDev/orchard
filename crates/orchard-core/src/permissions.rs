//! Translate flatpak `finish-args` into human-readable permission grants.
//!
//! There is no review step in Orchard, so the permissions viewer is the primary
//! safety surface: every listing shows exactly what an app can reach, with a
//! rough risk level, *before* install. Errors on the side of "review manually".

use serde::{Deserialize, Serialize};

/// Rough exposure level of a grant.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Risk {
    Low,
    Medium,
    High,
}

/// One human-readable grant, translated from a single `finish-args` entry.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Grant {
    /// The raw argument, e.g. `--socket=wayland`.
    pub arg: String,
    pub category: &'static str,
    pub summary: String,
    pub risk: Risk,
}

/// Translate a full `finish-args` array into displayable grants.
///
/// Unknown options are surfaced as `High` risk rather than hidden.
pub fn parse_finish_args(args: &[String]) -> Vec<Grant> {
    args.iter().map(parse_one).collect()
}

fn parse_one(arg: &str) -> Grant {
    if let Some(value) = arg.strip_prefix("--socket=") {
        let (summary, risk) = match value {
            "wayland" | "x11" | "fallback-x11" => ("Windowing system access", Risk::Low),
            "pulseaudio" => ("Audio playback/recording", Risk::Medium),
            "session-bus" => ("Session D-Bus (wide session access)", Risk::High),
            "system-bus" => ("System D-Bus (wide system access)", Risk::High),
            _ => ("Other socket access", Risk::Medium),
        };
        return grant(arg, "System", summary, risk);
    }
    if let Some(value) = arg.strip_prefix("--share=") {
        let (summary, risk) = match value {
            "network" => ("Full network access", Risk::Medium),
            "ipc" => ("Inter-process communication", Risk::Medium),
            _ => ("Other shared resource", Risk::Medium),
        };
        return grant(arg, "System", summary, risk);
    }
    if let Some(value) = arg.strip_prefix("--device=") {
        let (summary, risk) = match value {
            "dri" => ("GPU acceleration", Risk::Low),
            "kvm" => ("Kernel virtual machine access", Risk::High),
            "all" => ("All devices", Risk::High),
            _ => ("Device access", Risk::Medium),
        };
        return grant(arg, "Devices", summary, risk);
    }
    if let Some(value) = arg.strip_prefix("--filesystem=") {
        let (summary, risk) = match value {
            "host" | "host-os" | "host-etc" => ("Entire host filesystem", Risk::High),
            "home" => ("Home directory (read/write)", Risk::High),
            v if v.ends_with(":ro") => ("Filesystem path (read-only)", Risk::Medium),
            _ => ("Filesystem path (read/write)", Risk::High),
        };
        return grant(arg, "Files", summary, risk);
    }
    if let Some(name) = arg.strip_prefix("--talk-name=") {
        return grant(
            arg,
            "Services",
            &format!("D-Bus service access: {name}"),
            Risk::High,
        );
    }
    if let Some(name) = arg.strip_prefix("--own-name=") {
        return grant(
            arg,
            "Services",
            &format!("Owns D-Bus name: {name}"),
            Risk::Medium,
        );
    }
    if arg.starts_with("--allow=") {
        return grant(arg, "Sandbox", "Sandbox escape permission", Risk::High);
    }
    if arg.starts_with("--env=") {
        return grant(arg, "System", "Sets environment variable", Risk::Medium);
    }
    if arg.starts_with("--persist=") {
        return grant(arg, "Files", "Persistent home-subpath for app", Risk::Medium);
    }
    grant(
        arg,
        "Other",
        "Unrecognized option (review manually)",
        Risk::High,
    )
}

fn grant(arg: &str, category: &'static str, summary: &str, risk: Risk) -> Grant {
    Grant {
        arg: arg.to_string(),
        category,
        summary: summary.to_string(),
        risk,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn args(values: &[&str]) -> Vec<String> {
        values.iter().map(|s| s.to_string()).collect()
    }

    #[test]
    fn wayland_is_low_risk() {
        let grants = parse_finish_args(&args(&["--socket=wayland"]));
        assert_eq!(grants[0].risk, Risk::Low);
        assert_eq!(grants[0].category, "System");
    }

    #[test]
    fn host_filesystem_is_high_risk() {
        let grants = parse_finish_args(&args(&["--filesystem=host"]));
        assert_eq!(grants[0].risk, Risk::High);
        assert_eq!(grants[0].summary, "Entire host filesystem");
    }

    #[test]
    fn network_is_medium() {
        let grants = parse_finish_args(&args(&["--share=network"]));
        assert_eq!(grants[0].risk, Risk::Medium);
    }

    #[test]
    fn talk_name_is_dynamic_but_high() {
        let grants = parse_finish_args(&args(&["--talk-name=org.freedesktop.Notifications"]));
        assert_eq!(grants[0].risk, Risk::High);
        assert!(grants[0].summary.contains("org.freedesktop.Notifications"));
    }

    #[test]
    fn unknown_options_are_surfaced_as_high() {
        let grants = parse_finish_args(&args(&["--some-future-flag"]));
        assert_eq!(grants[0].risk, Risk::High);
        assert!(grants[0].summary.contains("Unrecognized"));
    }
}
