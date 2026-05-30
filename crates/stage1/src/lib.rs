use std::{
  collections::HashMap,
  env,
  ffi::CString,
  fs::{self, File, OpenOptions, Permissions},
  io::{BufRead, BufReader, Read, Seek, SeekFrom, Write},
  os::{
    fd::AsRawFd,
    unix::{
      fs::{FileTypeExt, MetadataExt, PermissionsExt, symlink},
      io::FromRawFd,
      process::CommandExt,
    },
  },
  path::{Path, PathBuf},
  process::{Command, ExitStatus, Stdio},
  thread,
  time::{Duration, Instant},
};

use anyhow::{Context, Result, bail};
use nix::{
  libc,
  mount::{MsFlags, mount},
  sys::stat::{Mode, SFlag, makedev, mknod},
  unistd::{chdir, chroot, execv, getpid},
};

const LO_FLAGS_READ_ONLY: u32 = 1;
const LO_FLAGS_AUTOCLEAR: u32 = 4;
const LOOP_SET_FD: libc::Ioctl = 0x4C00;
const LOOP_CLR_FD: libc::Ioctl = 0x4C01;
const LOOP_SET_STATUS64: libc::Ioctl = 0x4C04;
const LOOP_CTL_GET_FREE: libc::Ioctl = 0x4C82;
const LO_NAME_SIZE: usize = 64;
const LO_KEY_SIZE: usize = 32;

#[repr(C)]
#[derive(Clone, Copy)]
struct LoopInfo64 {
  lo_device:           u64,
  lo_inode:            u64,
  lo_rdevice:          u64,
  lo_offset:           u64,
  lo_sizelimit:        u64,
  lo_number:           u32,
  lo_encrypt_type:     u32,
  lo_encrypt_key_size: u32,
  lo_flags:            u32,
  lo_file_name:        [u8; LO_NAME_SIZE],
  lo_crypt_name:       [u8; LO_NAME_SIZE],
  lo_encrypt_key:      [u8; LO_KEY_SIZE],
  lo_init:             [u64; 2],
}

#[derive(Debug)]
enum DeviceManager {
  Udev {
    udevd:   PathBuf,
    udevadm: PathBuf,
    rules:   Option<PathBuf>,
  },
  Mdev {
    mdev: PathBuf,
    conf: Option<PathBuf>,
  },
  Mdevd {
    mdevd:    PathBuf,
    coldplug: PathBuf,
    conf:     Option<PathBuf>,
  },
}

impl Default for DeviceManager {
  fn default() -> Self {
    Self::Udev {
      udevd:   PathBuf::from("systemd-udevd"),
      udevadm: PathBuf::from("udevadm"),
      rules:   None,
    }
  }
}

impl DeviceManager {
  fn from_env(extra_utils: Option<&Path>) -> Self {
    match env::var("DEVICE_MANAGER").as_deref() {
      Ok("mdev") => {
        Self::Mdev {
          mdev: env::var("MDEV_BINARY").map(PathBuf::from).unwrap_or_else(
            |_| {
              extra_utils
                .map_or_else(|| PathBuf::from("mdev"), |u| u.join("bin/mdev"))
            },
          ),
          conf: env::var("MDEV_CONF").ok().map(PathBuf::from),
        }
      },
      Ok("mdevd") => {
        Self::Mdevd {
          mdevd:    env::var("MDEVD_BINARY").map(PathBuf::from).unwrap_or_else(
            |_| {
              extra_utils
                .map_or_else(|| PathBuf::from("mdevd"), |u| u.join("bin/mdevd"))
            },
          ),
          coldplug: env::var("MDEVD_COLDPLUG_BINARY")
            .map(PathBuf::from)
            .unwrap_or_else(|_| {
              extra_utils.map_or_else(
                || PathBuf::from("mdevd-coldplug"),
                |u| u.join("bin/mdevd-coldplug"),
              )
            }),
          conf:     env::var("MDEV_CONF").ok().map(PathBuf::from),
        }
      },
      _ => {
        Self::Udev {
          udevd:   env::var("UDEV_BINARY").map(PathBuf::from).unwrap_or_else(
            |_| {
              extra_utils.map_or_else(
                || PathBuf::from("systemd-udevd"),
                |u| u.join("bin/systemd-udevd"),
              )
            },
          ),
          udevadm: env::var("UDEVADM_BINARY")
            .map(PathBuf::from)
            .unwrap_or_else(|_| {
              extra_utils.map_or_else(
                || PathBuf::from("udevadm"),
                |u| u.join("bin/udevadm"),
              )
            }),
          rules:   env::var("udevRules").ok().map(PathBuf::from),
        }
      },
    }
  }

  fn start(&self) -> Result<()> {
    log_message("Starting device manager...", true);
    match self {
      Self::Udev { udevd, rules, .. } => {
        if let Some(rules) = rules {
          let rules_dir = Path::new("/etc/udev/rules.d");
          fs::create_dir_all(rules_dir)?;
          if rules.is_dir() {
            for entry in fs::read_dir(rules)? {
              let entry = entry?;
              fs::copy(entry.path(), rules_dir.join(entry.file_name()))?;
            }
          }
        }
        let conf = Path::new("/etc/udev/udev.conf");
        if !conf.exists() {
          fs::create_dir_all(conf.parent().unwrap())?;
          fs::write(conf, "udev_log=err\n")?;
        }
        Command::new(udevd)
          .arg("--daemon")
          .status()
          .with_context(|| {
            format!("Failed to start udevd: {}", udevd.display())
          })?;
      },
      Self::Mdev { mdev, conf } => {
        install_mdev_conf(conf.as_deref())?;
        // -d: listen for kernel hotplug events; -s: initial coldplug scan.
        Command::new(mdev).arg("-d").status().with_context(|| {
          format!("Failed to start mdev: {}", mdev.display())
        })?;
        Command::new(mdev).arg("-s").status().with_context(|| {
          format!("mdev coldplug scan failed: {}", mdev.display())
        })?;
      },
      Self::Mdevd {
        mdevd,
        coldplug,
        conf,
      } => {
        install_mdev_conf(conf.as_deref())?;
        let mut fds = [-1; 2];
        if unsafe { libc::pipe(fds.as_mut_ptr()) } != 0 {
          bail!("Failed to create mdevd readiness pipe");
        }
        let read_fd = fds[0];
        let write_fd = fds[1];

        let mut command = Command::new(mdevd);
        command.args(["-D", "3", "-O", "4"]);
        unsafe {
          command.pre_exec(move || {
            if libc::dup2(write_fd, 3) == -1 {
              return Err(std::io::Error::last_os_error());
            }
            if write_fd != 3 {
              libc::close(write_fd);
            }
            libc::close(read_fd);
            Ok(())
          });
        }

        let mut child = command.spawn().with_context(|| {
          format!("Failed to start mdevd: {}", mdevd.display())
        })?;
        unsafe {
          libc::close(write_fd);
        }

        let mut readiness = unsafe { File::from_raw_fd(read_fd) };
        let mut byte = [0u8; 1];
        if let Err(e) = readiness.read_exact(&mut byte) {
          let _ = child.wait();
          return Err(e).with_context(|| {
            format!("mdevd did not become ready: {}", mdevd.display())
          });
        }

        let status = Command::new(coldplug)
          .args(["-O", "4"])
          .status()
          .with_context(|| {
            format!("mdevd coldplug failed: {}", coldplug.display())
          })?;
        if !status.success() {
          bail!("mdevd coldplug exited with status: {status}");
        }
      },
    }
    log_message("Device manager started", true);
    Ok(())
  }

  fn trigger(&self) -> Result<()> {
    log_message("Triggering device events...", true);
    match self {
      Self::Udev { udevadm, .. } => {
        Command::new(udevadm)
          .args(["trigger", "--action=add"])
          .status()
          .context("Failed to trigger udev events")?;
      },
      Self::Mdev { .. } => {
        // Covered by the -s scan in start(); nothing more to do.
      },
      Self::Mdevd { .. } => {
        // Covered by the synchronous mdevd-coldplug in start().
      },
    }
    Ok(())
  }

  fn settle(&self) -> Result<()> {
    log_message("Waiting for device manager to settle...", true);
    match self {
      Self::Udev { udevadm, .. } => {
        let status = Command::new(udevadm)
          .args(["settle", "--timeout=30"])
          .status()
          .context("Failed to wait for udev")?;
        if !status.success() {
          log_message("Warning: udev settle timed out", true);
        }
      },
      Self::Mdev { .. } => {
        // mdev -s is synchronous; no separate settle step.
      },
      Self::Mdevd { .. } => {
        // mdevd-coldplug -O waits for mdevd to process the final coldplug
        // event.
      },
    }
    Ok(())
  }

  // Best-effort block re-trigger used in device-polling loops; errors are
  // swallowed.
  fn retrigger_block(&self) {
    match self {
      Self::Udev { udevadm, .. } => {
        let _ = Command::new(udevadm)
          .args(["trigger", "--subsystem-match=block", "--action=change"])
          .status();
        let _ = Command::new(udevadm)
          .args(["settle", "--timeout=3"])
          .status();
      },
      Self::Mdev { mdev, .. } => {
        let _ = Command::new(mdev).arg("-s").status();
      },
      Self::Mdevd { coldplug, .. } => {
        let _ = Command::new(coldplug).args(["-O", "4"]).status();
      },
    }
  }

  fn stop(&self) {
    log_message("Stopping device manager...", true);
    match self {
      Self::Udev { udevadm, .. } => {
        let _ = Command::new(udevadm).args(["control", "--exit"]).status();
      },
      Self::Mdev { mdev, .. } => {
        let name = mdev.file_name().unwrap_or(mdev.as_os_str());
        let _ = Command::new("pkill").arg(name).status();
      },
      Self::Mdevd { mdevd, .. } => {
        let name = mdevd.file_name().unwrap_or(mdevd.as_os_str());
        let _ = Command::new("pkill").arg("-TERM").arg(name).status();
      },
    }
  }

  // Write /dev/root udev rule so systemd's mount-unit generator can find the
  // root device. mdev-style managers do not process /run/udev/rules.d, so this
  // is a no-op for those backends.
  fn write_dev_root_rule(&self, target_root: &Path) -> Result<()> {
    match self {
      Self::Udev { .. } => write_dev_root_udev_rule(target_root),
      Self::Mdev { .. } | Self::Mdevd { .. } => Ok(()),
    }
  }
}

fn install_mdev_conf(conf: Option<&Path>) -> Result<()> {
  let dest = Path::new("/etc/mdev.conf");
  fs::create_dir_all(dest.parent().unwrap())?;
  if let Some(conf) = conf {
    fs::copy(conf, dest).with_context(|| {
      format!(
        "Failed to copy mdev.conf from {} to {}",
        conf.display(),
        dest.display()
      )
    })?;
  } else if !dest.exists() {
    fs::write(dest, "")?;
  }
  Ok(())
}

