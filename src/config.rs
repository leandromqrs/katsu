use crate::{builder::Bootloader, cli::OutputFormat, util::enter_chroot_run};
use bytesize::ByteSize;
use color_eyre::Result;
use serde::Deserialize;
use serde_derive::{Deserialize, Serialize};
use std::{
	collections::BTreeMap,
	fs,
	io::Write,
	path::{Path, PathBuf},
};
use tracing::{debug, info, trace, warn};
const DEFAULT_VOLID: &str = "KATSU-LIVEOS";

#[derive(Deserialize, Debug, Clone, Serialize)]
pub struct IsoConfig {
	/// Volume ID for the ISO image
	#[serde(default)]
	pub volume_id: Option<String>,
}

impl IsoConfig {
	pub fn get_volid(&self) -> String {
		if let Some(volid) = &self.volume_id {
			volid.clone()
		} else {
			DEFAULT_VOLID.to_string()
		}
	}
}

#[derive(Deserialize, Debug, Clone, Serialize)]
pub struct Manifest {
	pub builder: Option<String>,
	#[serde(default)]
	pub import: Vec<PathBuf>,
	/// The distro name for the build result
	// entrypoint must have a distro name
	#[serde(default)]
	pub distro: Option<String>,

	/// Output file name
	// entrypoint must have an output location
	#[serde(default)]
	pub out_file: Option<String>,

	#[serde(default)]
	pub disk: Option<PartitionLayout>,

	/// DNF configuration
	// todo: dynamically load this?
	#[serde(default)]
	pub dnf: crate::builder::DnfRootBuilder,

	#[serde(default)]
	pub bootc: crate::builder::BootcRootBuilder,

	/// Scripts to run before and after the build
	#[serde(default)]
	pub scripts: ScriptsManifest,

	/// Users to add to the image
	#[serde(default)]
	pub users: Vec<Auth>,

	/// Extra parameters to the kernel command line in bootloader configs
	pub kernel_cmdline: Option<String>,

	/// ISO config (optional)
	/// This is only used for ISO images
	#[serde(default)]
	pub iso: Option<IsoConfig>,

	// deserialize with From<&str>
	#[serde(default, deserialize_with = "deseralize_bootloader")]
	pub bootloader: Bootloader,
}

// Function to deserialize String into Bootloader

fn deseralize_bootloader<'de, D>(deserializer: D) -> Result<Bootloader, D::Error>
where
	D: serde::Deserializer<'de>,
{
	let s = String::deserialize(deserializer)?;
	Ok(Bootloader::from(s.as_str()))
}

impl Manifest {
	pub fn get_volid(&self) -> String {
		if let Some(iso) = &self.iso {
			iso.get_volid()
		} else {
			DEFAULT_VOLID.to_string()
		}
	}
	/// Loads a single manifest from a file
	pub fn load(path: &Path) -> Result<Self> {
		let mut manifest: Self = serde_yaml::from_str(&std::fs::read_to_string(path)?)?;

		// get dir of path relative to cwd

		fn path_not_exists_error(path: &Path) -> color_eyre::eyre::Report {
			tracing::error!(?path, "Path does not exist");
			color_eyre::eyre::eyre!("Path does not exist: {path:#?}", path = path)
		}

		let mut path_can = path.canonicalize()?;

		path_can.pop();
		trace!(path = ?path_can, "Canonicalizing path");

		for import in &mut manifest.import {
			debug!("Import: {import:#?}");
			if !path_can.join(&import).exists() {
				return Err(path_not_exists_error(&path_can.join(&import)));
			}
			*import = path_can.join(&import).canonicalize()?;
			debug!("Canonicalized import: {import:#?}");
		}

		// canonicalize all file paths in scripts, then modify their paths put in the manifest

		for script in &mut manifest.scripts.pre {
			if let Some(f) = script.file.as_mut() {
				trace!(?f, "Loading pre scripts");
				if !path_can.join(&f).exists() {
					return Err(path_not_exists_error(&path_can.join(&f)));
				}
				*f = path_can.join(&f).canonicalize()?;
			}
		}

		for script in &mut manifest.scripts.post {
			if let Some(f) = script.file.as_mut() {
				if !path_can.join(&f).exists() {
					return Err(path_not_exists_error(&path_can.join(&f)));
				}
				trace!(?f, "Loading post scripts");
				*f = path_can.join(&f).canonicalize()?;
			}
		}

		//  canonicalize repodir if it exists, relative to the file that imported it
		if let Some(repodir) = &mut manifest.dnf.repodir {
			// check if path even exists
			let repodir_can = path_can.join(&repodir);
			if !repodir_can.exists() {
				return Err(path_not_exists_error(&repodir_can));
			}
			*repodir = repodir_can.canonicalize()?;
		}

		Ok(manifest)
	}

