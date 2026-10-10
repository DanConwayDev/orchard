//! # orchard-flatpak
//!
//! The *only* place Orchard invokes the flatpak CLI. Keeping one seam means:
//!
//! - when Orchard itself runs inside a flatpak sandbox, every call is
//!   transparently wrapped in `flatpak-spawn --host` (the pattern Warehouse
//!   uses), because a sandboxed app cannot touch the host flatpak directly;
//! - commands are returned as [`Command`] builders so they are unit-testable
//!   without executing anything and the UI layer can add progress handling.

use std::path::Path;
use std::process::Command;

/// Name of the flatpak remote the local index shim backs.
pub const REMOTE_NAME: &str = "orchard";

/// True when Orchard itself runs inside a flatpak sandbox.
pub fn running_in_flatpak() -> bool {
    Path::new("/.flatpak-info").exists()
}

/// Base `flatpak` invocation, escaping the sandbox if necessary.
pub fn flatpak_command() -> Command {
    if running_in_flatpak() {
        let mut cmd = Command::new("flatpak-spawn");
        cmd.arg("--host").arg("flatpak");
        cmd
    } else {
        Command::new("flatpak")
    }
}

/// Register the local index shim (`orchard-indexd`) as an OCI remote.
///
/// `index_url` looks like `oci+http://127.0.0.1:7780`. See
/// `flatpak-remote(5)`: `oci+http(s)://` URLs point at an index of OCI images
/// in a container registry rather than an ostree repo.
pub fn ensure_oci_remote(index_url: &str) -> Command {
    let mut cmd = flatpak_command();
    cmd.args(["remote-add", "--oci", "--user", REMOTE_NAME, index_url]);
    cmd
}

/// Install an app ref (e.g. `app/com.example.Hello/x86_64/stable`) from a remote.
pub fn install(app_ref: &str, remote: &str) -> Command {
    let mut cmd = flatpak_command();
    cmd.args(["install", "--user", "-y", remote, app_ref]);
    cmd
}

/// Direct-install an OCI image without a remote (flatpak >= 1.17).
///
/// Fallback path only: updates lack remote integration, so prefer [`install`]
/// via the shim in the normal flow. Syntax is a TODO to confirm in M0.
pub fn install_from_oci(uri: &str) -> Command {
    let mut cmd = flatpak_command();
    cmd.args(["install", "--user", "-y", uri]);
    cmd
}

/// Update everything the `orchard` remote serves (plus whatever else is stale).
pub fn update() -> Command {
    let mut cmd = flatpak_command();
    cmd.args(["update", "--user", "-y"]);
    cmd
}

#[cfg(test)]
mod tests {
    use super::*;

    fn arg_strings(cmd: &Command) -> Vec<String> {
        cmd.get_args()
            .map(|a| a.to_string_lossy().into_owned())
            .collect()
    }

    #[test]
    fn install_builds_expected_invocation() {
        let cmd = install("app/com.example.Hello/x86_64/stable", REMOTE_NAME);
        let args = arg_strings(&cmd);
        // Ends with the install arguments regardless of sandbox wrapping.
        let tail: Vec<String> = args
            .iter()
            .skip(args.len() - 5)
            .cloned()
            .collect();
        assert_eq!(
            tail,
            ["install", "--user", "-y", "orchard", "app/com.example.Hello/x86_64/stable"]
        );
    }

    #[test]
    fn remote_add_is_oci() {
        let cmd = ensure_oci_remote("oci+http://127.0.0.1:7780");
        let args = arg_strings(&cmd);
        assert!(args.contains(&"--oci".to_string()));
        assert!(args.contains(&"oci+http://127.0.0.1:7780".to_string()));
        assert!(args.contains(&REMOTE_NAME.to_string()));
    }
}