#[derive(Debug, Default)]
struct Stage1Config {
  target_root:          PathBuf,
  extra_utils:          Option<PathBuf>,
  kernel_modules:       Vec<String>,
  resume_device:        Option<String>,
  resume_devices:       Vec<String>,
  fs_info:              Option<PathBuf>,
  pre_fail_commands:    Option<PathBuf>,
  pre_device_commands:  Option<PathBuf>,
  pre_lvm_commands:     Option<PathBuf>,
  post_device_commands: Option<PathBuf>,
  post_resume_commands: Option<PathBuf>,
  post_mount_commands:  Option<PathBuf>,
  early_mount_script:   Option<PathBuf>,
  link_units:           Option<PathBuf>,
  check_journaling_fs:  bool,
  set_host_id:          Option<String>,
  distro_name:          String,
  device_manager:       DeviceManager,
  link_units_dest:      PathBuf,
}

#[derive(Debug, Default)]
struct KernelCmdline {
  root:          Option<String>,
  init:          Option<String>,
  console:       Vec<String>,
  shell_on_fail: bool,
  debug1:        bool,
  debug1devices: bool,
  debug1mounts:  bool,
  debug:         bool,
  trace:         bool,
  panic_on_fail: bool,
  no_modprobe:   bool,
  copy_to_ram:   bool,
  persistence:   Option<String>,
  resume:        Option<String>,
  boot_gfx_mode: Option<String>,
  quiet:         bool,
  params:        HashMap<String, Option<String>>,
}

#[derive(Debug, Clone)]
struct FsInfo {
  device:     String,
  mountpoint: PathBuf,
  fstype:     String,
  options:    Vec<String>,
}

#[derive(Debug, Clone)]
struct MountOptions {
  raw: Vec<String>,
}

impl MountOptions {
  fn from_vec(raw: Vec<String>) -> Self {
    Self { raw }
  }

  fn from_slice(raw: &[String]) -> Self {
    Self { raw: raw.to_vec() }
  }

  fn from_csv(raw: &str) -> Self {
    Self {
      raw: raw
        .split(',')
        .filter(|opt| !opt.is_empty())
        .map(String::from)
        .collect(),
    }
  }

  fn parse_for_mount(&self) -> (MsFlags, Option<String>) {
    let filtered = self
      .raw
      .iter()
      .map(String::as_str)
      .filter(|opt| !opt.is_empty() && !opt.starts_with("x-") && *opt != "loop")
      .collect::<Vec<_>>();
    parse_mount_options(filtered.iter().copied())
  }

  fn raw_for_mount(&self) -> Vec<&str> {
    self
      .raw
      .iter()
      .map(String::as_str)
      .filter(|opt| !opt.is_empty() && !opt.starts_with("x-") && *opt != "loop")
      .collect()
  }
}

#[derive(Debug, Clone)]
struct Mount<'a> {
  source:  &'a str,
  target:  &'a Path,
  fstype:  Option<&'a str>,
  options: MountOptions,
}

impl<'a> Mount<'a> {
  fn new(
    source: &'a str,
    target: &'a Path,
    fstype: Option<&'a str>,
    options: MountOptions,
  ) -> Self {
    Self {
      source,
      target,
      fstype,
      options,
    }
  }

  fn source_path(&self) -> &Path {
    Path::new(self.source)
  }

  fn mount_fstype(&self) -> Option<&str> {
    self
      .fstype
      .filter(|value| !value.is_empty() && *value != "auto")
  }

  fn uses_loop_device(&self) -> bool {
    fs::metadata(self.source_path())
      .map(|metadata| metadata.file_type().is_file())
      .unwrap_or(false)
  }

  fn is_bind_mount(&self) -> bool {
    self.fstype == Some("bind")
      || self
        .options
        .raw
        .iter()
        .any(|opt| opt == "bind" || opt == "rbind")
  }

  fn is_recursive_bind_mount(&self) -> bool {
    self.options.raw.iter().any(|opt| opt == "rbind")
  }

  fn apply_filesystem(&self, dm: &DeviceManager) -> Result<()> {
    if self.is_bind_mount() {
      self.mount_bind().with_context(|| {
        format!("Failed to bind mount {} to {:?}", self.source, self.target)
      })?;
      self.remount_bind().with_context(|| {
        format!("Failed to remount {:?} with security options", self.target)
      })?;
      return Ok(());
    }

    match self.fstype {
      Some("zfs") => {
        log_message(
          &format!(
            "Skipping mount of {} (handled by kernel)",
            self.fstype.unwrap_or("auto")
          ),
          true,
        );
        return Ok(());
      },
      Some("bcachefs") => {
        for component in self.source.split(':') {
          wait_for_device(component, 30, dm).ok();
        }
        return mount_bcachefs(
          self.source,
          self.target,
          &self.options.raw,
          Some(Duration::from_secs(30)),
        );
      },
      Some("overlay") => {
        self.mount_overlay().with_context(|| {
          format!("Failed to mount overlay at {:?}", self.target)
        })?;
      },
      _ => self.apply()?,
    }

    Ok(())
  }

  fn apply(&self) -> Result<()> {
    let (flags, data) = self.options.parse_for_mount();
    if self.uses_loop_device() {
      log_message(
        &format!(
          "Mounting regular file {} via loop device at {:?}",
          self.source, self.target
        ),
        true,
      );
      let (loop_device, _loop_fd) = self
        .attach_loop_device(flags.contains(MsFlags::MS_RDONLY))
        .with_context(|| {
          format!("Failed to configure loop device for {}", self.source)
        })?;

      // Keep _loop_fd alive until *after* mount() so LO_FLAGS_AUTOCLEAR
      // does not fire before the kernel has taken its own reference via
      // blkdev_get_by_path -> lo_open.
      self
        .mount_with_source(loop_device.as_str(), flags, data.as_deref())
        .with_context(|| {
          format!(
            "Failed to mount {} ({}) via {} at {:?}",
            self.source,
            self.mount_fstype().unwrap_or("auto"),
            loop_device,
            self.target
          )
        })?;
      return Ok(());
    }

    self
      .mount_with_source(self.source, flags, data.as_deref())
      .with_context(|| {
        format!(
          "Failed to mount {} ({}) at {:?}",
          self.source,
          self.mount_fstype().unwrap_or("auto"),
          self.target
        )
      })
  }

  fn mount_with_source(
    &self,
    source: &str,
    flags: MsFlags,
    data: Option<&str>,
  ) -> Result<()> {
    mount(Some(source), self.target, self.mount_fstype(), flags, data)
      .map_err(Into::into)
  }

  fn mount_bind(&self) -> Result<()> {
    let flags = if self.is_recursive_bind_mount() {
      MsFlags::MS_BIND | MsFlags::MS_REC
    } else {
      MsFlags::MS_BIND
    };

    mount(
      Some(self.source),
      self.target,
      None::<&str>,
      flags,
      None::<&str>,
    )
    .map_err(Into::into)
  }

  fn mount_overlay(&self) -> Result<()> {
    let filtered_opts = self.options.raw_for_mount();
    for opt in &filtered_opts {
      for prefix in &["upperdir=", "workdir="] {
        if let Some(path) = opt.strip_prefix(prefix) {
          fs::create_dir_all(path).ok();
        }
      }
    }
    mount(
      Some("overlay"),
      self.target,
      Some("overlay"),
      MsFlags::empty(),
      Some(filtered_opts.join(",").as_str()),
    )
    .map_err(Into::into)
  }

  fn remount_bind(&self) -> Result<()> {
    let (flags, data) = self.options.parse_for_mount();
    mount(
      None::<&str>,
      self.target,
      None::<&str>,
      MsFlags::MS_REMOUNT | MsFlags::MS_BIND | flags,
      data.as_deref(),
    )
    .map_err(Into::into)
  }

  fn attach_loop_device(&self, read_only: bool) -> Result<(String, File)> {
    load_module("loop").ok();

    let backing_file = OpenOptions::new()
      .read(true)
      .write(!read_only)
      .open(self.source_path())
      .with_context(|| {
        format!("Failed to open loop backing file {}", self.source)
      })?;
    let loop_control = OpenOptions::new()
      .read(true)
      .write(true)
      .open("/dev/loop-control")
      .context("Failed to open /dev/loop-control")?;

    let loop_number =
      unsafe { libc::ioctl(loop_control.as_raw_fd(), LOOP_CTL_GET_FREE, 0) };
    if loop_number < 0 {
      return Err(std::io::Error::last_os_error())
        .context("LOOP_CTL_GET_FREE failed");
    }

    let loop_path = format!("/dev/loop{loop_number}");
    let loop_device = OpenOptions::new()
      .read(true)
      .write(true)
      .open(&loop_path)
      .with_context(|| format!("Failed to open loop device {loop_path}"))?;

    let set_fd = unsafe {
      libc::ioctl(
        loop_device.as_raw_fd(),
        LOOP_SET_FD,
        backing_file.as_raw_fd(),
      )
    };
    if set_fd < 0 {
      return Err(std::io::Error::last_os_error())
        .with_context(|| format!("LOOP_SET_FD failed for {loop_path}"));
    }

    let mut loop_info: LoopInfo64 = unsafe { std::mem::zeroed() };
    loop_info.lo_flags = LO_FLAGS_AUTOCLEAR;
    if read_only {
      loop_info.lo_flags |= LO_FLAGS_READ_ONLY;
    }
    let file_name = self.source_path().as_os_str().as_encoded_bytes();
    let copy_len = file_name.len().min(loop_info.lo_file_name.len());
    loop_info.lo_file_name[..copy_len].copy_from_slice(&file_name[..copy_len]);

    let set_status = unsafe {
      libc::ioctl(loop_device.as_raw_fd(), LOOP_SET_STATUS64, &loop_info)
    };
    if set_status < 0 {
      let err = std::io::Error::last_os_error();
      let _ = unsafe { libc::ioctl(loop_device.as_raw_fd(), LOOP_CLR_FD, 0) };
      return Err(err)
        .with_context(|| format!("LOOP_SET_STATUS64 failed for {loop_path}"));
    }

    Ok((loop_path, loop_device))
  }
}

impl KernelCmdline {
  fn parse() -> Result<Self> {
    let content = fs::read_to_string("/proc/cmdline")
      .context("Failed to read /proc/cmdline")?;

    let mut cmdline = Self::default();

    for token in content.split_whitespace() {
      let mut parts = token.splitn(2, '=');
      let key = parts.next().unwrap_or("");
      let value = parts.next().map(String::from);

      match key {
        "root" => {
          // Rewrite root=UUID=... and root=LABEL=... (common bootloader
          // cmdline forms) to the udev-managed /dev/disk/by-* paths.
          cmdline.root = value.map(rewrite_uuid_label);
        },
        "init" => cmdline.init = value,
        "console" => {
          if let Some(v) = value {
            cmdline.console.push(v);
          }
        },
        "boot.shell_on_fail" => {
          cmdline.shell_on_fail =
            value.as_ref().is_none_or(|v| v != "0" && v != "false");
        },
        "boot.debug1" => {
          cmdline.debug1 =
            value.as_ref().is_none_or(|v| v != "0" && v != "false");
        },
        "boot.debug1devices" => {
          cmdline.debug1devices =
            value.as_ref().is_none_or(|v| v != "0" && v != "false");
        },
        "boot.debug1mounts" => {
          cmdline.debug1mounts =
            value.as_ref().is_none_or(|v| v != "0" && v != "false");
        },
        "boot.debug" => {
          cmdline.debug =
            value.as_ref().is_none_or(|v| v != "0" && v != "false");
        },
        "boot.trace" => {
          cmdline.trace =
            value.as_ref().is_none_or(|v| v != "0" && v != "false");
        },
        "boot.panic_on_fail" => {
          cmdline.panic_on_fail =
            value.as_ref().is_none_or(|v| v != "0" && v != "false");
        },
        "boot.no_modprobe" => {
          cmdline.no_modprobe =
            value.as_ref().is_none_or(|v| v != "0" && v != "false");
        },
        "boot.copytoram" => {
          cmdline.copy_to_ram =
            value.as_ref().is_none_or(|v| v != "0" && v != "false");
        },
        "boot.persistence" => cmdline.persistence = value,
        "resume" => cmdline.resume = value.map(rewrite_uuid_label),
        "boot.gfx_mode" => cmdline.boot_gfx_mode = value,
        "quiet" => {
          cmdline.quiet =
            value.as_ref().is_none_or(|v| v != "0" && v != "false");
        },
        _ => {
          cmdline.params.insert(key.to_string(), value);
        },
      }
    }

    Ok(cmdline)
  }

