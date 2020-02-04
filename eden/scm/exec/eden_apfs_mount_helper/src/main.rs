/*
 * Copyright (c) Facebook, Inc. and its affiliates.
 *
 * This software may be used and distributed according to the terms of the
 * GNU General Public License version 2.
 */

//! This is a little macOS specific utility that is intended to be installed setuid root
//! so that it can mount scratch volumes into a portion of the filesytem
//! owned by a non-privileged user.
//! It is intended to be used together with edenfs, but may also be
//! useful for non-virtualized repos as a way to move IO out of a recursive
//! watch.
use anyhow::*;
use serde::*;
use std::os::unix::fs::MetadataExt;
use std::os::unix::process::CommandExt;
use std::path::PathBuf;
use std::process::Command;
use structopt::StructOpt;

// Take care with the full path to the utility so that we are not so easily
// tricked into running something scary if we are setuid root.
const DISKUTIL: &'static str = "/usr/sbin/diskutil";
const MOUNT_APFS: &'static str = "/sbin/mount_apfs";

#[derive(StructOpt, Debug)]
enum Opt {
    /// List APFS volumes
    #[structopt(name = "list")]
    List {
        #[structopt(long = "all")]
        all: bool,
    },

    /// Mount some space at the specified path.
    /// You must be the owner of the path.
    #[structopt(name = "mount")]
    Mount { mount_point: String },

    /// Unmount the eden space from a specific path.
    /// This will only allow unmounting volumes that were created
    /// by this utility.
    #[structopt(name = "unmount")]
    UnMount {
        /// The mounted path that you wish to unmount
        mount_point: String,
        /// Force the unmount, even if files are open and busy
        #[structopt(long = "force")]
        force: bool,
    },

    /// Unmount and delete a volume associated with a specific path.
    /// This will only allow deleting volumes that were created
    /// by this utility
    #[structopt(name = "delete")]
    Delete {
        /// The mounted path that you wish to unmount
        mount_point: String,
    },
}

#[derive(Clone, Debug, PartialEq, Eq)]
#[derive(Deserialize)]
#[serde(rename_all = "PascalCase")]
struct ApfsContainer {
    container_reference: String,
    volumes: Vec<ApfsVolume>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
#[derive(Deserialize)]
#[serde(rename_all = "PascalCase")]
struct ApfsVolume {
    device_identifier: String,
    mount_point: Option<String>,
    name: Option<String>,
}

#[derive(Deserialize)]
#[serde(rename_all = "PascalCase")]
struct Containers {
    containers: Vec<ApfsContainer>,
}

// A note about `native-plist` vs `json-plist`.
// The intent is that `native-plist` be the thing that we use for real in the long
// term, but we are currently blocked from using this in our CI system due to some
// vendoring issues with external crates.  For the sake of unblocking this feature
// the `json-plist` feature (which is the default) uses a `plutil` executable on
// macos to convert the plist to json and then uses serde_json to extract the data
// of interest.
// In the near future we should unblock the vendoring issue and will be able to
// remove the use of plutil.

#[cfg(feature = "native-plist")]
/// Parse the output from `diskutil apfs list -plist`
fn parse_apfs_plist(data: &str) -> Result<Vec<ApfsContainer>> {
    let containers: Containers =
        plist::from_bytes(data.as_bytes()).context("parsing plist data")?;
    Ok(containers.containers)
}

#[cfg(feature = "json-plist")]
/// Parse the output from `diskutil apfs list -plist` by running it through
/// plutil and converting it to json
fn parse_apfs_plist(data: &str) -> Result<Vec<ApfsContainer>> {
    use std::io::{Read, Write};

    // Run plutil and tell it to convert stdin (that last `-` arg)
    // into json and output it to stdout (the `-o -`).
    let child = new_cmd_unprivileged("/usr/bin/plutil")
        .args(&["-convert", "json", "-o", "-", "-"])
        .stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::piped())
        .spawn()?;

    let mut input = child.stdin.unwrap();
    input.write_all(data.as_bytes())?;
    drop(input);

    let mut json = String::new();
    child.stdout.unwrap().read_to_string(&mut json)?;

    let containers: Containers = serde_json::from_str(&json).context("parsing json data")?;
    Ok(containers.containers)
}