	pub fn load_all(path: &Path, output: OutputFormat) -> Result<Self> {
		use std::mem::take;

		// get all imports, then merge them all
		let mut manifest = Self::load(path)?;
		// do not override:
		let bootloader = take(&mut manifest.bootloader);
		let iso = take(&mut manifest.iso);
		let disk = take(&mut manifest.disk);

		let mut dnf = take(&mut manifest.dnf);
		// everything but the package lists
		manifest.dnf.packages = take(&mut dnf.packages);
		manifest.dnf.arch_packages = take(&mut dnf.arch_packages);
		manifest.dnf.arch_exclude = take(&mut dnf.arch_exclude);
		manifest.dnf.options = take(&mut dnf.options);
		manifest.dnf.exclude = take(&mut dnf.exclude);
		manifest.dnf.repodir = take(&mut dnf.repodir);

		manifest = manifest.import.iter().try_fold(manifest.clone(), |acc, import| {
			Result::<_>::Ok(merge_struct::merge(&acc, &Self::load_all(import, output)?)?)
		})?;

		manifest.bootloader = bootloader;
		match output {
			OutputFormat::Iso => manifest.iso = iso.or(manifest.iso),
			OutputFormat::Device => todo!("DeviceBuilder not implemented?"),
			OutputFormat::DiskImage => manifest.disk = disk.or(manifest.disk),
			OutputFormat::Folder => manifest.out_file = None,
		}
		(dnf.packages, dnf.arch_packages, dnf.arch_exclude, dnf.exclude, dnf.repodir) = (
			manifest.dnf.packages,
			manifest.dnf.arch_packages,
			manifest.dnf.arch_exclude,
			manifest.dnf.exclude,
			manifest.dnf.repodir,
		);
		dnf.options = merge_struct::merge(&manifest.dnf.options, &manifest.dnf.global_options)?;

		manifest.dnf = dnf;

		Ok(manifest)
	}
}

#[derive(Deserialize, Debug, Clone, Serialize, Default)]
pub struct ScriptsManifest {
	#[serde(default)]
	pub pre: Vec<Script>,
	#[serde(default)]
	pub post: Vec<Script>,
}

fn script_default_priority() -> i32 {
	50
}

#[derive(Deserialize, Debug, Clone, Serialize, PartialEq, Eq, Default)]
// load script from file, or inline if there's one specified
pub struct Script {
	pub id: Option<String>,
	pub name: Option<String>,
	pub file: Option<PathBuf>,
	pub inline: Option<String>,
	pub chroot: Option<bool>,
	#[serde(default)]
	pub needs: Vec<String>,
	/// Default 50, the higher, the later the script executes
	#[serde(default = "script_default_priority")]
	pub priority: i32,
}

impl Script {
	pub fn load(&self) -> Option<String> {
		if self.inline.is_some() {
			self.inline.clone()
		} else if let Some(f) = &self.file {
			std::fs::read_to_string(f.canonicalize().unwrap_or_default()).ok()
		} else {
			self.file
				.as_ref()
				.and_then(|f| std::fs::read_to_string(f.canonicalize().unwrap_or_default()).ok())
		}
	}
}

/// Utility function for determining partition /dev names
/// For cases where it's a mmcblk, or nvme, or loop device etc
pub fn partition_name(disk: &str, partition: usize) -> String {
	format!(
		"{disk}{}{partition}",
		if disk.starts_with("/dev/mmcblk")
			|| disk.starts_with("/dev/nvme")
			|| disk.starts_with("/dev/loop")
		{
			// mmcblk0p1 / nvme0n1p1 / loop0p1
			"p"
		} else {
			// sda1
			""
		}
	)
}