  fn get(&self, key: &str) -> Option<&String> {
    self.params.get(key).and_then(|v| v.as_ref())
  }
}

/// Rewrite `UUID=<hex>` to `/dev/disk/by-uuid/<hex>` and
/// `LABEL=<name>` to `/dev/disk/by-label/<name>` so the device-wait
/// loop can canonicalise them through udev symlinks. Unknown
/// prefixes are returned unchanged.
fn rewrite_uuid_label(value: String) -> String {
  if let Some(rest) = value.strip_prefix("UUID=") {
    format!("/dev/disk/by-uuid/{rest}")
  } else if let Some(rest) = value.strip_prefix("LABEL=") {
    format!("/dev/disk/by-label/{rest}")
  } else {
    value
  }
}

impl Stage1Config {
  fn from_env() -> Self {
    let extra_utils: Option<PathBuf> =
      env::var("extraUtils").ok().map(PathBuf::from);
    let device_manager = DeviceManager::from_env(extra_utils.as_deref());

    Self {
      target_root: env::var("targetRoot")
        .map_or_else(|_| PathBuf::from("/mnt-root"), PathBuf::from),
      extra_utils,
      kernel_modules: env::var("kernelModules")
        .map(|mods| mods.split_whitespace().map(String::from).collect())
        .unwrap_or_default(),
      resume_device: env::var("resumeDevice").ok(),
      resume_devices: env::var("resumeDevices")
        .map(|devs| devs.split_whitespace().map(String::from).collect())
        .unwrap_or_default(),
      fs_info: env::var("fsInfo").ok().map(PathBuf::from),
      pre_fail_commands: env::var("preFailCommands").ok().map(PathBuf::from),
      pre_device_commands: env::var("preDeviceCommands")
        .ok()
        .map(PathBuf::from),
      pre_lvm_commands: env::var("preLVMCommands").ok().map(PathBuf::from),
      post_device_commands: env::var("postDeviceCommands")
        .ok()
        .map(PathBuf::from),
      post_resume_commands: env::var("postResumeCommands")
        .ok()
        .map(PathBuf::from),
      post_mount_commands: env::var("postMountCommands")
        .ok()
        .map(PathBuf::from),
      early_mount_script: env::var("earlyMountScript").ok().map(PathBuf::from),
      link_units: env::var("linkUnits").ok().map(PathBuf::from),
      check_journaling_fs: env::var("checkJournalingFS")
        .map(|v| v != "0" && v != "false")
        .unwrap_or(true),
      set_host_id: env::var("HOST_ID").ok(),
      distro_name: env::var("distroName")
        .unwrap_or_else(|_| "NixOS".to_string()),
      device_manager,
      link_units_dest: env::var("LINK_UNITS_DEST")
        .map(PathBuf::from)
        .unwrap_or_else(|_| PathBuf::from("/etc/systemd/network")),
    }
  }
}

fn log_message(msg: &str, to_kmsg: bool) {
  eprintln!("stage-1-init: {msg}");

  if to_kmsg
    && let Ok(mut file) = OpenOptions::new().write(true).open("/dev/kmsg")
  {
    // Printk priority prefix: <3>=error, <4>=warn, <6>=info.
    // Messages prefixed with "FAIL:" or "Warning:" get their respective level;
    // everything else is informational.
    let level = if msg.starts_with("FAIL:") {
      "<3>"
    } else if msg.starts_with("Warning:") {
      "<4>"
    } else {
      "<6>"
    };
    let _ = writeln!(file, "{level}stage-1-init: {msg}");
  }
}

fn setup_environment(extra_utils: Option<&Path>) -> Result<()> {
  // Set PATH
  let path = if let Some(utils) = extra_utils {
    format!("{}/bin:{}/sbin", utils.display(), utils.display())
  } else {
    "/bin:/sbin:/usr/bin:/usr/sbin".to_string()
  };
  // SAFETY: single-threaded at this point; no other threads can observe the
  // environment change.
  unsafe {
    env::set_var("PATH", &path);
  }

  // Export LD_LIBRARY_PATH so extraUtils binaries find their bundled libs.
  // Matches `export LD_LIBRARY_PATH=@extraUtils@/lib` from stage-1-init.sh
  // which is load-bearing for cryptsetup/lvm/mdadm/btrfs-progs builds that
  // the initrd ships with non-standard rpath-less linkage.
  if let Some(utils) = extra_utils {
    let lib_dir = utils.join("lib");
    // SAFETY: same rationale as the PATH set-var above.
    unsafe {
      env::set_var("LD_LIBRARY_PATH", &lib_dir);
    }
  }

  // Create /bin and /sbin symlinks if extra_utils is provided
  if let Some(utils) = extra_utils {
    let bin_dir = Path::new("/bin");
    let sbin_dir = Path::new("/sbin");

    if !bin_dir.exists() {
      let _ = fs::remove_file(bin_dir);
      symlink(utils.join("bin"), bin_dir)
        .context("Failed to create /bin symlink")?;
    }

    if !sbin_dir.exists() {
      let _ = fs::remove_file(sbin_dir);
      symlink(utils.join("sbin"), sbin_dir)
        .context("Failed to create /sbin symlink")?;
    }
  }

  Ok(())
}

fn create_directories() -> Result<()> {
  let dirs = [
    "/etc",
    "/dev",
    "/proc",
    "/sys",
    "/run",
    "/tmp",
    "/mnt",
    "/mnt-root",
    "/var",
    "/var/log",
  ];

  for dir in &dirs {
    fs::create_dir_all(dir)
      .with_context(|| format!("Failed to create directory: {dir}"))?;
  }

  Ok(())
}

fn create_essential_devices() -> Result<()> {
  // Create /dev/console if it doesn't exist
  if !Path::new("/dev/console").exists() {
    mknod(
      "/dev/console",
      SFlag::S_IFCHR,
      Mode::from_bits_truncate(0o600),
      makedev(5, 1),
    )
    .context("Failed to create /dev/console")?;
  }

  // Create /dev/null if it doesn't exist
  if !Path::new("/dev/null").exists() {
    mknod(
      "/dev/null",
      SFlag::S_IFCHR,
      Mode::from_bits_truncate(0o666),
      makedev(1, 3),
    )
    .context("Failed to create /dev/null")?;
  }

  // Create /dev/kmsg if it doesn't exist
  if !Path::new("/dev/kmsg").exists() {
    mknod(
      "/dev/kmsg",
      SFlag::S_IFCHR,
      Mode::from_bits_truncate(0o600),
      makedev(1, 11),
    )
    .ok(); // Non-critical
  }

  Ok(())
}

fn create_essential_files() -> Result<()> {
  // Create empty /etc/fstab
  let fstab = Path::new("/etc/fstab");
  if !fstab.exists() {
    fs::write(fstab, "# Initial fstab\n")
      .context("Failed to create /etc/fstab")?;
  }

  // Create /etc/mtab as symlink to /proc/mounts
  let mtab = Path::new("/etc/mtab");
  if !mtab.exists() && !mtab.is_symlink() {
    let _ = fs::remove_file(mtab);
    symlink("/proc/mounts", mtab)
      .context("Failed to create /etc/mtab symlink")?;
  }

  // Create /var/log/messages for logging
  let log_file = Path::new("/var/log/messages");
  if !log_file.exists() {
    fs::write(log_file, "").context("Failed to create /var/log/messages")?;
  }

  Ok(())
}

fn mount_essential_filesystems() -> Result<()> {
  // Mount proc
  let proc_path = Path::new("/proc");
  if !is_mounted(proc_path) {
    fs::create_dir_all(proc_path)?;
    mount(
      Some("proc"),
      proc_path,
      Some("proc"),
      MsFlags::MS_NOSUID | MsFlags::MS_NOEXEC | MsFlags::MS_NODEV,
      None::<&str>,
    )
    .context("Failed to mount /proc")?;
  }

  // Mount sysfs
  let sys_path = Path::new("/sys");
  if !is_mounted(sys_path) {
    fs::create_dir_all(sys_path)?;
    mount(
      Some("sysfs"),
      sys_path,
      Some("sysfs"),
      MsFlags::MS_NOSUID | MsFlags::MS_NOEXEC | MsFlags::MS_NODEV,
      None::<&str>,
    )
    .context("Failed to mount /sys")?;
  }

  // Mount devtmpfs
  let dev_path = Path::new("/dev");
  if !is_mounted(dev_path) {
    mount(
      Some("devtmpfs"),
      dev_path,
      Some("devtmpfs"),
      MsFlags::MS_NOSUID | MsFlags::MS_STRICTATIME,
      Some("mode=0755"),
    )
    .context("Failed to mount devtmpfs")?;
  }

  // Mount devpts
  let devpts_path = Path::new("/dev/pts");
  if !is_mounted(devpts_path) {
    fs::create_dir_all(devpts_path)?;
    mount(
      Some("devpts"),
      devpts_path,
      Some("devpts"),
      MsFlags::MS_NOSUID | MsFlags::MS_NOEXEC,
      Some("mode=0620,gid=5"),
    )
    .ok(); // Non-critical
  }

  // Mount tmpfs on /run
  let run_path = Path::new("/run");
  if !is_mounted(run_path) {
    mount(
      Some("tmpfs"),
      run_path,
      Some("tmpfs"),
      MsFlags::MS_NOSUID | MsFlags::MS_NODEV | MsFlags::MS_STRICTATIME,
      Some("mode=0755"),
    )
    .ok(); // May already be mounted
  }

  // Mount tmpfs on /tmp
  let tmp_path = Path::new("/tmp");
  if !is_mounted(tmp_path) {
    mount(
      Some("tmpfs"),
      tmp_path,
      Some("tmpfs"),
      MsFlags::MS_NOSUID | MsFlags::MS_NODEV | MsFlags::MS_STRICTATIME,
      Some("mode=1777"),
    )
    .ok();
  }

  Ok(())
}