/// Obtain the list of apfs containers and volumes by executing `diskutil`.
fn apfs_list() -> Result<Vec<ApfsContainer>> {
    let output = new_cmd_unprivileged(DISKUTIL)
        .args(&["apfs", "list", "-plist"])
        .output()?;
    if !output.status.success() {
        anyhow::bail!("failed to execute diskutil list: {:#?}", output);
    }
    Ok(parse_apfs_plist(&String::from_utf8(output.stdout)?)?)
}

fn find_existing_volume<'a>(containers: &'a [ApfsContainer], name: &str) -> Option<&'a ApfsVolume> {
    for container in containers {
        for volume in &container.volumes {
            if volume.name.as_ref().map(String::as_ref) == Some(name) {
                return Some(volume);
            }
        }
    }
    None
}

/// Prepare a command to be run with root privs.
/// The path must be absolute to avoid being fooled into running something
/// unexpected.
/// The caller must already have root privs, otherwise this will fail.
fn new_cmd_with_root_privs(path: &str) -> Command {
    let path: PathBuf = path.into();
    assert!(path.is_absolute());
    assert!(
        geteuid() == 0,
        "root privs are required to run {}",
        path.display()
    );
    Command::new(path)
}

/// Prepare a command to be run with no special privs.
/// We're usually installed setuid root so we already have privs; the
/// command invocation will restore the real uid/gid of the caller
/// as part of running the command so that we avoid running too much
/// stuff with privs.
fn new_cmd_unprivileged(path: &str) -> Command {
    let path: PathBuf = path.into();
    assert!(path.is_absolute());
    let mut cmd = Command::new(path);

    if geteuid() == 0 {
        // We're running with effective root privs; run this command
        // with the privs of the real user, just in case.
        cmd.uid(getuid()).gid(getgid());
    }

    cmd
}

/// Create a new subvolume with the specified name.
/// Note that this does NOT require any special privilege on macOS.
fn make_new_volume(name: &str) -> Result<ApfsVolume> {
    let output = new_cmd_unprivileged(DISKUTIL)
        .args(&["apfs", "addVolume", "disk1", "apfs", name, "-nomount"])
        .output()?;
    if !output.status.success() {
        anyhow::bail!("failed to execute diskutil addVolume: {:?}", output);
    }
    let containers = apfs_list()?;
    find_existing_volume(&containers, name)
        .ok_or_else(|| anyhow!("failed to create volume `{}`: {:#?}", name, output))
        .map(ApfsVolume::clone)
}

fn getgid() -> u32 {
    unsafe { libc::getgid() }
}

fn getuid() -> u32 {
    unsafe { libc::getuid() }
}

fn geteuid() -> u32 {
    unsafe { libc::geteuid() }
}

fn get_real_uid() -> Result<u32> {
    let uid = getuid();

    if uid != 0 {
        return Ok(uid);
    }

    // We're really root (not just setuid root).  We may actually be
    // running under sudo so let's see what sudo says about the UID
    match std::env::var("SUDO_UID") {
        Ok(uid) => Ok(uid.parse().context(format!(
            "parsing the SUDO_UID={} env var as an integer",
            uid
        ))?),
        Err(std::env::VarError::NotPresent) => Ok(uid),
        Err(std::env::VarError::NotUnicode(_)) => bail!("the SUDO_UID env var is not unicode"),
    }
}