#[test]
fn test_dev_name() {
	let devname = partition_name("/dev/mmcblk0", 1);
	assert_eq!(devname, "/dev/mmcblk0p1");

	let devname = partition_name("/dev/nvme0n1", 1);
	assert_eq!(devname, "/dev/nvme0n1p1");

	let devname = partition_name("/dev/loop0", 1);
	assert_eq!(devname, "/dev/loop0p1");

	let devname = partition_name("/dev/sda", 1);
	assert_eq!(devname, "/dev/sda1");
}

#[derive(Deserialize, Debug, Clone, Serialize, PartialEq, Eq, Default)]
pub struct PartitionLayout {
	pub size: Option<ByteSize>,
	pub partitions: Vec<Partition>,
}

#[derive(Serialize, Debug)]
struct TplFstabEntry<'a> {
	uuid: String,
	mp: String,
	fsname: &'a str,
	fsck: u8,
}

#[allow(dead_code)]
impl PartitionLayout {
	pub fn new() -> Self {
		Self::default()
	}

	/// Adds a partition to the layout
	pub fn add_partition(&mut self, partition: Partition) {
		self.partitions.push(partition);
	}

	pub fn get_index(&self, mountpoint: &str) -> Option<usize> {
		// index should be +1 of the actual partition number (sda1 is index 0)
		self.partitions.iter().position(|p| p.mountpoint == mountpoint).map(|i| i + 1)
	}

	pub fn get_partition(&self, mountpoint: &str) -> Option<&Partition> {
		self.partitions.iter().find(|p| p.mountpoint == mountpoint)
	}

	pub fn sort_partitions(&self) -> Vec<(usize, Partition)> {
		// We should sort partitions by mountpoint, so that we can mount them in order
		// In this case, from the least nested to the most nested, so count the number of slashes

		// sort by least nested to most nested

		// However, also keep the original order of the partitions from the manifest

		// the key is the original index of the partition so we can get the right devname from its index

		let mut ordered = BTreeMap::new();

		for part in &self.partitions {
			let index = self.get_index(&part.mountpoint).unwrap();
			ordered.insert(index, part.clone());

			trace!(?index, ?part, "Index and partition");
		}

		// now sort by mountpoint, least nested to most nested by counting the number of slashes
		// but make an exception if it's just /, then it's 0

		// if it has the same number of slashes, sort by alphabetical order

		let mut ordered = ordered.into_iter().collect::<Vec<_>>();

		ordered.sort_unstable_by(|(_, a), (_, b)| {
			// trim trailing slashes
			let am = a.mountpoint.trim_end_matches('/').matches('/').count();
			let bm = b.mountpoint.trim_end_matches('/').matches('/').count();
			if a.mountpoint.is_empty() {
				// empty mountpoint should always come first
				std::cmp::Ordering::Less
			} else if b.mountpoint.is_empty() {
				// empty mountpoint should always come first
				std::cmp::Ordering::Greater
			} else if a.mountpoint == "/" {
				// / should always come first
				std::cmp::Ordering::Less
			} else if b.mountpoint == "/" {
				// / should always come first
				std::cmp::Ordering::Greater
			} else if am == bm {
				// alphabetical order
				a.mountpoint.cmp(&b.mountpoint)
			} else {
				am.cmp(&bm)
			}
		});
		ordered
	}

	pub fn mount_to_chroot(&self, disk: &Path, chroot: &Path) -> Result<()> {
		// mount partitions to chroot

		// sort partitions by mountpoint
		let ordered: Vec<_> = self.sort_partitions();

		// Ok, so for some reason the partitions are swapped?
		for (index, part) in &ordered {
			// println!("Partition {index}: {part:#?}");

			if part.mountpoint.is_empty()
				|| part.filesystem == "none"
				|| part.filesystem == "swap"
				|| part.mountpoint == "-"
			{
				// skip empty mountpoints
				warn!(?part, "This partition is not supposed to be mounted! Skipping... If you want this partition to be mounted, please specify a mountpoint starting with /");
				continue;
			}
			let devname = partition_name(&disk.to_string_lossy(), *index);

			// clean the mountpoint so we don't have the slash at the start
			let mp_cleaned = part.mountpoint.trim_start_matches('/');
			let mountpoint = chroot.join(mp_cleaned);

			std::fs::create_dir_all(&mountpoint)?;

			trace!("mount {devname} {mountpoint:?}");

			cmd_lib::run_cmd!(mount $devname $mountpoint 2>&1)?;
		}

		Ok(())
	}