fn is_mounted(path: &Path) -> bool {
  if let Ok(file) = File::open("/proc/mounts") {
    let reader = BufReader::new(file);
    for line in reader.lines().map_while(Result::ok) {
      let parts: Vec<&str> = line.split_whitespace().collect();
      if parts.len() >= 2 && parts[1] == path.to_string_lossy().as_ref() {
        return true;
      }
    }
  }
  false
}

fn is_device_mounted(device: &str) -> bool {
  let canonical = fs::canonicalize(device).ok();
  let canonical_str = canonical.as_deref().and_then(|p| p.to_str());
  if let Ok(file) = File::open("/proc/mounts") {
    let reader = BufReader::new(file);
    for line in reader.lines().map_while(Result::ok) {
      let parts = line.split_whitespace().collect::<Vec<&str>>();
      if parts.len() >= 2
        && (parts[0] == device || Some(parts[0]) == canonical_str)
      {
        return true;
      }
    }
  }
  false
}

// Wait up to timeout_secs for device to appear; re-triggers the device manager
// periodically.
fn wait_for_device(
  device: &str,
  timeout_secs: u64,
  dm: &DeviceManager,
) -> Result<()> {
  let device_path = Path::new(device);
  let start = Instant::now();
  let timeout = Duration::from_secs(timeout_secs);
  let mut last_retrigger = Instant::now();

  log_message(&format!("Waiting for device: {device}"), true);

  while start.elapsed() < timeout {
    if device_is_ready(device_path) {
      log_message(&format!("Device {device} is ready"), true);
      return Ok(());
    }

    if last_retrigger.elapsed() >= Duration::from_secs(5) {
      dm.retrigger_block();
      last_retrigger = Instant::now();
    }

    thread::sleep(Duration::from_millis(100));
  }

  bail!("Failed to wait for root device")
}

fn device_is_ready(device_path: &Path) -> bool {
  if device_path.exists()
    && let Ok(metadata) = fs::metadata(device_path)
  {
    return metadata.file_type().is_block_device();
  }
  false
}

fn load_module(module: &str) -> Result<()> {
  log_message(&format!("Loading module: {module}"), true);

  let status = Command::new("modprobe")
    .arg("-q")
    .arg(module)
    .status()
    .context("Failed to run modprobe")?;

  if !status.success() {
    log_message(&format!("Warning: Failed to load module: {module}"), true);
  }

  Ok(())
}

fn load_kernel_modules(modules: &[String], no_modprobe: bool) -> Result<()> {
  if no_modprobe {
    log_message("Skipping module loading (boot.no_modprobe)", true);
    return Ok(());
  }

  for module in modules {
    load_module(module).ok();
  }

  Ok(())
}

fn setup_link_units(link_units: &Path, dest: &Path) -> Result<()> {
  if let Some(parent) = dest.parent() {
    fs::create_dir_all(parent)?;
  }
  if dest.is_symlink() || dest.exists() {
    fs::remove_file(dest)?;
  }
  symlink(link_units, dest)?;
  Ok(())
}

fn activate_lvm() -> Result<()> {
  log_message("Activating LVM volumes...", true);

  // extraUtils ships `lvm` (the multicall binary from lvm2) but not a
  // standalone `vgchange`, matching stage-1.nix which only copies
  // dmsetup + lvm. Invoking `vgchange` directly therefore fails on the
  // standard initrd; we must go through the multicall entry point just
  // like `lvm vgchange -ay` in stage-1-init.sh.
  let status = Command::new("lvm").args(["vgchange", "-ay"]).status();

  match status {
    Ok(s) if s.success() => log_message("LVM volumes activated", true),
    Ok(_) => {
      log_message("No LVM volumes found or activation failed", true);
    },
    Err(_) => {
      log_message("lvm not available, skipping LVM activation", true);
    },
  }

  Ok(())
}

// Read the filesystem type for a block device from udev's property database.
fn udev_fs_type(device: &str) -> Option<String> {
  let meta = fs::metadata(device).ok()?;
  let rdev = meta.rdev();
  let major = libc::major(rdev);
  let minor = libc::minor(rdev);
  let content =
    fs::read_to_string(format!("/run/udev/data/b{major}:{minor}")).ok()?;
  content.lines().find_map(|line| {
    let fstype = line.strip_prefix("E:ID_FS_TYPE=")?;
    if fstype.is_empty() {
      None
    } else {
      Some(fstype.to_string())
    }
  })
}

fn has_swap_signature(device: &str) -> bool {
  // Swap header magic ("SWAPSPACE2") sits at the last 10 bytes of page 0.
  let page_size_raw = unsafe { libc::sysconf(libc::_SC_PAGESIZE) };
  if page_size_raw <= 0 {
    return false;
  }
  let offset = (page_size_raw as u64).saturating_sub(10);
  let Ok(mut f) = File::open(device) else {
    return false;
  };
  if f.seek(SeekFrom::Start(offset)).is_err() {
    return false;
  }
  let mut magic = [0u8; 10];
  f.read_exact(&mut magic).is_ok()
    && (&magic == b"SWAPSPACE2" || &magic == b"PAGESIZE\0\0")
}

/// Handle resume from hibernation
fn handle_resume(
  resume_device: Option<&str>,
  resume_devices: &[String],
) -> Result<()> {
  let mut resume_dev: Option<String> = None;

  // First check explicit resume device from cmdline
  if let Some(dev) = resume_device
    && Path::new(dev).exists()
  {
    resume_dev = Some(dev.to_string());
  }

  // If not found, check the configured resume devices
  if resume_dev.is_none() {
    for dev in resume_devices {
      if Path::new(dev).exists() && has_swap_signature(dev) {
        resume_dev = Some(dev.clone());
        break;
      }
    }
  }

  let resume_dev = if let Some(d) = resume_dev {
    d
  } else {
    log_message("No resume device found", true);
    return Ok(());
  };

  log_message(&format!("Attempting resume from: {resume_dev}"), true);

  // Try to resume
  let resume_path = Path::new(&resume_dev);
  let resume_result = if resume_path.exists() {
    // The kernel expects major:minor (decimal), not the device path.
    // Use stat to extract the raw device number and decompose it.
    fs::metadata(resume_path)
      .context("Failed to stat resume device")
      .and_then(|meta| {
        let rdev = meta.rdev();
        let major = libc::major(rdev);
        let minor = libc::minor(rdev);
        fs::write("/sys/power/resume", format!("{major}:{minor}"))
          .context("Failed to write to /sys/power/resume")
      })
  } else {
    Err(anyhow::anyhow!(
      "Resume device does not exist: {resume_dev}"
    ))
  };

  if let Err(e) = resume_result {
    log_message(
      &format!("Resume failed (this is normal if not resuming): {e}"),
      true,
    );
  } else {
    log_message("Resume completed", true);
  }

  Ok(())
}

fn parse_fs_info(path: &Path) -> Result<Vec<FsInfo>> {
  let mut fs_infos = Vec::new();

  if !path.exists() {
    return Ok(fs_infos);
  }

  let content =
    fs::read_to_string(path).context("Failed to read fsInfo file")?;

  // Format: 4 lines per entry - mountPoint, device, fsType, options
  // (comma-separated). This matches how nixpkgs' stage-1.nix writes the file.
  let mut lines = content.lines();
  loop {
    let mount_point = match lines.next() {
      Some(l) if !l.is_empty() => l,
      _ => break,
    };
    let device = match lines.next() {
      Some(l) => l,
      None => break,
    };
    let fstype = match lines.next() {
      Some(l) => l,
      None => break,
    };
    let options = match lines.next() {
      Some(l) => l,
      None => break,
    };

    fs_infos.push(FsInfo {
      device:     device.to_string(),
      mountpoint: PathBuf::from(mount_point),
      fstype:     fstype.to_string(),
      options:    if options.is_empty() {
        Vec::new()
      } else {
        options.split(',').map(String::from).collect()
      },
    });
  }

  Ok(fs_infos)
}

fn needs_fsck(fstype: &str, check_journaling: bool) -> bool {
  match fstype {
    // ext2 has no journal - always check it.
    "ext2" => true,
    // Journaling filesystems where `fsck` works but is an expensive no-op
    // unless checkJournalingFS is on. Matches stage-1-init.sh:340-345:
    // userspace fsck is only invoked when the operator explicitly opts in.
    "ext3" | "ext4" | "reiserfs" | "xfs" | "jfs" | "f2fs" => check_journaling,
    // fat/ntfs have no journal; always check.
    "vfat" | "msdos" | "ntfs" => true,
    // Self-healing / kernel-side-checked filesystems that generic fsck must
    // never touch (line 309).
    "btrfs" | "zfs" | "bcachefs" => false,
    // Skipped explicitly upstream for various reasons (read-only, no fsck
    // tool, experimental). Listed so reviewers see parity with the shell
    // even though the default arm below would catch them.
    "iso9660" | "udf" | "apfs" | "nilfs2" | "squashfs" | "erofs" => false,
    // Anything we don't know about: don't invoke fsck. Matches the shell's
    // `auto`-fallthrough behaviour at line 324.
    _ => false,
  }
}

fn run_fsck(device: &str, fstype: &str, _options: &[String]) -> Result<bool> {
  // Device might be already mounted manually, e.g. NBD-device or the host
  // filesystem of the file which contains encrypted root fs.
  if is_device_mounted(device) {
    log_message(&format!("skip checking already mounted {device}"), true);
    return Ok(true);
  }

  log_message(&format!("Checking {fstype} filesystem on {device}"), true);

  // Skip non-block devices. Matches bash `[ ! -b "$device" ] && continue`:
  // anything that isn't a block device (pseudo-fs, missing path, etc.) is
  // skipped.
  match fs::metadata(device) {
    Ok(meta) if !meta.file_type().is_block_device() => return Ok(true),
    Err(_) => return Ok(true),
    Ok(_) => {},
  }

  let mut cmd = Command::new("fsck");
  cmd.arg("-a").arg("-t").arg(fstype).arg("-T").arg(device);

  // Add filesystem type specific options
  match fstype {
    "ext2" | "ext3" | "ext4" => {
      cmd.arg("-C0"); // Show progress on stdout
    },
    _ => {},
  }

  let status = cmd.status().context("Failed to run fsck")?;

  // fsck returns a bitmap; combined codes like 3 (1|2) and 6 (2|4) are
  // common. stage-1-init.sh:352-366 handles this with bitwise-OR tests:
  //   bit 1 (value 2) set  -> reboot immediately
  //   bit 2 (value 4) set  -> unrepaired errors, fail
  //   code >= 8            -> fsck itself failed, fail
  //   bit 0 only (0 or 1)  -> OK / errors corrected, continue
  let Some(code) = status.code() else {
    // Signal death etc.; matching "code >= 8" branch.
    bail!("fsck was terminated by a signal: {status}");
  };

  if code & 2 != 0 {
    log_message(&format!("fsck finished on {device}, rebooting..."), true);
    // Give kmsg a moment to flush, then request a reboot.
    std::thread::sleep(std::time::Duration::from_secs(3));
    let _ = Command::new("reboot").arg("-f").status();
    // If reboot(1) is missing or failed to take effect, fall through to a
    // panic so the caller spawns the recovery shell instead of silently
    // mounting a filesystem that asked for a reboot.
    bail!("fsck requested reboot but `reboot -f` did not halt the system");
  }

  if code & 4 != 0 {
    bail!(
      "{device} has unrepaired errors (fsck exit code {code}); fix manually",
    );
  }

  if code >= 8 {
    bail!("fsck on {device} failed with exit code {code}");
  }

  // code is 0 or 1 here: clean, or errors corrected and safe to mount.
  if code == 1 {
    log_message(&format!("fsck corrected errors on {device}"), true);
  }
  Ok(true)
}