fn mount_scratch_space_on(mount_point: &str) -> Result<()> {
    println!("want to mount at {:?}", mount_point);

    // First, let's ensure that mounting at this location makes sense.
    // Inspect the directory and ensure that it is owned by us.
    let metadata = std::fs::metadata(mount_point)
        .context(format!("Obtaining filesystem metadata for {}", mount_point))?;
    let my_uid = get_real_uid()?;
    if metadata.uid() != my_uid {
        bail!(
            "Refusing to set up a volume for {} because the owned uid {} doesn't match your uid {}",
            mount_point,
            metadata.uid(),
            my_uid
        );
    }

    println!("my real uid is {}, effective is {}", my_uid, unsafe {
        libc::geteuid()
    });

    let containers = apfs_list()?;
    let name = encode_mount_point_as_volume_name(mount_point);
    let volume = match find_existing_volume(&containers, &name) {
        Some(existing) => {
            if existing.mount_point.is_some()
                && existing.mount_point != Some(mount_point.to_string())
            {
                // macOS will automatically mount volumes at system boot,
                // but mount them under /Volumes.  That will block our attempt
                // to mount the scratch space below, so if we see that this
                // volume is mounted and not where we want it, we simply unmount
                // it here now: this should be fine because we own these volumes
                // and where they get mounted.  No one else should have a legit
                // reason for mounting it elsewhere.
                unmount_scratch(mount_point, true)?;
            }
            existing.clone()
        }
        None => make_new_volume(&name)?,
    };

    // Mount the volume at the desired mount point.
    // This is the only part of this utility that requires root privs.
    let output = new_cmd_with_root_privs(MOUNT_APFS)
        .args(&[
            "-onobrowse,nodev,nosuid",
            "-u",
            &format!("{}", metadata.uid()),
            "-g",
            &format!("{}", metadata.gid()),
            &format!("/dev/{}", volume.device_identifier),
            mount_point,
        ])
        .output()?;
    if !output.status.success() {
        anyhow::bail!(
            "failed to execute mount_apfs /dev/{} {}: {:#?}",
            volume.device_identifier,
            mount_point,
            output
        );
    }
    println!("output: {:?}", output);

    // Make sure that we own the mounted directory; the default is mounted
    // with root:wheel ownership, and that isn't desirable
    let mount_point_cstr = std::ffi::CString::new(mount_point)
        .context("creating a C string from the mount point path")?;
    let rc = unsafe { libc::chown(mount_point_cstr.as_ptr(), metadata.uid(), metadata.gid()) };
    if rc != 0 {
        let err = std::io::Error::last_os_error();
        bail!("failed to chown the mount point back to the owner: {}", err);
    }

    Ok(())
}

/// Encode a mount point as a volume name.
/// The story here is that diskutil allows any user to create an APFS
/// volume, but requires root privs to mount it into the VFS.
/// We're setuid root to facilitate this, but to make things safe(r)
/// we create volumes with an encoded name so that we can tell that
/// they were created by this tool for a specific mount point.
/// We will only mount volumes that have that encoded name, at the
/// location encoded by their name and refuse to mount anything else.
fn encode_mount_point_as_volume_name(mount_point: &str) -> String {
    format!("edenfs:{}", mount_point)
}

fn unmount_scratch(mount_point: &str, force: bool) -> Result<()> {
    let containers = apfs_list()?;
    let name = encode_mount_point_as_volume_name(mount_point);
    if let Some(volume) = find_existing_volume(&containers, &name) {
        let mut cmd = new_cmd_unprivileged(DISKUTIL);
        cmd.arg("unmount");

        if force {
            cmd.arg("force");
        }
        cmd.arg(&volume.device_identifier);
        let output = cmd.output()?;
        if !output.status.success() {
            anyhow::bail!(
                "failed to execute diskutil unmount {}: {:?}",
                volume.device_identifier,
                output
            );
        }
    } else {
        bail!("Did not find a volume named {}", name);
    }
    Ok(())
}

fn delete_scratch(mount_point: &str) -> Result<()> {
    let containers = apfs_list()?;
    let name = encode_mount_point_as_volume_name(mount_point);
    if let Some(volume) = find_existing_volume(&containers, &name) {
        // This will implicitly unmount, so we don't need to deal
        // with that here
        let output = new_cmd_unprivileged(DISKUTIL)
            .args(&["apfs", "deleteVolume", &volume.device_identifier])
            .output()?;
        if !output.status.success() {
            anyhow::bail!(
                "failed to execute diskutil deleteVolume {}: {:?}",
                volume.device_identifier,
                output
            );
        }
        Ok(())
    } else {
        bail!("Did not find a volume named {}", name);
    }
}

fn main() -> Result<()> {
    let opts = Opt::from_args();

    match opts {
        Opt::List { all } => {
            let containers = apfs_list()?;
            if all {
                println!("{:#?}", containers);
            } else {
                for container in containers {
                    for vol in container.volumes {
                        if let Some(name) = vol.name.as_ref() {
                            if name.starts_with("edenfs:") {
                                println!("{:#?}", vol);
                            }
                        }
                    }
                }
            }
            Ok(())
        }

        Opt::Mount { mount_point } => mount_scratch_space_on(&mount_point),

        Opt::UnMount { mount_point, force } => {
            unmount_scratch(&mount_point, force)?;
            Ok(())
        }

        Opt::Delete { mount_point } => {
            delete_scratch(&mount_point)?;
            Ok(())
        }
    }
}