	pub fn unmount_from_chroot(&self, chroot: &Path) -> Result<()> {
		// unmount partitions from chroot
		// sort partitions by mountpoint
		for mp in self.sort_partitions().into_iter().rev().map(|(_, p)| p.mountpoint) {
			if mp.is_empty() || mp == "-" {
				continue;
			}
			let mp = chroot.join(mp.trim_start_matches('/'));
			trace!("umount {mp:?}");
			cmd_lib::run_cmd!(umount $mp 2>&1)?;
		}
		Ok(())
	}

	/// Generate fstab entries for the partitions
	pub fn fstab(&self, chroot: &Path) -> Result<String> {
		// sort partitions by mountpoint
		let ordered = self.sort_partitions();

		crate::prepend_comment!(PREPEND: "/etc/fstab", "static file system information.", katsu::config::PartitionLayout::fstab);

		let mut entries = vec![];

		ordered.iter().try_for_each(|(_, part)| -> Result<()> {
			if part.filesystem != "none" {
				let mp = PathBuf::from(&part.mountpoint).to_string_lossy().to_string();
				let mountpoint_chroot = part.mountpoint.trim_start_matches('/');
				let mountpoint_chroot = chroot.join(mountpoint_chroot);
				let devname = cmd_lib::run_fun!(findmnt -n -o SOURCE $mountpoint_chroot)?;

				// We will generate by UUID
				let uuid = cmd_lib::run_fun!(blkid -s UUID -o value $devname)?;

				let fsname = if part.filesystem == "efi" { "vfat" } else { &part.filesystem };
				let fsck = if part.filesystem == "efi" { 0 } else { 2 };

				entries.push(TplFstabEntry { uuid, mp, fsname, fsck });
			}
			Ok(())
		})?;

		trace!(?entries, "fstab entries generated");

		Ok(crate::tpl!("fstab.tera" => { PREPEND, entries }))
	}

	pub fn apply(&self, disk: &PathBuf, target_arch: &str) -> Result<()> {
		// This is a destructive operation, so we need to make sure we don't accidentally wipe the wrong disk

		info!("Applying partition layout to disk: {disk:#?}");

		// format disk with GPT

		trace!("Formatting disk with GPT");
		trace!("parted -s {disk:?} mklabel gpt");
		cmd_lib::run_cmd!(parted -s $disk mklabel gpt 2>&1)?;

		// create partitions
		self.partitions.iter().try_fold((1, 0), |(i, mut last_end), part| {
			let devname = partition_name(&disk.to_string_lossy(), i);
			trace!(devname, "Creating partition {i}: {part:#?}");

			let span = tracing::trace_span!("partition", devname);
			let _enter = span.enter();

			let start_string = if i == 1 {
				// create partition at start of disk
				"0".to_string()
			} else {
				// create partition after last partition
				format!("{}MiB", last_end / 1024 / 1024)
			};

			let end_string = part.size.map_or("100%".to_string(), |size| {
				// create partition with size
				last_end += size.as_u64();

				// remove space for partition table
				format!("{}MiB", last_end / 1024 / 1024)
			});

			// not going to change this for now though, but will revisit
			debug!(start = start_string, end = end_string, "Creating partition");
			trace!("parted -s {disk:?} mkpart primary fat32 {start_string} {end_string}");
			cmd_lib::run_cmd!(parted -s $disk mkpart primary fat32 $start_string $end_string 2>&1)?;

			let part_type_uuid = part.partition_type.uuid(target_arch);

			debug!("Setting partition type");
			trace!("parted -s {disk:?} type {i} {part_type_uuid}");
			cmd_lib::run_cmd!(parted -s $disk type $i $part_type_uuid 2>&1)?;

			if let Some(flags) = &part.flags {
				debug!("Setting partition attribute flags");

				for flag in flags {
					let position = flag.flag_position();
					trace!("sgdisk -A {i}:set:{position} {disk:?}");
					cmd_lib::run_cmd!(sgdisk -A $i:set:$position $disk 2>&1)?;
				}
			}

			if part.filesystem == "efi" {
				debug!("Setting esp on for efi partition");
				trace!("parted -s {disk:?} set {i} esp on");
				cmd_lib::run_cmd!(parted -s $disk set $i esp on 2>&1)?;
			}

			if let Some(label) = &part.label {
				debug!(label, "Setting label");
				trace!("parted -s {disk:?} name {i} {label}");
				cmd_lib::run_cmd!(parted -s $disk name $i $label 2>&1)?;
			}

			trace!("Refreshing partition tables");
			let _ = cmd_lib::run_cmd!(partprobe); // comes with parted supposedly

			// time to format the filesystem
			let fsname = &part.filesystem;
			// Some stupid hackery checks for the args of mkfs.fat
			debug!(fsname, "Formatting partition");
			if fsname == "efi" {
				trace!("mkfs.fat -F32 {devname}");
				cmd_lib::run_cmd!(mkfs.fat -F32 $devname 2>&1)?;
			} else if fsname == "none" {
			} else {
				trace!("mkfs.{fsname} {devname}");
				cmd_lib::run_cmd!(mkfs.$fsname $devname 2>&1)?;
			}

			Result::<_>::Ok((i + 1, last_end))
		})?;

		Ok(())
	}
}