fn mount_filesystem(fs_info: &FsInfo, dm: &DeviceManager) -> Result<()> {
  log_message(
    &format!(
      "Mounting {} ({}) at {:?}",
      fs_info.device, fs_info.fstype, fs_info.mountpoint
    ),
    true,
  );

  // Create mountpoint. File bind-mounts require a file target, not a directory.
  let is_file_bind = (fs_info.fstype == "bind"
    || fs_info.options.iter().any(|o| o == "bind" || o == "rbind"))
    && fs::metadata(&fs_info.device)
      .map(|m| !m.is_dir())
      .unwrap_or(false);

  if is_file_bind {
    if let Some(parent) = fs_info.mountpoint.parent() {
      fs::create_dir_all(parent).with_context(|| {
        format!("Failed to create parent of mountpoint: {:?}", parent)
      })?;
    }
    if !fs_info.mountpoint.exists() {
      fs::write(&fs_info.mountpoint, b"").with_context(|| {
        format!("Failed to create file mountpoint: {:?}", fs_info.mountpoint)
      })?;
    }
  } else {
    fs::create_dir_all(&fs_info.mountpoint).with_context(|| {
      format!("Failed to create mountpoint: {:?}", fs_info.mountpoint)
    })?;
  }

  let mount_request = Mount::new(
    &fs_info.device,
    &fs_info.mountpoint,
    Some(fs_info.fstype.as_str()),
    MountOptions::from_slice(&fs_info.options),
  );
  mount_request.apply_filesystem(dm)
}

/// Invoke `mount.bcachefs <device> <mountpoint> [-o opts]`, retrying until
/// `wait_timeout` on non-zero exits (where [`None`] = single attempt).
fn mount_bcachefs(
  device: &str,
  mountpoint: &Path,
  options: &[String],
  wait_timeout: Option<Duration>,
) -> Result<()> {
  fs::create_dir_all(mountpoint)
    .with_context(|| format!("Failed to create mountpoint: {mountpoint:?}"))?;

  log_message(
    &format!("Mounting bcachefs {device} at {mountpoint:?}"),
    true,
  );

  let filtered_opts = options
    .iter()
    .map(String::as_str)
    .filter(|o| !o.starts_with("x-") && !o.is_empty())
    .collect::<Vec<_>>();

  let run = || -> Result<(ExitStatus, String)> {
    let mut cmd = Command::new("mount.bcachefs");
    if !filtered_opts.is_empty() {
      cmd.arg("-o").arg(filtered_opts.join(","));
    }
    cmd.arg(device).arg(mountpoint);
    cmd.stdout(Stdio::piped());
    cmd.stderr(Stdio::piped());
    let out = cmd.output().context("Failed to spawn mount.bcachefs")?;
    let mut combined = String::from_utf8_lossy(&out.stderr).into_owned();
    let stdout = String::from_utf8_lossy(&out.stdout);
    if !stdout.is_empty() {
      if !combined.is_empty() {
        combined.push('\n');
      }
      combined.push_str(&stdout);
    }
    Ok((out.status, combined.trim().to_string()))
  };

  let log_output = |output: &str| {
    for line in output.lines() {
      log_message(&format!("mount.bcachefs: {line}"), true);
    }
  };

  let Some(timeout) = wait_timeout else {
    let (status, output) = run()?;
    if !status.success() {
      log_output(&output);
      bail!("mount.bcachefs {device} {mountpoint:?} exited with {status}");
    }
    return Ok(());
  };

  let start = Instant::now();
  loop {
    let (status, output) = run()?;
    if status.success() {
      return Ok(());
    }
    if start.elapsed() >= timeout {
      log_output(&output);
      bail!(
        "mount.bcachefs {device} {mountpoint:?} failed after {:?}: last exit \
         {}",
        timeout,
        status
      );
    }
    log_message(
      &format!(
        "mount.bcachefs {device} not ready (exit {status}), retrying..."
      ),
      true,
    );
    log_output(&output);
    thread::sleep(Duration::from_millis(500));
  }
}

fn mount_root(
  cmdline: &KernelCmdline,
  target_root: &Path,
  fs_infos: &[FsInfo],
  dm: &DeviceManager,
) -> Result<()> {
  log_message("Mounting root filesystem...", true);

  // fsInfo's "/" entry carries fstype/options, so read it even when `root=` is
  // set on the cmdline, so we can still detect bcachefs below.
  let fsinfo_root = fs_infos.iter().find(|f| f.mountpoint == Path::new("/"));

  let (root_device_owned, fsinfo_fstype): (String, Option<String>) =
    if let Some(r) = cmdline.root.as_ref() {
      (r.clone(), fsinfo_root.map(|e| e.fstype.clone()))
    } else {
      let entry = fsinfo_root.context(
        "No root= parameter specified on kernel command line and no '/' entry \
         in fsInfo",
      )?;
      (entry.device.clone(), Some(entry.fstype.clone()))
    };
  let root_device = &root_device_owned;

  // Handle special root devices
  if root_device == "tmpfs" {
    // Root on tmpfs (e.g., for live systems)
    fs::create_dir_all(target_root)?;
    mount(
      Some("tmpfs"),
      target_root,
      Some("tmpfs"),
      MsFlags::empty(),
      Some("mode=0755"),
    )
    .context("Failed to mount tmpfs root")?;
    return Ok(());
  }

  if root_device.starts_with("/dev/nfs") || root_device.starts_with("nfs:") {
    // NFS root
    fs::create_dir_all(target_root)?;
    let nfs_opts = cmdline
      .get("rootflags")
      .map_or("nolock", std::string::String::as_str);
    mount(
      Some(root_device.as_str()),
      target_root,
      Some("nfs"),
      MsFlags::empty(),
      Some(nfs_opts),
    )
    .context("Failed to mount NFS root")?;
    return Ok(());
  }

  if root_device.starts_with("//") {
    // CIFS root
    fs::create_dir_all(target_root)?;
    let cifs_opts = cmdline
      .get("rootflags")
      .map_or("", std::string::String::as_str);
    mount(
      Some(root_device.as_str()),
      target_root,
      Some("cifs"),
      MsFlags::empty(),
      Some(cifs_opts),
    )
    .context("Failed to mount CIFS root")?;
    return Ok(());
  }

  // In bcachefs, the filesystem UUID is fs-level, and not partition-level, so
  // `/dev/disk/by-uuid/<uuid>` may never appear and multi-device paths never
  // appear. Skip the udev-symlink wait and hand off to `mount.bcachefs`.
  let early_fstype = cmdline
    .get("rootfstype")
    .cloned()
    .or_else(|| fsinfo_fstype.clone());
  if early_fstype.as_deref() == Some("bcachefs") {
    let mut mount_opts: Vec<String> = cmdline
      .get("rootflags")
      .map(|s| s.split(',').map(String::from).collect())
      .unwrap_or_default();
    if let Some(e) = fsinfo_root {
      for opt in &e.options {
        if !mount_opts.contains(opt) {
          mount_opts.push(opt.clone());
        }
      }
    }
    if !mount_opts.iter().any(|o| o == "ro" || o == "rw") {
      mount_opts.push("rw".to_string());
    }
    dm.retrigger_block();
    mount_bcachefs(
      root_device,
      target_root,
      &mount_opts,
      Some(Duration::from_secs(30)),
    )?;
    log_message(&format!("Root filesystem mounted at {target_root:?}"), true);
    return Ok(());
  }

  // Resolve the device to a concrete path and wait for it to be ready.
  // by-label and by-uuid paths are udev-managed symlinks; resolve them
  // directly.
  let mount_device_owned: String = {
    let start = Instant::now();
    let timeout = Duration::from_secs(30);
    let mut last_retrigger = Instant::now();
    let mut resolved: Option<String> = None;

    while start.elapsed() < timeout {
      let candidate: Option<String> = if root_device
        .starts_with("/dev/disk/by-label/")
        || root_device.starts_with("/dev/disk/by-uuid/")
      {
        fs::canonicalize(root_device)
          .ok()
          .map(|p| p.to_string_lossy().into_owned())
      } else {
        Some(root_device.clone())
      };

      if let Some(dev) = candidate
        && device_is_ready(Path::new(&dev))
      {
        resolved = Some(dev);
        break;
      }

      // Periodically re-trigger block events and re-run `vgchange -ay` to
      // activate LVM volumes that showed up after initial device setup
      // (stacked LVM / late-arriving USB / slow PVs).
      if last_retrigger.elapsed() >= Duration::from_secs(3) {
        dm.retrigger_block();
        if Command::new("lvm")
          .arg("--version")
          .stdout(std::process::Stdio::null())
          .stderr(std::process::Stdio::null())
          .status()
          .is_ok_and(|s| s.success())
        {
          let _ = Command::new("lvm")
            .args(["vgchange", "-ay"])
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .status();
        }
        last_retrigger = Instant::now();
      }

      thread::sleep(Duration::from_millis(100));
    }

    resolved.ok_or_else(|| {
      anyhow::anyhow!("Timed out waiting for root device: {root_device}")
    })?
  };
  let mount_device = mount_device_owned.as_str();
  log_message(&format!("Using root device: {mount_device}"), true);

  let fstype = cmdline
    .get("rootfstype")
    .cloned()
    .or_else(|| udev_fs_type(mount_device))
    .or(fsinfo_fstype)
    .unwrap_or_else(|| "auto".to_string());

  // Build mount options: rootflags= from the cmdline (an operator override the
  // shell does not support), then merge options from the fsInfo record for /.
  // Exact-duplicate options are skipped; for same-key conflicts (e.g. two
  // subvol= entries) the kernel uses the last one, which is the fsInfo value.
  // The bcachefs path above applies the same merge.
  let mut mount_opts: Vec<String> = cmdline
    .get("rootflags")
    .map(|s| s.split(',').map(String::from).collect())
    .unwrap_or_default();

  if let Some(e) = fsinfo_root {
    for opt in &e.options {
      if !mount_opts.contains(opt) {
        mount_opts.push(opt.clone());
      }
    }
  }

  // Default to rw if neither ro nor rw was supplied by cmdline or fsInfo.
  if !mount_opts.iter().any(|o| o == "ro" || o == "rw") {
    mount_opts.push("rw".to_string());
  }

  // Check and run fsck if needed; propagate errors so the caller can invoke
  // the recovery shell rather than mounting a corrupt root filesystem.
  if needs_fsck(&fstype, true) {
    run_fsck(mount_device, &fstype, &mount_opts)?;
  }

  // Mount the root filesystem
  fs::create_dir_all(target_root)?;

  Mount::new(
    mount_device,
    target_root,
    Some(fstype.as_str()),
    MountOptions::from_vec(mount_opts),
  )
  .apply()
  .with_context(|| {
    format!("Failed to mount root filesystem {mount_device} at {target_root:?}")
  })?;

  log_message(&format!("Root filesystem mounted at {target_root:?}"), true);

  Ok(())
}