// We only run the tests on macos as we currently default to a mode that requires
// the plutil utility to be installed.  That limitation can be removed once some
// build system work is completed that will unblock using a different crate vendoring
// system at fb.
#[cfg(all(test, any(target_os = "macos", feature = "native-plist")))]
mod test {
    use super::*;
    use pretty_assertions::assert_eq;

    #[test]
    fn test_plist() {
        let data = r#"
<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0">
<dict>
	<key>Containers</key>
	<array>
		<dict>
			<key>APFSContainerUUID</key>
			<string>C4AC89F6-8658-4857-972C-D485C213523A</string>
			<key>CapacityCeiling</key>
			<integer>499963174912</integer>
			<key>CapacityFree</key>
			<integer>30714478592</integer>
			<key>ContainerReference</key>
			<string>disk1</string>
			<key>DesignatedPhysicalStore</key>
			<string>disk0s2</string>
			<key>Fusion</key>
			<false/>
			<key>PhysicalStores</key>
			<array>
				<dict>
					<key>DeviceIdentifier</key>
					<string>disk0s2</string>
					<key>DiskUUID</key>
					<string>2F978E12-5A2C-4EEB-BAE2-0E09CAEADC06</string>
					<key>Size</key>
					<integer>499963174912</integer>
				</dict>
			</array>
			<key>Volumes</key>
			<array>
				<dict>
					<key>APFSVolumeUUID</key>
					<string>9AA7F3A4-A615-4F8D-91E3-F5C86D988D71</string>
					<key>CapacityInUse</key>
					<integer>461308219392</integer>
					<key>CapacityQuota</key>
					<integer>0</integer>
					<key>CapacityReserve</key>
					<integer>0</integer>
					<key>CryptoMigrationOn</key>
					<false/>
					<key>DeviceIdentifier</key>
					<string>disk1s1</string>
					<key>Encryption</key>
					<true/>
					<key>FileVault</key>
					<true/>
					<key>Locked</key>
					<false/>
					<key>Name</key>
					<string>Macintosh HD</string>
					<key>Roles</key>
					<array/>
				</dict>
				<dict>
					<key>APFSVolumeUUID</key>
					<string>A91FD4EA-684D-4122-9ACD-27E1465E99F6</string>
					<key>CapacityInUse</key>
					<integer>43061248</integer>
					<key>CapacityQuota</key>
					<integer>0</integer>
					<key>CapacityReserve</key>
					<integer>0</integer>
					<key>CryptoMigrationOn</key>
					<false/>
					<key>DeviceIdentifier</key>
					<string>disk1s2</string>
					<key>Encryption</key>
					<false/>
					<key>FileVault</key>
					<false/>
					<key>Locked</key>
					<false/>
					<key>Name</key>
					<string>Preboot</string>
					<key>Roles</key>
					<array>
						<string>Preboot</string>
					</array>
				</dict>
				<dict>
					<key>APFSVolumeUUID</key>
					<string>1C94FFC8-7649-470E-952D-16672E135C43</string>
					<key>CapacityInUse</key>
					<integer>510382080</integer>
					<key>CapacityQuota</key>
					<integer>0</integer>
					<key>CapacityReserve</key>
					<integer>0</integer>
					<key>CryptoMigrationOn</key>
					<false/>
					<key>DeviceIdentifier</key>
					<string>disk1s3</string>
					<key>Encryption</key>
					<false/>
					<key>FileVault</key>
					<false/>
					<key>Locked</key>
					<false/>
					<key>Name</key>
					<string>Recovery</string>
					<key>Roles</key>
					<array>
						<string>Recovery</string>
					</array>
				</dict>
				<dict>
					<key>APFSVolumeUUID</key>
					<string>6BC72964-0CA0-48AE-AAE1-7E9BFA8B2005</string>
					<key>CapacityInUse</key>
					<integer>6442676224</integer>
					<key>CapacityQuota</key>
					<integer>0</integer>
					<key>CapacityReserve</key>
					<integer>0</integer>
					<key>CryptoMigrationOn</key>
					<false/>
					<key>DeviceIdentifier</key>
					<string>disk1s4</string>
					<key>Encryption</key>
					<true/>
					<key>FileVault</key>
					<false/>
					<key>Locked</key>
					<false/>
					<key>Name</key>
					<string>VM</string>
					<key>Roles</key>
					<array>
						<string>VM</string>
					</array>
				</dict>
				<dict>
					<key>APFSVolumeUUID</key>
					<string>6C7EEDAD-385B-49AB-857B-AD15D98D13ED</string>
					<key>CapacityInUse</key>
					<integer>790528</integer>
					<key>CapacityQuota</key>
					<integer>0</integer>
					<key>CapacityReserve</key>
					<integer>0</integer>
					<key>CryptoMigrationOn</key>
					<false/>
					<key>DeviceIdentifier</key>
					<string>disk1s5</string>
					<key>Encryption</key>
					<true/>
					<key>FileVault</key>
					<false/>
					<key>Locked</key>
					<false/>
					<key>Name</key>
					<string>edenfs:/Users/wez/fbsource/buck-out</string>
					<key>Roles</key>
					<array/>
				</dict>
				<dict>
					<key>APFSVolumeUUID</key>
					<string>0DAB1407-0283-408E-88EE-CD41CE9E7BCA</string>
					<key>CapacityInUse</key>
					<integer>781156352</integer>
					<key>CapacityQuota</key>
					<integer>0</integer>
					<key>CapacityReserve</key>
					<integer>0</integer>
					<key>CryptoMigrationOn</key>
					<false/>
					<key>DeviceIdentifier</key>
					<string>disk1s6</string>
					<key>Encryption</key>
					<true/>
					<key>FileVault</key>
					<false/>
					<key>Locked</key>
					<false/>
					<key>Name</key>
					<string>edenfs:/Users/wez/fbsource/fbcode/buck-out</string>
					<key>Roles</key>
					<array/>
				</dict>
				<dict>
					<key>APFSVolumeUUID</key>
					<string>253A48CA-074E-496E-9A62-9F64831D7A65</string>
					<key>CapacityInUse</key>
					<integer>925696</integer>
					<key>CapacityQuota</key>
					<integer>0</integer>
					<key>CapacityReserve</key>
					<integer>0</integer>
					<key>CryptoMigrationOn</key>
					<false/>
					<key>DeviceIdentifier</key>
					<string>disk1s7</string>
					<key>Encryption</key>
					<true/>
					<key>FileVault</key>
					<false/>
					<key>Locked</key>
					<false/>
					<key>Name</key>
					<string>edenfs:/Users/wez/fbsource/fbobjc/buck-out</string>
					<key>Roles</key>
					<array/>
				</dict>
			</array>
		</dict>
	</array>
</dict>
</plist>"#;
        let containers = parse_apfs_plist(data).unwrap();
        assert_eq!(
            containers,
            vec![ApfsContainer {
                container_reference: "disk1".to_owned(),
                volumes: vec![
                    ApfsVolume {
                        device_identifier: "disk1s1".to_owned(),
                        mount_point: None,
                        name: Some("Macintosh HD".to_owned()),
                    },
                    ApfsVolume {
                        device_identifier: "disk1s2".to_owned(),
                        mount_point: None,
                        name: Some("Preboot".to_owned()),
                    },
                    ApfsVolume {
                        device_identifier: "disk1s3".to_owned(),
                        mount_point: None,
                        name: Some("Recovery".to_owned()),
                    },
                    ApfsVolume {
                        device_identifier: "disk1s4".to_owned(),
                        mount_point: None,
                        name: Some("VM".to_owned()),
                    },
                    ApfsVolume {
                        device_identifier: "disk1s5".to_owned(),
                        mount_point: None,
                        name: Some("edenfs:/Users/wez/fbsource/buck-out".to_owned()),
                    },
                    ApfsVolume {
                        device_identifier: "disk1s6".to_owned(),
                        mount_point: None,
                        name: Some("edenfs:/Users/wez/fbsource/fbcode/buck-out".to_owned()),
                    },
                    ApfsVolume {
                        device_identifier: "disk1s7".to_owned(),
                        mount_point: None,
                        name: Some("edenfs:/Users/wez/fbsource/fbobjc/buck-out".to_owned()),
                    },
                ],
            },]
        );
    }
}