#[test]
fn test_partlay() {
	use std::str::FromStr;
	use tracing_subscriber::prelude::__tracing_subscriber_SubscriberExt;
	use tracing_subscriber::Layer;

	// Partition layout test
	let subscriber =
		tracing_subscriber::Registry::default().with(tracing_error::ErrorLayer::default()).with(
			tracing_subscriber::fmt::layer()
				.pretty()
				.with_filter(tracing_subscriber::EnvFilter::from_str("trace").unwrap()),
		);
	tracing::subscriber::set_global_default(subscriber).expect("setting default subscriber failed");

	let mock_disk = PathBuf::from("/dev/sda");

	let mut partlay = PartitionLayout::new();

	partlay.add_partition(Partition {
		label: Some("EFI".to_string()),
		partition_type: PartitionType::Esp,
		flags: None,
		size: Some(ByteSize::mib(100)),
		filesystem: "efi".to_string(),
		mountpoint: "/boot/efi".to_string(),
		subvolumes: vec![],
	});

	partlay.add_partition(Partition {
		label: Some("boot".to_string()),
		partition_type: PartitionType::Xbootldr,
		flags: None,
		size: Some(ByteSize::gib(100)),
		filesystem: "ext4".to_string(),
		mountpoint: "/boot".to_string(),
		subvolumes: vec![],
	});

	partlay.add_partition(Partition {
		label: Some("ROOT".to_string()),
		partition_type: PartitionType::Root,
		flags: None,
		size: Some(ByteSize::gib(100)),
		filesystem: "ext4".to_string(),
		mountpoint: "/".to_string(),
		subvolumes: vec![],
	});

	for (i, part) in partlay.partitions.iter().enumerate() {
		println!("Partition {i}:");
		println!("{part:#?}");

		// get index of partition
		let index = partlay.get_index(&part.mountpoint).unwrap();
		println!("Index: {index}");

		println!("Partition name: {}", partition_name(&mock_disk.to_string_lossy(), index));

		println!("====================");
	}

	let lay = partlay.sort_partitions();

	println!("{partlay:#?}");
	println!("sorted: {lay:#?}");

	// Assert that:

	// 1. The partitions are sorted by mountpoint
	// / will come first
	// /boot will come second
	// /boot/efi will come last

	let assertion = vec![
		(
			3,
			Partition {
				label: Some("ROOT".to_string()),
				partition_type: PartitionType::Root,
				flags: None,
				size: Some(ByteSize::gib(100)),
				filesystem: "ext4".to_string(),
				mountpoint: "/".to_string(),
				subvolumes: vec![],
			},
		),
		(
			2,
			Partition {
				label: Some("boot".to_string()),
				partition_type: PartitionType::Xbootldr,
				flags: None,
				size: Some(ByteSize::gib(100)),
				filesystem: "ext4".to_string(),
				mountpoint: "/boot".to_string(),
				subvolumes: vec![],
			},
		),
		(
			1,
			Partition {
				label: Some("EFI".to_string()),
				partition_type: PartitionType::Esp,
				flags: None,
				size: Some(ByteSize::mib(100)),
				filesystem: "efi".to_string(),
				mountpoint: "/boot/efi".to_string(),
				subvolumes: vec![],
			},
		),
	];

	assert_eq!(lay, assertion)

	// partlay.apply(&mock_disk).unwrap();
	// check if parts would be applied correctly
}