fn mount_additional_filesystems(
  fs_infos: &[FsInfo],
  target_root: &Path,
  dm: &DeviceManager,
) -> Result<()> {
  for fs_info in fs_infos {
    if fs_info.mountpoint == Path::new("/") {
      continue; // Skip root, already mounted
    }

    // Adjust mountpoint to be under target_root
    let adjusted_mountpoint = if fs_info.mountpoint.is_absolute() {
      target_root.join(
        fs_info
          .mountpoint
          .strip_prefix("/")
          .unwrap_or(&fs_info.mountpoint),
      )
    } else {
      target_root.join(&fs_info.mountpoint)
    };

    let mut adjusted_fs_info = fs_info.clone();
    adjusted_fs_info.mountpoint = adjusted_mountpoint;

    // For overlay mounts, rewrite lowerdir/upperdir/workdir to be under
    // target_root.
    if fs_info.fstype == "overlay" {
      let target_root_str = target_root.to_string_lossy();
      adjusted_fs_info.options = fs_info
        .options
        .iter()
        .map(|opt| {
          for prefix in &["lowerdir=", "upperdir=", "workdir="] {
            if let Some(rest) = opt.strip_prefix(prefix) {
              // Rewrite each colon-separated path component.
              let adjusted = rest
                .split(':')
                .map(|p| {
                  if p.starts_with('/') {
                    format!("{target_root_str}{p}")
                  } else {
                    p.to_string()
                  }
                })
                .collect::<Vec<_>>()
                .join(":");
              return format!("{prefix}{adjusted}");
            }
          }
          opt.clone()
        })
        .collect();
    }

    if let Err(e) = mount_filesystem(&adjusted_fs_info, dm) {
      log_message(
        &format!(
          "Warning: failed to mount {:?}: {:#}",
          adjusted_fs_info.mountpoint, e
        ),
        true,
      );
    }
  }

  Ok(())
}

fn copy_iso_to_ram(cmdline: &KernelCmdline, target_root: &Path) -> Result<()> {
  if !cmdline.copy_to_ram {
    return Ok(());
  }

  log_message("Copying ISO to RAM...", true);

  let iso_source = cmdline
    .get("iso_source")
    .map_or("/run/iso", std::string::String::as_str);

  let iso_dest = target_root.join("iso");
  fs::create_dir_all(&iso_dest)?;

  match copy_dir_recursive(&PathBuf::from(iso_source), &iso_dest) {
    Ok(()) => log_message("ISO copied to RAM", true),
    Err(e) => {
      log_message(&format!("Warning: Failed to copy ISO to RAM: {e}"), true);
    },
  }

  Ok(())
}

fn handle_persistence(
  cmdline: &KernelCmdline,
  target_root: &Path,
  dm: &DeviceManager,
) -> Result<()> {
  let persist_opt = match &cmdline.persistence {
    Some(p) => p.clone(),
    None => return Ok(()),
  };

  log_message(&format!("Setting up persistence: {persist_opt}"), true);

  let (device, path) = if persist_opt.contains(':') {
    let mut parts = persist_opt.splitn(2, ':');
    let dev = parts.next().unwrap().to_string();
    let p = parts.next().unwrap().to_string();
    (Some(dev), p)
  } else {
    (None, persist_opt)
  };

  if let Some(dev) = device {
    wait_for_device(&dev, 10, dm).ok();

    let persist_mount = Path::new("/run/persistence");
    fs::create_dir_all(persist_mount)?;

    mount(
      Some(dev.as_str()),
      persist_mount,
      Some("auto"),
      MsFlags::empty(),
      None::<&str>,
    )
    .context("Failed to mount persistence device")?;

    let persist_source =
      persist_mount.join(path.strip_prefix('/').unwrap_or(&path));
    if persist_source.exists() {
      log_message(
        &format!("Bind-mounting persistence from {persist_source:?}"),
        true,
      );
      mount(
        Some(&persist_source),
        target_root,
        None::<&str>,
        MsFlags::MS_BIND,
        None::<&str>,
      )
      .context("Failed to bind-mount persistence")?;
    } else {
      log_message(
        &format!("Warning: persistence path {persist_source:?} not found"),
        true,
      );
    }
  }

  Ok(())
}

fn move_path(src: &Path, dst: &Path) -> Result<()> {
  if fs::rename(src, dst).is_ok() {
    return Ok(());
  }
  // rename failed (likely EXDEV across filesystems); fall back to copy + delete
  if src.is_dir() {
    copy_dir_recursive(src, dst)?;
    fs::remove_dir_all(src)
      .with_context(|| format!("Failed to remove {src:?} after copy"))?;
  } else {
    fs::copy(src, dst)
      .with_context(|| format!("Failed to copy {src:?} to {dst:?}"))?;
    fs::remove_file(src)
      .with_context(|| format!("Failed to remove {src:?} after copy"))?;
  }
  Ok(())
}

// Handle NIXOS_LUSTRATE: move old root aside and restore selected entries.
fn handle_lustrate(target_root: &Path) -> Result<()> {
  let lustrate_file = target_root.join("nixos-lustrate");

  if !lustrate_file.exists() {
    return Ok(());
  }

  log_message("Handling NIXOS_LUSTRATE...", true);

  let content = fs::read_to_string(&lustrate_file)?;

  // Stage moves into old-root.tmp first; rename atomically when complete so
  // a crash mid-way leaves old-root.tmp (not old-root), and a re-run retries.
  let backup_tmp = target_root.join("old-root.tmp");
  let backup_dir = target_root.join("old-root");
  fs::create_dir_all(&backup_tmp)?;

  for entry in fs::read_dir(target_root)? {
    let entry = entry?;
    let name = entry.file_name();
    let name_str = name.to_string_lossy();

    if name_str.starts_with("nix")
      || name_str.starts_with("boot")
      || name_str == "old-root"
      || name_str == "old-root.tmp"
    {
      continue;
    }

    let dest = backup_tmp.join(&name);
    if let Err(e) = move_path(&entry.path(), &dest) {
      log_message(
        &format!("Warning: move failed for {:?}: {}", entry.path(), e),
        true,
      );
    }
  }

  fs::rename(&backup_tmp, &backup_dir).with_context(|| {
    format!("Failed to rename {:?} to {:?}", backup_tmp, backup_dir)
  })?;

  // Restore entries listed in the lustrate file (mirrors original bash read
  // loop)
  for keeper in content.lines() {
    let keeper = keeper.trim();
    if keeper.is_empty() || keeper.starts_with('#') {
      continue;
    }
    let stripped = keeper.strip_prefix('/').unwrap_or(keeper);
    let src = backup_dir.join(stripped);
    let dst = target_root.join(stripped);
    if src.exists() {
      if let Some(parent) = dst.parent() {
        fs::create_dir_all(parent)?;
      }
      if let Err(e) = copy_dir_recursive(&src, &dst) {
        log_message(&format!("Warning: failed to restore {src:?}: {e}"), true);
      }
    }
  }

  fs::remove_file(&lustrate_file)?;
  log_message("Lustrate complete", true);

  Ok(())
}

// Recursively copy src's contents into dest.
fn copy_dir_recursive(src: &Path, dest: &Path) -> Result<()> {
  fs::create_dir_all(dest)
    .with_context(|| format!("Failed to create directory {dest:?}"))?;

  // Restrict directory permissions: secret directories must not be
  // world-readable.
  fs::set_permissions(dest, Permissions::from_mode(0o700))
    .with_context(|| format!("Failed to set permissions on {dest:?}"))?;

  for entry in fs::read_dir(src)
    .with_context(|| format!("Failed to read directory {src:?}"))?
  {
    let entry = entry?;
    let src_path = entry.path();
    let dest_path = dest.join(entry.file_name());

    let file_type = entry.file_type()?;
    if file_type.is_dir() {
      copy_dir_recursive(&src_path, &dest_path)?;
    } else {
      // Includes regular files and symlinks (copy resolves symlinks).
      fs::copy(&src_path, &dest_path).with_context(|| {
        format!("Failed to copy {src_path:?} to {dest_path:?}")
      })?;
    }
  }
  Ok(())
}

fn link_extra_utils_secrets(extra_utils: Option<&Path>) -> Result<()> {
  let Some(utils) = extra_utils else {
    return Ok(());
  };
  let extra_secrets = utils.join("secrets");
  if !extra_secrets.is_dir() {
    return Ok(());
  }
  log_message("Linking extraUtils secrets...", true);
  for entry in fs::read_dir(&extra_secrets)? {
    let entry = entry?;
    let source = entry.path();
    let rel = source.strip_prefix(&extra_secrets)?;
    let dest = Path::new("/").join(rel);
    if let Some(parent) = dest.parent() {
      fs::create_dir_all(parent)?;
    }

    // Symlink into the initrd namespace so tools running during stage 1
    // (cryptsetup, network helpers, etc.) find them at the
    // expected absolute paths.
    if source.is_dir() {
      symlink_dir_recurse(&source, &dest)?;
    } else {
      symlink(&source, &dest)
        .with_context(|| format!("Failed to symlink {source:?} to {dest:?}"))?;
    }
  }
  Ok(())
}

// Must run after mount_essential_filesystems so /run is a real tmpfs;
// /.initrd-secrets entries targeting /run/* would be hidden if copied before
// the tmpfs mount.
fn copy_initrd_secrets() -> Result<()> {
  let initrd_secrets = Path::new("/.initrd-secrets");
  if !initrd_secrets.is_dir() {
    return Ok(());
  }
  log_message("Copying /.initrd-secrets...", true);
  for entry in fs::read_dir(initrd_secrets)? {
    let entry = entry?;
    let source = entry.path();
    let rel = source.strip_prefix(initrd_secrets)?;
    let dest = Path::new("/").join(rel);
    if let Some(parent) = dest.parent() {
      fs::create_dir_all(parent)?;
    }

    let meta = entry.metadata()?;
    if meta.is_dir() {
      copy_dir_recursive(&source, &dest)?;
    } else {
      fs::copy(&source, &dest)
        .with_context(|| format!("Failed to copy {source:?} to {dest:?}"))?;
      // Secret files in the initrd must not be world-readable.
      fs::set_permissions(&dest, Permissions::from_mode(0o600))?;
    }
  }
  Ok(())
}

/// Symlink an entire directory tree, mirroring the structure of `src` under
/// `dest`.  Directories become real directories (not symlinks) so their
/// contents are reachable through the filesystem.
fn symlink_dir_recurse(src: &Path, dest: &Path) -> Result<()> {
  fs::create_dir_all(dest)?;
  for entry in fs::read_dir(src)? {
    let entry = entry?;
    let child_src = entry.path();
    let child_dest = dest.join(entry.file_name());
    if child_src.is_dir() {
      symlink_dir_recurse(&child_src, &child_dest)?;
    } else {
      symlink(&child_src, &child_dest).with_context(|| {
        format!("Failed to symlink {child_src:?} to {child_dest:?}")
      })?;
    }
  }
  Ok(())
}

