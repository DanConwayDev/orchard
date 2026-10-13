//! The *only* place Orchard invokes the flatpak CLI. Keeping one seam means:
//!
//! - when Orchard itself runs inside a flatpak sandbox, every call is
//!   transparently wrapped in `flatpak-spawn --host` (the pattern Warehouse
//!   uses), because a sandboxed app cannot touch the host flatpak directly;
//! - commands are returned as [`Command`] builders so they are unit-testable
//!   without executing anything and the UI layer can add progress handling.
//!
//! Scope: the reviewed (Flathub) channel installs to the **system**
//! installation, where flathub's remotes and runtimes already live. The
//! publisher channel installs to the **user** installation: flatpak's
//! system-scope image install is broken as of 1.18.4 (see
//! [`install_image`]), and a user-scope install also needs no polkit
//! dialog, so no password on install.
//!
//! Install transport for Orchard-channel apps: `flatpak install` accepts
//! image locations directly (`oci:<dir>`, `oci-archive:<file>`,
//! `docker://<registry>/<repo>:<tag>`), auto-detected by prefix (verified
//! against flatpak 1.18.4, `app/flatpak-builtins-install.c`). The image's
//! config labels must carry `org.flatpak.ref` and `org.flatpak.metadata`;
//! flatpak reconstructs the ostree commit from the layers plus labels at
//! install time, so no ostree machinery is needed on the publish side.

use std::path::Path;
use std::process::Command;

/// Name of the flatpak remote the local index shim backs (future native
/// remote flow; the MVP installs images directly instead).
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
/// `index_url` looks like `oci+http://127.0.0.1:7780`. Note (validated in the
/// M0 spike, flatpak sources `common/flatpak-dir.c`): an `oci+http(s)` remote
/// fetches an **ostree summary** from the URL base, whose ref entries map to
/// image manifest digests, with `xa.oci-registry-uri` in the summary
/// metadata naming the registry that serves blobs. Serving that summary is
/// the future native-remote flow; until then the shim's HTTP catalog API is
/// what the client uses.
pub fn ensure_oci_remote(index_url: &str) -> Command {
    let mut cmd = flatpak_command();
    cmd.args(["remote-add", "--oci", "--user", REMOTE_NAME, index_url]);
    cmd
}

/// Install an app id from a normal remote (the Flathub channel), e.g.
/// `flatpak install --system -y flathub org.gnome.Polari`.
pub fn install_ref(app_id: &str, remote: &str) -> Command {
    let mut cmd = flatpak_command();
    cmd.args(["install", "--system", "-y", remote, app_id]);
    cmd
}

/// The flatpak installation an op targets. `System` is the machine-wide
/// install (polkit-gated); `User` is per-user under `$HOME`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Scope {
    System,
    User,
}

impl Scope {
    fn flag(self) -> &'static str {
        match self {
            Scope::System => "--system",
            Scope::User => "--user",
        }
    }
}

/// Install an OCI image directly, no remote involved (flatpak >= 1.17).
///
/// `location` is an image location as understood by `flatpak install`:
/// `oci:<dir>`, `oci-archive:<file>`, or `docker://<registry>/<repo>:<tag>`.
///
/// This is user scope, on purpose. Flatpak's image-install flow creates the
/// transient origin remote with an empty URL, and every system-helper trust
/// check keys on the URL (`flatpak_dir_get_remote_oci` wants an `oci+`
/// prefix), so `install --system` of any image location dies with "Can't
/// pull from untrusted non-gpg verified remote" on flatpak 1.18.4 through
/// current main. The user scope never consults the system helper: the pull
/// rides the image source directly, and that path works end to end
/// (verified against 1.18.4 with a local registry). A user-scope install
/// also needs no polkit dialog.
pub fn install_image(location: &str) -> Command {
    let mut cmd = flatpak_command();
    cmd.args(["install", "--user", "-y", location]);
    cmd
}

/// Update everything the `orchard` remote serves (plus whatever else is stale).
pub fn update() -> Command {
    let mut cmd = flatpak_command();
    cmd.args(["update", "--system", "-y"]);
    cmd
}

/// Uninstall an app id from the given scope. User data is kept.
pub fn uninstall(scope: Scope, app_id: &str) -> Command {
    let mut cmd = flatpak_command();
    cmd.args(["uninstall", scope.flag(), "-y", app_id]);
    cmd
}

/// Launch an installed app.
pub fn run(app_id: &str) -> Command {
    let mut cmd = flatpak_command();
    cmd.arg("run").arg(app_id);
    cmd
}

/// List installed apps across installations:
/// `flatpak list --app --columns=application,version,origin,installation`.
/// Rows are tab-separated; `installation` is `user` or `system`.
pub fn list_installed() -> Command {
    let mut cmd = flatpak_command();
    cmd.args([
        "list",
        "--app",
        "--columns=application,version,origin,installation",
    ]);
    cmd
}