// TODO: add more partitions from https://uapi-group.org/specifications/specs/discoverable_partitions_specification/#partition-names ?

/// Represents GPT partition types which can be used, a subset of https://uapi-group.org/specifications/specs/discoverable_partitions_specification.
/// If the partition type you need isn't in the enum, please file an issue and use the GUID variant.
/// This is not the filesystem which is formatted on the partition.
#[derive(Deserialize, Debug, Clone, Serialize, PartialEq, Eq)]
#[serde(rename_all = "kebab-case")]
pub enum PartitionType {
	// TODO: we need a global arch option in Katsu
	/// Root partition for the target architecture of the build if set, otherwise defaults to the local architecture
	Root,
	/// Root partition for ARM64
	RootArm64,
	/// Root partition for x86_64
	RootX86_64,
	/// Efi system partition
	Esp,
	/// Extended boot loader, defined by the Boot Loader Specification
	Xbootldr,
	/// Swap partition
	Swap,
	/// A generic partition that carries a Linux filesystem
	LinuxGeneric,
	/// MBR header partition for grub-install
	BiosGrub,
	/// An arbitrary GPT partition type GUID/UUIDv4
	#[serde(untagged)]
	Guid(uuid::Uuid),
}

impl PartitionType {
	/// Get the GPT partition type GUID
	fn uuid(&self, target_arch: &str) -> String {
		// https://uapi-group.org/specifications/specs/discoverable_partitions_specification/#partition-names
		match self {
			PartitionType::Root => {
				return match target_arch {
					"x86_64" => PartitionType::RootX86_64.uuid(target_arch),
					"aarch64" => PartitionType::RootArm64.uuid(target_arch),
					_ => unimplemented!(),
				}
			},
			PartitionType::RootArm64 => "b921b045-1df0-41c3-af44-4c6f280d3fae",
			PartitionType::RootX86_64 => "4f68bce3-e8cd-4db1-96e7-fbcaf984b709",
			PartitionType::Esp => "c12a7328-f81f-11d2-ba4b-00a0c93ec93b",
			PartitionType::Xbootldr => "bc13c2ff-59e6-4262-a352-b275fd6f7172",
			PartitionType::Swap => "0657fd6d-a4ab-43c4-84e5-0933c84b4f4f",
			PartitionType::LinuxGeneric => "0fc63daf-8483-4772-8e79-3d69d8477de4",
			PartitionType::BiosGrub => "21686148-6449-6E6F-744E-656564454649",
			PartitionType::Guid(guid) => return guid.to_string(),
		}
		.to_string()
	}
}

/// Represents GPT partition attrbite flags which can be used, from https://uapi-group.org/specifications/specs/discoverable_partitions_specification/#partition-attribute-flags.
#[derive(Deserialize, Debug, Clone, Serialize, PartialEq, Eq)]
#[serde(rename_all = "kebab-case")]
pub enum PartitionFlag {
	/// Disable auto discovery for the partition, preventing automatic mounting
	NoAuto,
	/// Mark partition for mounting as read-only
	ReadOnly,
	/// Enable automatically growing the underlying file system when mounted
	GrowFs,
	/// An arbitrary GPT attribute flag position, 0 - 63
	#[serde(untagged)]
	FlagPosition(u8),
}

impl PartitionFlag {
	/// Get the position offset for this flag
	fn flag_position(&self) -> u8 {
		// https://uapi-group.org/specifications/specs/discoverable_partitions_specification/#partition-attribute-flags
		match &self {
			PartitionFlag::NoAuto => 63,
			PartitionFlag::ReadOnly => 60,
			PartitionFlag::GrowFs => 59,
			PartitionFlag::FlagPosition(position @ 0..=63) => *position,
			_ => unimplemented!(),
		}
	}
}