/// Emit a udev rule mapping the real root device to /dev/root so systemd's
/// mount-unit generator can find it. stage-1-init.sh does the equivalent via
/// `udevadm info --device-id-of-file`.
fn write_dev_root_udev_rule(target_root: &Path) -> Result<()> {
  // Prefer the iso file if this is a livecd boot, as the shell does; fall back
  // to stat'ing target_root itself so bind-mounted / overlay roots still work.
  let iso = target_root.join("iso");
  let stat_target = if iso.exists() {
    iso
  } else {
    target_root.to_path_buf()
  };

  let meta = match fs::metadata(&stat_target) {
    Ok(m) => m,
    Err(e) => {
      log_message(
        &format!(
          "Skipping /dev/root udev rule; stat({}) failed: {e}",
          stat_target.display()
        ),
        true,
      );
      return Ok(());
    },
  };

  let dev = meta.dev();
  let (major, minor) = (libc::major(dev), libc::minor(dev));

  // Shell: `if [ "$ROOT_MAJOR" -a "$ROOT_MINOR" -a "$ROOT_MAJOR" != 0 ]`.
  if major == 0 {
    log_message(
      "Skipping /dev/root udev rule; root is not on a block device (pseudo \
       fs?)",
      true,
    );
    return Ok(());
  }

  let rules_dir = Path::new("/run/udev/rules.d");
  fs::create_dir_all(rules_dir)
    .with_context(|| format!("Failed to create {}", rules_dir.display()))?;
  let rule = format!(
    "ACTION==\"add|change\", SUBSYSTEM==\"block\", ENV{{MAJOR}}==\"{major}\", \
     ENV{{MINOR}}==\"{minor}\", SYMLINK+=\"root\"\n"
  );
  let path = rules_dir.join("61-dev-root-link.rules");
  fs::write(&path, rule)
    .with_context(|| format!("Failed to write {}", path.display()))?;
  log_message(
    &format!(
      "Wrote /dev/root udev rule ({major}:{minor}) to {}",
      path.display()
    ),
    true,
  );
  Ok(())
}

fn kill_remaining_processes() -> Result<()> {
  log_message("Killing remaining processes...", true);

  // Signal all processes except ourselves and storage daemons to terminate
  // Storage daemons are distinguished by an @ in front of their command line:
  // See:
  //  <https://www.freedesktop.org/wiki/Software/systemd/RootStorageDaemons>
  let my_pid = getpid().as_raw();

  // First try SIGTERM
  for entry in fs::read_dir("/proc")? {
    let entry = entry?;
    if let Ok(pid) = entry.file_name().to_string_lossy().parse::<i32>()
      && pid > 1
      && pid != my_pid
      && !is_storage_daemon(pid)
    {
      unsafe {
        libc::kill(pid, libc::SIGTERM);
      }
    }
  }

  // Wait a bit
  thread::sleep(Duration::from_millis(500));

  // Then SIGKILL remaining processes (still excluding storage daemons)
  for entry in fs::read_dir("/proc")? {
    let entry = entry?;
    if let Ok(pid) = entry.file_name().to_string_lossy().parse::<i32>()
      && pid > 1
      && pid != my_pid
      && !is_storage_daemon(pid)
    {
      unsafe {
        libc::kill(pid, libc::SIGKILL);
      }
    }
  }

  Ok(())
}

fn is_storage_daemon(pid: i32) -> bool {
  let cmdline_path = format!("/proc/{pid}/cmdline");
  if let Ok(content) = fs::read_to_string(&cmdline_path) {
    // cmdline is null-separated; check if first argument starts with @
    if let Some(first_arg) = content.split('\0').next() {
      return first_arg.starts_with('@');
    }
  }
  false
}

fn start_recovery_shell(reason: &str, pre_fail_commands: Option<&Path>) -> ! {
  eprintln!("\n");
  eprintln!("========================================");
  eprintln!("Boot failed: {reason}");
  eprintln!("Starting recovery shell...");
  eprintln!("========================================");
  eprintln!("\n");

  // Run pre-fail commands if available
  if let Some(commands) = pre_fail_commands
    && commands.exists()
  {
    let _ = Command::new(commands).status();
  }

  // Try to spawn a shell
  let shell = env::var("SHELL").unwrap_or_else(|_| "/bin/sh".to_string());

  let _ = Command::new(&shell).env("PS1", "(initrd) $ ").status();

  // If shell exits or fails, halt
  eprintln!("Shell exited. Halting...");
  loop {
    unsafe {
      libc::sync();
      libc::reboot(libc::RB_HALT_SYSTEM);
    }
  }
}

fn fail(reason: &str, cmdline: &KernelCmdline, config: &Stage1Config) -> ! {
  log_message(&format!("FAIL: {reason}"), true);

  if cmdline.shell_on_fail || cmdline.debug1 {
    start_recovery_shell(reason, config.pre_fail_commands.as_deref());
  } else if cmdline.panic_on_fail {
    // Trigger kernel panic
    let _ = fs::write("/proc/sysrq-trigger", "c");
    std::process::exit(1);
  } else {
    // Reboot
    eprintln!("Boot failed. Rebooting in 10 seconds...");
    thread::sleep(Duration::from_secs(10));
    unsafe {
      libc::reboot(libc::RB_AUTOBOOT);
    }
    std::process::exit(1);
  }
}

fn switch_root(
  target_root: &Path,
  init: &str,
  cmdline: &KernelCmdline,
) -> Result<()> {
  log_message(&format!("Switching root to {target_root:?}"), true);

  // Check that init exists
  let init_path = target_root.join(init.trim_start_matches('/'));
  if !init_path.exists() {
    bail!("Init program not found: {init}");
  }

  // Move essential mounts into the new root. The early mount script may have
  // already mounted these at target_root/{dev,proc,sys,run}; MS_MOVE on an
  // already-occupied destination returns EBUSY, which we swallow.
  let essential_mounts = ["/dev", "/proc", "/sys", "/run"];
  for mountpoint in &essential_mounts {
    let old_path = Path::new(mountpoint);
    let new_path = target_root.join(mountpoint.trim_start_matches('/'));
    fs::create_dir_all(&new_path).ok();
    mount(
      Some(old_path),
      &new_path,
      None::<&str>,
      MsFlags::MS_MOVE,
      None::<&str>,
    )
    .ok();
  }

  // Change to the new root
  chdir(target_root)
    .with_context(|| format!("Failed to chdir to {target_root:?}"))?;

  // The initrd root is a ramfs; pivot_root(2) does not work on ramfs.
  // Move the new root filesystem onto / with MS_MOVE, then chroot into it.
  mount(
    Some("."),
    Path::new("/"),
    None::<&str>,
    MsFlags::MS_MOVE,
    None::<&str>,
  )
  .context("Failed to move new root to /")?;

  chroot(Path::new(".")).context("Failed to chroot into new root")?;

  chdir("/").context("Failed to chdir to new /")?;

  // Set up console
  setup_console(cmdline)?;

  for fd in 3..1024 {
    unsafe {
      libc::close(fd);
    }
  }

  log_message(&format!("Executing init: {init}"), true);

  // Upstream stage-1-init.sh exec's switch_root via `env -i` to wipe the
  // environment before handing off to /init: the LD_LIBRARY_PATH we set to
  // @extraUtils@/lib for initrd tools (cryptsetup, lvm, etc.) points at a
  // stripped-down libc without libbpf/libseccomp, and letting it leak into
  // PID 1 breaks systemd's dlopen of those features, which in turn,
  // disables seccomp sandboxing and the service-spawn PATH logic, so every
  // unit with a relative ExecStart (systemd-tmpfiles, journalctl, bootctl,
  // modprobe, udevadm) fails at boot with status=203/EXEC.
  //
  // Clear the whole environment to mirror `env -i`. /init is responsible
  // for re-exporting its own HOME/PATH.
  //
  // SAFETY: single-threaded at this point; no other threads can observe
  // the environment change.
  unsafe {
    for (key, _) in std::env::vars_os().collect::<Vec<_>>() {
      std::env::remove_var(&key);
    }
  }

  let argv = [CString::new(init).context("Invalid init path")?];

  execv(&argv[0], &argv)
    .with_context(|| format!("Failed to exec init: {init}"))?;

  bail!("execv returned unexpectedly")
}

fn setup_console(cmdline: &KernelCmdline) -> Result<()> {
  unsafe {
    libc::close(0);
    libc::close(1);
    libc::close(2);
  }

  // Build a /dev/<device> path from the first console= entry. Strip any
  // baud/mode suffix (e.g. "ttyS0,115200n8" -> "ttyS0").
  let console_path: String = cmdline.console.first().map_or_else(
    || "/dev/console".to_string(),
    |s| {
      let dev = s.split(',').next().unwrap_or(s);
      if dev.starts_with('/') {
        dev.to_string()
      } else {
        format!("/dev/{dev}")
      }
    },
  );

  // SAFETY: CString ensures null termination required by libc::open.
  let c_console = CString::new(console_path.as_str())
    .context("console path contains a null byte")?;
  let mut fd =
    unsafe { libc::open(c_console.as_ptr(), libc::O_RDWR | libc::O_NOCTTY) };

  if fd < 0 {
    fd = unsafe {
      libc::open(c"/dev/console".as_ptr(), libc::O_RDWR | libc::O_NOCTTY)
    };
    if fd < 0 {
      bail!("Failed to open console");
    }
  }

  unsafe {
    libc::dup2(fd, 0);
    libc::dup2(fd, 1);
    libc::dup2(fd, 2);
    if fd > 2 {
      libc::close(fd);
    }
  }

  Ok(())
}

fn run_hook_script(script: Option<&Path>, description: &str) -> Result<()> {
  if let Some(script) = script
    && script.exists()
    && fs::metadata(script).map(|m| m.len() > 0).unwrap_or(false)
  {
    log_message(&format!("Running {description}: {script:?}"), true);

    // Run via sh since hook files are plain text without a shebang.
    let status = Command::new("sh")
      .arg(script)
      .status()
      .with_context(|| format!("Failed to run {description}"))?;

    if !status.success() {
      log_message(
        &format!(
          "Warning: {} exited with status: {:?}",
          description,
          status.code()
        ),
        true,
      );
    }
  }
  Ok(())
}

fn set_host_id(hex_id: Option<&str>) -> Result<()> {
  let Some(hex) = hex_id else {
    return Ok(());
  };
  let hex = hex.trim();
  if hex.len() != 8 {
    bail!("HOST_ID must be an 8-character hex string, got: '{hex}'");
  }
  let n = u32::from_str_radix(hex, 16)
    .with_context(|| format!("Invalid HOST_ID hex string: '{hex}'"))?;
  let bytes = n.to_ne_bytes();
  log_message(&format!("Setting host ID: {hex}"), true);
  fs::write("/etc/hostid", bytes).context("Failed to write /etc/hostid")?;
  Ok(())
}