/// Info about one installed app in the given scope: `flatpak info
/// <scope> <app>`. Its text output carries the installed size among
/// other fields.
pub fn info(scope: Scope, app_id: &str) -> Command {
    let mut cmd = flatpak_command();
    cmd.args(["info", scope.flag(), app_id]);
    cmd
}

/// The sandbox metadata ini of an app on a remote, before install:
/// `flatpak remote-info --show-metadata <remote> <app>`. This is the
/// manifest of the commit that would be installed.
pub fn remote_metadata(remote: &str, app_id: &str) -> Command {
    let mut cmd = flatpak_command();
    cmd.args(["remote-info", "--show-metadata", "--system", remote, app_id]);
    cmd
}

/// Update an installed app to its remote's current commit:
/// `flatpak update --system -y <app>`. The app's own origin remote
/// supplies the new commit.
pub fn update_ref(app_id: &str) -> Command {
    let mut cmd = flatpak_command();
    cmd.args(["update", "--system", "-y", app_id]);
    cmd
}

/// Summary of the remote's current commit for an app, before install:
/// `flatpak remote-info <remote> <app>`: version and the download /
/// installed sizes of what would be installed.
pub fn remote_info(remote: &str, app_id: &str) -> Command {
    let mut cmd = flatpak_command();
    cmd.args(["remote-info", "--system", remote, app_id]);
    cmd
}

/// The download and installed sizes from `flatpak remote-info` output
/// (right-aligned keys, humanized values).
pub fn parse_remote_info(output: &str) -> (Option<String>, Option<String>) {
    let mut download = None;
    let mut installed = None;
    for line in output.lines() {
        let line = line.trim();
        if let Some(rest) = line.strip_prefix("Download Size:") {
            download = Some(rest.trim().to_string());
        } else if let Some(rest) = line.strip_prefix("Installed Size:") {
            installed = Some(rest.trim().to_string());
        }
    }
    (download, installed)
}

/// The sandbox metadata ini of an installed app, read straight from the
/// deploy (`app/<id>/current/active/metadata`). System install first,
/// then user. This is what powers the permissions view for apps that
/// came from a remote rather than an Orchard catalog.
pub fn installed_metadata(app_id: &str) -> Option<String> {
    let mut roots = vec![std::path::PathBuf::from("/var/lib/flatpak/app")];
    if let Some(home) = std::env::var_os("HOME") {
        roots.push(
            std::path::PathBuf::from(home)
                .join(".local/share/flatpak/app"),
        );
    }
    roots
        .iter()
        .map(|root| root.join(app_id).join("current/active/metadata"))
        .find_map(|path| std::fs::read_to_string(path).ok())
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
    fn install_ref_builds_expected_invocation() {
        let cmd = install_ref("org.gnome.Polari", "flathub");
        let args = arg_strings(&cmd);
        let tail: Vec<String> = args.iter().skip(args.len() - 5).cloned().collect();
        assert_eq!(
            tail,
            ["install", "--system", "-y", "flathub", "org.gnome.Polari"]
        );
    }

    #[test]
    fn install_image_passes_location_through() {
        for location in [
            "oci:/abs/path/to/layout",
            "oci-archive:/tmp/x.oci.tgz",
            "docker://localhost:5000/hello:stable",
        ] {
            let cmd = install_image(location);
            let args = arg_strings(&cmd);
            // user scope: flatpak's system-scope image install is broken
            // (empty-URL origin remote), see install_image's doc
            assert!(args.contains(&"--user".to_string()));
            assert!(args.last().is_some_and(|last| last == location));
        }
    }

    #[test]
    fn uninstall_targets_the_given_scope() {
        let cmd = uninstall(Scope::User, "rocks.orchard.Hello");
        assert_eq!(
            arg_strings(&cmd),
            ["uninstall", "--user", "-y", "rocks.orchard.Hello"]
        );
        let cmd = uninstall(Scope::System, "org.gnome.Polari");
        assert_eq!(
            arg_strings(&cmd),
            ["uninstall", "--system", "-y", "org.gnome.Polari"]
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

    #[test]
    fn update_ref_builds_expected_invocation() {
        let cmd = update_ref("org.gnome.Polari");
        let args = arg_strings(&cmd);
        let tail: Vec<String> = args.iter().skip(args.len() - 4).cloned().collect();
        assert_eq!(tail, ["update", "--system", "-y", "org.gnome.Polari"]);
    }

    #[test]
    fn parse_remote_info_reads_size_lines() {        let (download, installed) = parse_remote_info(
            "\n        ID: com.spotify.Client\n        Ref: \
             app/com.spotify.Client/x86_64/stable\n    Download Size: 9.8 MB\n   \
             Installed Size: 14.1 MB\n     Runtime: \
             org.freedesktop.Platform/x86_64/25.08\n",
        );
        assert_eq!(download.as_deref(), Some("9.8 MB"));
        assert_eq!(installed.as_deref(), Some("14.1 MB"));

        let (download, installed) = parse_remote_info("no sizes here\n");
        assert_eq!(download, None);
        assert_eq!(installed, None);
    }
}