#[derive(Deserialize, Debug, Clone, Serialize, PartialEq, Eq)]
pub struct Partition {
	pub label: Option<String>,
	/// Partition type
	#[serde(rename = "type")]
	pub partition_type: PartitionType,
	/// GPT partition attribute flags to add
	// todo: maybe represent this as a bitflag number, parted consumes the positions so I'm doing this for now
	pub flags: Option<Vec<PartitionFlag>>,
	/// If not specified, the partition will be created at the end of the disk (100%)
	pub size: Option<ByteSize>,
	/// Filesystem of the partition
	pub filesystem: String,
	/// The mountpoint of the partition
	// todo: make this optional so we can have partitions that aren't mounted
	// and also btrfs subvolumes
	pub mountpoint: String,

	/// Will only be used if the filesystem is btrfs
	#[serde(default)]
	pub subvolumes: Vec<BtrfsSubvolume>,
}

#[derive(Deserialize, Debug, Clone, Serialize, PartialEq, Eq)]
pub struct BtrfsSubvolume {
	pub name: String,
	pub mountpoint: String,
}

#[test]
fn test_bytesize() {
	use std::str::FromStr;

	let size = ByteSize::mib(100);
	println!("{size:#?}");

	let size = ByteSize::from_str("100M").unwrap();
	println!("{:#?}", size.as_u64())
}

fn _default_true() -> bool {
	true
}

/// Image default users configuration
#[derive(Deserialize, Debug, Clone, Serialize)]
pub struct Auth {
	/// Username for the user
	pub username: String,
	/// Passwords are optional, but heavily recommended
	/// Passwords must be hashed with crypt(3) or mkpasswd(1)
	pub password: Option<String>,
	/// Groups to add the user to
	#[serde(default)]
	pub groups: Vec<String>,
	/// Whether to create a home directory for the user
	/// Defaults to true
	#[serde(default = "_default_true")]
	pub create_home: bool,
	/// Shell for the user
	#[serde(default)]
	pub shell: Option<String>,
	/// UID for the user
	#[serde(default)]
	pub uid: Option<u32>,
	/// GID for the user
	#[serde(default)]
	pub gid: Option<u32>,

	/// SSH keys for the user
	/// This will be written to ~/.ssh/authorized_keys
	#[serde(default)]
	pub ssh_keys: Vec<String>,
}

impl Auth {
	pub fn add_to_chroot(&self, chroot: &Path) -> Result<()> {
		// add user to chroot

		let mut args = vec![];

		if let Some(uid) = self.uid {
			args.push("-u".to_string());
			args.push(uid.to_string());
		}
		if let Some(gid) = self.gid {
			args.push("-g".to_string());
			args.push(gid.to_string());
		}

		if let Some(shell) = &self.shell {
			args.push("-s".to_string());
			args.push(shell.to_string());
		}

		if let Some(password) = &self.password {
			args.push("-p".to_string());
			args.push(password.to_string());
		}

		args.push(if self.create_home { "-m" } else { "-M" }.to_string());

		// add groups
		for group in &self.groups {
			args.push("-G".to_string());
			args.push(group.to_string());
		}

		args.push(self.username.to_owned());

		trace!(?args, "useradd args");

		enter_chroot_run(chroot, || {
			info!(?self, "Adding user to chroot");
			std::process::Command::new("useradd").args(&args).status()?;
			Ok(())
		})?;

		// add ssh keys
		if !self.ssh_keys.is_empty() {
			let mut ssh_dir = PathBuf::from(chroot);
			ssh_dir.push("home");
			ssh_dir.push(&self.username);
			ssh_dir.push(".ssh");

			fs::create_dir_all(&ssh_dir)?;

			let mut auth_keys = ssh_dir.clone();
			auth_keys.push("authorized_keys");

			let mut auth_keys_file = fs::File::create(auth_keys)?;

			for key in &self.ssh_keys {
				auth_keys_file.write_all(key.as_bytes())?;
				auth_keys_file.write_all(b"\n")?;
			}
		}

		Ok(())
	}
}

// #[test]
// fn test_recurse() {
// 	// cd tests/ng/recurse

// 	let manifest = Manifest::load_all(PathBuf::from("tests/ng/recurse/manifest.yaml")).unwrap();

// 	println!("{manifest:#?}");

// 	// let ass: Manifest = Manifest { import: vec!["recurse1.yaml", "recurse2.yaml"], distro: Some("RecursiveOS"), out_file: None, dnf: (), scripts: () }
// }