fn parse_args(args: &[String]) -> Stage1Config {
  let mut config = Stage1Config::from_env();

  // Parse CLI args (override env vars)
  let mut i = 1;
  while i < args.len() {
    match args[i].as_str() {
      "--target-root" | "-t" if i + 1 < args.len() => {
        config.target_root = PathBuf::from(&args[i + 1]);
        i += 1;
      },
      "--extra-utils" if i + 1 < args.len() => {
        config.extra_utils = Some(PathBuf::from(&args[i + 1]));
        i += 1;
      },
      "--distro-name" if i + 1 < args.len() => {
        config.distro_name = args[i + 1].clone();
        i += 1;
      },
      _ => {},
    }
    i += 1;
  }

  config
}

fn parse_shell_args(line: &str) -> Vec<String> {
  let mut args = Vec::new();
  let mut current = String::new();
  let mut chars = line.chars().peekable();

  while let Some(c) = chars.next() {
    match c {
      '\'' => {
        // Single-quoted string: take everything until closing '
        for c2 in chars.by_ref() {
          if c2 == '\'' {
            break;
          }
          current.push(c2);
        }
      },
      '"' => {
        for c2 in chars.by_ref() {
          if c2 == '"' {
            break;
          }
          current.push(c2);
        }
      },
      ' ' | '\t' => {
        if !current.is_empty() {
          args.push(std::mem::take(&mut current));
        }
      },
      _ => current.push(c),
    }
  }
  if !current.is_empty() {
    args.push(current);
  }
  args
}

// Parse mount options into MsFlags bits and a leftover data string.
// Options that map to kernel flags are consumed; the rest are rejoined for the
// data parameter. Accepts any iterator of option strings (e.g. a comma-split
// &str or a pre-split Vec<String> slice).
fn parse_mount_options<'a>(
  opts: impl Iterator<Item = &'a str>,
) -> (MsFlags, Option<String>) {
  let mut flags = MsFlags::empty();
  let mut data: Vec<&'a str> = Vec::new();

  for opt in opts {
    match opt {
      "ro" => flags |= MsFlags::MS_RDONLY,
      // userspace-only options are silently handled by mount(8), but must be
      // stripped before mount(2)
      "defaults" | "auto" | "noauto" | "user" | "nouser" | "_netdev"
      | "nofail" | "rw" | "exec" | "async" | "" => {},
      "nosuid" => flags |= MsFlags::MS_NOSUID,
      "nodev" => flags |= MsFlags::MS_NODEV,
      "noexec" => flags |= MsFlags::MS_NOEXEC,
      "sync" => flags |= MsFlags::MS_SYNCHRONOUS,
      "noatime" => flags |= MsFlags::MS_NOATIME,
      "nodiratime" => flags |= MsFlags::MS_NODIRATIME,
      "relatime" => flags |= MsFlags::MS_RELATIME,
      "strictatime" => flags |= MsFlags::MS_STRICTATIME,
      "lazytime" => flags |= MsFlags::MS_LAZYTIME,
      "bind" => flags |= MsFlags::MS_BIND,
      "rbind" => flags |= MsFlags::MS_BIND | MsFlags::MS_REC,
      "remount" => flags |= MsFlags::MS_REMOUNT,
      "silent" => flags |= MsFlags::MS_SILENT,
      "dirsync" => flags |= MsFlags::MS_DIRSYNC,
      o if o.starts_with("x-") => {},
      _ => data.push(opt),
    }
  }

  let data_str = if data.is_empty() {
    None
  } else {
    Some(data.join(","))
  };
  (flags, data_str)
}

fn special_mount_target(target_root: &Path, mountpoint: &str) -> PathBuf {
  let absolute = Path::new(mountpoint);

  // These roots are MS_MOVE'd into the final root, so child mounts have to live
  // under the initrd mount tree or they will be hidden after switch_root.
  for moved_root in ["/dev", "/proc", "/sys", "/run"].map(Path::new) {
    if absolute.starts_with(moved_root) {
      return absolute.to_path_buf();
    }
  }

  target_root.join(mountpoint.strip_prefix('/').unwrap_or(mountpoint))
}

/// Main entry point for stage 1 initialization
pub fn run(args: &[String]) -> Result<()> {
  // Mount /proc early so KernelCmdline::parse() can read /proc/cmdline.
  // The rest of the essential mounts happen later in
  // mount_essential_filesystems().
  {
    let proc_path = Path::new("/proc");
    let _ = fs::create_dir_all(proc_path);
    if !is_mounted(proc_path) {
      let _ = mount(
        Some("proc"),
        proc_path,
        Some("proc"),
        MsFlags::MS_NOSUID | MsFlags::MS_NOEXEC | MsFlags::MS_NODEV,
        None::<&str>,
      );
    }
  }

  let config = parse_args(args);
  let cmdline =
    KernelCmdline::parse().context("Failed to parse kernel command line")?;

  setup_console(&cmdline).ok();

  let greeting = format!("<<< {} Stage 1 >>>", config.distro_name);
  println!("{greeting}");
  log_message(&greeting, true);

  setup_environment(config.extra_utils.as_deref())
    .context("Failed to set up environment")?;
  link_extra_utils_secrets(config.extra_utils.as_deref())
    .context("Failed to link extraUtils secrets")?;
  create_directories().context("Failed to create directories")?;
  create_essential_devices().context("Failed to create essential devices")?;
  mount_essential_filesystems()
    .context("Failed to mount essential filesystems")?;
  copy_initrd_secrets().context("Failed to copy initrd secrets")?;
  create_essential_files().context("Failed to create essential files")?;

  set_host_id(config.set_host_id.as_deref())
    .context("Failed to set host ID")?;

  run_hook_script(config.pre_device_commands.as_deref(), "pre-device commands")
    .context("Pre-device commands failed")?;

  load_kernel_modules(&config.kernel_modules, cmdline.no_modprobe)
    .context("Failed to load kernel modules")?;

  if let Some(link_units) = config.link_units.as_deref() {
    setup_link_units(link_units, &config.link_units_dest)
      .context("Failed to set up systemd link units")?;
  }

  config
    .device_manager
    .start()
    .context("Failed to start device manager")?;
  config
    .device_manager
    .trigger()
    .context("Failed to trigger device events")?;
  config
    .device_manager
    .settle()
    .context("Failed to settle device manager")?;

  // LUKS-on-LVM and similar stacked setups need a hook here to cryptsetup-open
  // devices before vgchange can scan them. stage-1-init.sh:286 injects
  // `@preLVMCommands@` at exactly this point.
  run_hook_script(config.pre_lvm_commands.as_deref(), "pre-LVM commands")
    .context("Pre-LVM commands failed")?;

  activate_lvm().context("Failed to activate LVM")?;

  // Operator-triggered checkpoint: drop into the recovery shell after devices
  // are assembled but before anything is mounted. This is consistent with the
  // upstream stage1 script.
  if cmdline.debug1devices {
    fail("boot.debug1devices checkpoint reached", &cmdline, &config);
  }

  run_hook_script(
    config.post_device_commands.as_deref(),
    "post-device commands",
  )
  .context("Post-device commands failed")?;

  handle_resume(
    cmdline
      .resume
      .as_deref()
      .or(config.resume_device.as_deref()),
    &config.resume_devices,
  )
  .context("Failed to handle resume")?;

  run_hook_script(
    config.post_resume_commands.as_deref(),
    "post-resume commands",
  )
  .context("Post-resume commands failed")?;

  // Parse filesystem info early so mount_root can fall back to it when root= is
  // absent.
  let fs_infos: Vec<FsInfo> = if let Some(fs_info_path) = &config.fs_info {
    parse_fs_info(fs_info_path).context("Failed to parse filesystem info")?
  } else {
    Vec::new()
  };

  if let Err(e) = mount_root(
    &cmdline,
    &config.target_root,
    &fs_infos,
    &config.device_manager,
  ) {
    fail(
      &format!("Failed to mount root filesystem: {e}"),
      &cmdline,
      &config,
    );
  }

  // XXX: Must run before mount_additional_filesystems.
  handle_lustrate(&config.target_root).context("Failed to handle lustrate")?;

  for fs_info in &fs_infos {
    if fs_info.mountpoint == Path::new("/") {
      continue;
    }
    if needs_fsck(&fs_info.fstype, config.check_journaling_fs)
      && let Err(e) =
        run_fsck(&fs_info.device, &fs_info.fstype, &fs_info.options)
    {
      log_message(
        &format!("Warning: fsck failed on {}: {e:#}", fs_info.device),
        true,
      );
    }
  }

  if !fs_infos.is_empty() {
    mount_additional_filesystems(
      &fs_infos,
      &config.target_root,
      &config.device_manager,
    )
    .context("Failed to mount additional filesystems")?;
  }

  if let Some(script) = &config.early_mount_script
    && script.exists()
  {
    log_message(&format!("Running early mount script: {script:?}"), true);

    let script_content = fs::read_to_string(script)
      .context("Failed to read early mount script")?;

    for line in script_content.lines() {
      let line = line.trim();
      if line.is_empty() || line.starts_with('#') {
        continue;
      }
      let Some(rest) = line.strip_prefix("specialMount ") else {
        continue;
      };
      let args = parse_shell_args(rest);
      if args.len() < 4 {
        log_message(
          &format!("Warning: malformed specialMount line: {line}"),
          true,
        );
        continue;
      }
      let device = &args[0];
      let mountpoint = &args[1];
      let options = &args[2];
      let fstype = &args[3];

      let target = special_mount_target(&config.target_root, mountpoint);
      if is_mounted(&target) {
        log_message(
          &format!(
            "Skipping specialMount {mountpoint}: {target:?} is already mounted"
          ),
          true,
        );
        continue;
      }
      fs::create_dir_all(&target)?;

      let mount_result = Mount::new(
        device,
        &target,
        Some(fstype.as_str()),
        MountOptions::from_csv(options),
      )
      .apply();
      if let Err(e) = mount_result {
        fail(
          &format!(
            "Early mount script: failed to mount {device} at {target:?}: {e}"
          ),
          &cmdline,
          &config,
        );
      }
    }
  }

  run_hook_script(config.post_mount_commands.as_deref(), "post-mount commands")
    .context("Post-mount commands failed")?;

  config
    .device_manager
    .write_dev_root_rule(&config.target_root)
    .context("Failed to emit /dev/root udev rule")?;

  copy_iso_to_ram(&cmdline, &config.target_root)
    .context("Failed to copy ISO to RAM")?;
  handle_persistence(&cmdline, &config.target_root, &config.device_manager)
    .context("Failed to handle persistence")?;

  config.device_manager.stop();

  kill_remaining_processes().context("Failed to kill remaining processes")?;

  // Post-kill-processes checkpoint. Deliberately after
  // udevd is gone so the recovery shell is the only thing still running.
  if cmdline.debug1mounts {
    fail("boot.debug1mounts checkpoint reached", &cmdline, &config);
  }

  let init = cmdline
    .init
    .as_deref()
    .unwrap_or("/nix/var/nix/profiles/system/sw/bin/init");

  if let Err(e) = switch_root(&config.target_root, init, &cmdline) {
    fail(&format!("Failed to switch root: {e}"), &cmdline, &config);
  }

  bail!("switch_root returned unexpectedly")
}
