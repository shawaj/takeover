use std::env::{current_exe, set_current_dir};
use std::fs::{copy, create_dir, create_dir_all, read_link, remove_dir_all, OpenOptions};
use std::os::unix::fs::symlink;
use std::path::{Path, PathBuf};
use std::thread::sleep;
use std::time::Duration;

use nix::unistd::sync;

use failure::ResultExt;
use log::{debug, error, info};

pub(crate) mod migrate_info;
use migrate_info::MigrateInfo;

pub(crate) mod assets;
use assets::Assets;

mod api_calls;
mod block_device_info;
mod defs;
mod device;
mod device_impl;
mod image_retrieval;
mod utils;
mod wifi_config;

use crate::common::{
    call,
    defs::{
        BALENA_CONFIG_PATH, BALENA_IMAGE_NAME, CP_CMD, MOUNT_CMD, OLD_ROOT_MP, STAGE2_CONFIG_NAME,
        SWAPOFF_CMD, SYSTEM_CONNECTIONS_DIR, TELINIT_CMD, TRANSFER_DIR,
    },
    dir_exists, file_exists, format_size_with_unit, get_mem_info, is_admin,
    mig_error::{MigErrCtx, MigError, MigErrorKind},
    options::Options,
    path_append,
    stage2_config::Stage2Config,
};

use block_device_info::BlockDeviceInfo;
use mod_logger::Logger;
use utils::{mktemp, mount_fs};

use crate::common::stage2_config::UmountPart;
use std::io::Write;

const XTRA_FS_SIZE: u64 = 10 * 1024 * 1024; // const XTRA_MEM_FREE: u64 = 10 * 1024 * 1024; // 10 MB
const DO_CLEANUP: bool = true;

fn get_required_space(opts: &Options, mig_info: &MigrateInfo) -> Result<u64, MigError> {
    let mut req_size: u64 = mig_info.get_assets().busybox_size() as u64 + XTRA_FS_SIZE;

    req_size += if let Some(image_path) = opts.get_image() {
        if image_path.exists() {
            image_path
                .metadata()
                .context(upstream_context!(&format!(
                    "Failed to retrieve imagesize for '{}'",
                    image_path.display()
                )))?
                .len() as u64
        } else {
            error!("Image could not be found: '{}'", image_path.display());
            return Err(MigError::displayed());
        }
    } else {
        error!("Required parameter image is missing.");
        return Err(MigError::displayed());
    };

    req_size += if let Some(config_path) = opts.get_config().clone() {
        if file_exists(&config_path) {
            config_path
                .metadata()
                .context(upstream_context!(&format!(
                    "Failed to retrieve file size for '{}'",
                    config_path.display()
                )))?
                .len() as u64
        } else {
            error!("Config could not be found: '{}'", config_path.display());
            return Err(MigError::displayed());
        }
    } else {
        error!("The required parameter --config/-c was not provided");
        return Err(MigError::displayed());
    };

    for nwmgr_cfg in opts.get_nwmgr_cfg() {
        req_size += nwmgr_cfg
            .metadata()
            .context(upstream_context!(&format!(
                "Failed to retrieve file size for '{}'",
                nwmgr_cfg.display()
            )))?
            .len();
    }

    let curr_exe = current_exe().context(upstream_context!(
        "Failed to retrieve path of current executable"
    ))?;
    req_size += curr_exe
        .metadata()
        .context(upstream_context!(&format!(
            "Failed to retrieve file size for '{}'",
            curr_exe.display()
        )))?
        .len();

    req_size += mig_info.get_assets().busybox_size() as u64;

    // TODO: account for network manager config and backup
    Ok(req_size)
}

fn copy_files<P: AsRef<Path>>(mig_info: &MigrateInfo, takeover_dir: P) -> Result<(), MigError> {
    let takeover_dir = takeover_dir.as_ref();
    let transfer_dir = path_append(takeover_dir, TRANSFER_DIR);

    if !dir_exists(&transfer_dir)? {
        create_dir(&transfer_dir).context(upstream_context!(&format!(
            "Failed to create transfer directory: '{}'",
            transfer_dir.display()
        )))?;
    }

    // *********************************************************
    // write busybox executable to tmpfs

    let busybox = mig_info.get_assets().write_to(&takeover_dir)?;

    info!("Copied busybox executable to '{}'", busybox.display());

    // *********************************************************
    // write balena image to tmpfs

    let to_image_path = path_append(&transfer_dir, BALENA_IMAGE_NAME);
    let image_path = mig_info.get_image_path();
    copy(image_path, &to_image_path).context(upstream_context!(&format!(
        "Failed to copy '{}' to {}",
        image_path.display(),
        &to_image_path.display()
    )))?;
    info!("Copied image to '{}'", to_image_path.display());

    // *********************************************************
    // write config.json to tmpfs

    let to_cfg_path = path_append(&transfer_dir, BALENA_CONFIG_PATH);
    let config_path = mig_info.get_balena_cfg().get_path();
    copy(config_path, &to_cfg_path).context(upstream_context!(&format!(
        "Failed to copy '{}' to {}",
        config_path.display(),
        &to_cfg_path.display()
    )))?;

    // *********************************************************
    // write network_manager filess to tmpfs
    let mut nwmgr_cfgs: u64 = 0;
    let nwmgr_path = path_append(&transfer_dir, SYSTEM_CONNECTIONS_DIR);
    create_dir_all(&nwmgr_path).context(upstream_context!(&format!(
        "Failed to create directory '{}",
        nwmgr_path.display()
    )))?;

    for source_file in mig_info.get_nwmgr_files() {
        nwmgr_cfgs += 1;
        let target_file = path_append(&nwmgr_path, &format!("balena-{:02}", nwmgr_cfgs));
        copy(&source_file, &target_file).context(upstream_context!(&format!(
            "Failed to copy '{}' to '{}'",
            source_file.display(),
            target_file.display()
        )))?;
    }

    for wifi_config in mig_info.get_wifis() {
        wifi_config.create_nwmgr_file(&nwmgr_path, nwmgr_cfgs)?;
    }

    // TODO: copy backup

    // *********************************************************
    // write this executable to tmpfs

    let target_path = path_append(takeover_dir, "takeover");
    let curr_exe = current_exe().context(upstream_context!(
        "Failed to retrieve path of current executable"
    ))?;

    copy(&curr_exe, &target_path).context(upstream_context!(&format!(
        "Failed to copy current executable '{}' to '{}",
        curr_exe.display(),
        target_path.display()
    )))?;

    info!("Copied current executable to '{}'", target_path.display());
    Ok(())
}

fn prepare(opts: &Options, mig_info: &mut MigrateInfo) -> Result<(), MigError> {
    info!("Preparing for takeover..");
    // *********************************************************
    // turn off swap
    if let Ok(cmd_res) = call(SWAPOFF_CMD, &["-a"], true) {
        if cmd_res.status.success() {
            info!("SWAP was disabled successfully");
        } else {
            error!("Failed to disable SWAP, stderr: '{}'", cmd_res.stderr);
            return Err(MigError::displayed());
        }
    }

    // *********************************************************
    // calculate required memory

    let (mem_tot, mem_free) = get_mem_info()?;
    info!(
        "Found {} total, {} free memory",
        format_size_with_unit(mem_tot),
        format_size_with_unit(mem_free)
    );

    let req_space = get_required_space(opts, mig_info)?;

    // TODO: maybe kill some procs first
    if mem_free < req_space + XTRA_FS_SIZE {
        error!(
            "Not enough memory space found to copy files to RAMFS, required size is {} free memory is {}",
            format_size_with_unit(req_space + XTRA_FS_SIZE),
            format_size_with_unit(mem_free)
        );
        return Err(MigError::displayed());
    }

    // *********************************************************
    // make mountpoint for tmpfs

    let takeover_dir = mktemp(true, Some("TO.XXXXXXXX"), Some("/"))?;

    mig_info.set_to_dir(&takeover_dir);

    info!("Created takeover directory in '{}'", takeover_dir.display());

    // *********************************************************
    // mount tmpfs

    mount_fs(&takeover_dir, "tmpfs", "tmpfs", mig_info)?;

    let curr_path = takeover_dir.join("etc");
    create_dir(&curr_path).context(upstream_context!(&format!(
        "Failed to create directory '{}'",
        curr_path.display()
    )))?;

    // *********************************************************
    // initialize essential paths

    let curr_path = curr_path.join("mtab");
    symlink("/proc/mounts", &curr_path).context(upstream_context!(&format!(
        "Failed to create symlink /proc/mounts to '{}'",
        curr_path.display()
    )))?;

    info!("Created mtab in  '{}'", curr_path.display());

    let curr_path = takeover_dir.join("proc");
    mount_fs(curr_path, "proc", "proc", mig_info)?;

    let curr_path = takeover_dir.join("tmp");
    mount_fs(&curr_path, "tmpfs", "tmpfs", mig_info)?;

    let curr_path = takeover_dir.join("sys");
    mount_fs(&curr_path, "sys", "sysfs", mig_info)?;

    let curr_path = takeover_dir.join("dev");
    if let Err(_) = mount_fs(&curr_path, "dev", "devtmpfs", mig_info) {
        mount_fs(&curr_path, "tmpfs", "tmpfs", mig_info)?;

        let cmd_res = call(
            CP_CMD,
            &["-a", "/dev/*", &*curr_path.to_string_lossy()],
            true,
        )?;
        if !cmd_res.status.success() {
            error!(
                "Failed to copy /dev file systemto '{}', error : '{}",
                curr_path.display(),
                cmd_res.stderr
            );
            return Err(MigError::displayed());
        }

        let curr_path = takeover_dir.join("dev/pts");
        if curr_path.exists() {
            remove_dir_all(&curr_path).context(upstream_context!(&format!(
                "Failed to delete directory '{}'",
                curr_path.display()
            )))?;
        }
    }

    let curr_path = takeover_dir.join("dev/pts");
    mount_fs(&curr_path, "devpts", "devpts", mig_info)?;

    // *********************************************************
    // create mountpoint for old root

    let curr_path = path_append(&takeover_dir, OLD_ROOT_MP);

    create_dir_all(&curr_path).context(upstream_context!(&format!(
        "Failed to create directory '{}'",
        curr_path.display()
    )))?;

    info!("Created directory '{}'", curr_path.display());

    copy_files(mig_info, &takeover_dir)?;

    // *********************************************************
    // setup new init

    let tty = read_link("/proc/self/fd/1")
        .context(upstream_context!("Failed to read link for /proc/self/fd/1"))?;

    let old_init_path = read_link("/proc/1/exe")
        .context(upstream_context!("Failed to read link for /proc/1/exe"))?;
    let new_init_path = takeover_dir
        .join("tmp")
        .join(old_init_path.file_name().unwrap());
    Assets::write_stage2_script(&takeover_dir, &new_init_path, &tty)?;

    let block_dev_info = BlockDeviceInfo::new()?;

    let flash_dev = if let Some(flash_dev) = opts.get_flash_to() {
        if let Some(flash_dev) = block_dev_info.get_devices().get(flash_dev) {
            flash_dev
        } else {
            error!(
                "Could not find configured flash device '{}'",
                flash_dev.display()
            );
            return Err(MigError::displayed());
        }
    } else {
        block_dev_info.get_root_device()
    };

    if !file_exists(&flash_dev.as_ref().get_dev_path()) {
        error!(
            "The device could not be found: '{}'",
            flash_dev.get_dev_path().display()
        );
        return Err(MigError::displayed());
    }

    // collect partitions that need to be unmounted
    let mut umount_parts: Vec<UmountPart> = Vec::new();

    for (_dev_path, device) in block_dev_info.get_devices() {
        if let Some(parent) = device.get_parent() {
            // this is a partition rather than a device
            if parent.get_name() == flash_dev.get_name() {
                // it is a partition of the flash device
                if let Some(mount) = device.get_mountpoint() {
                    let mut inserted = false;
                    for (idx, mpoint) in umount_parts.iter().enumerate() {
                        if mpoint.mountpoint.starts_with(mount.get_mountpoint()) {
                            umount_parts.insert(
                                idx,
                                UmountPart {
                                    dev_name: device.get_dev_path().to_path_buf(),
                                    mountpoint: PathBuf::from(mount.get_mountpoint()),
                                    fs_type: mount.get_fs_type().to_string(),
                                },
                            );
                            inserted = true;
                            break;
                        }
                    }
                    if !inserted {
                        umount_parts.push(UmountPart {
                            dev_name: device.get_dev_path().to_path_buf(),
                            mountpoint: PathBuf::from(mount.get_mountpoint()),
                            fs_type: mount.get_fs_type().to_string(),
                        });
                    }
                }
            }
        }
    }
    umount_parts.reverse();

    let s2_cfg = Stage2Config {
        log_dev: opts.get_log_to().clone(),
        log_level: mig_info.get_log_level().to_string(),
        flash_dev: flash_dev.get_dev_path().to_path_buf(),
        pretend: opts.is_pretend(),
        umount_parts,
        flash_external: opts.is_flash_external(),
    };

    let s2_cfg_path = takeover_dir.join(STAGE2_CONFIG_NAME);
    let mut s2_cfg_file = OpenOptions::new()
        .create(true)
        .write(true)
        .open(&s2_cfg_path)
        .context(upstream_context!(&format!(
            "Failed to open stage2 config file for writing: '{}'",
            s2_cfg_path.display()
        )))?;

    let s2_cfg_txt = s2_cfg.serialize()?;
    debug!("Stage 2 config: \n{}", s2_cfg_txt);

    s2_cfg_file
        .write(s2_cfg_txt.as_bytes())
        .context(upstream_context!(&format!(
            "Failed to write stage2 config file to '{}'",
            s2_cfg_path.display()
        )))?;

    info!("Wrote stage2 config to '{}'", s2_cfg_path.display());

    set_current_dir(&takeover_dir).context(upstream_context!(&format!(
        "Failed to change current dir to '{}'",
        takeover_dir.display()
    )))?;

    let cmd_res = call(
        MOUNT_CMD,
        &[
            "--bind",
            &*new_init_path.to_string_lossy(),
            &*old_init_path.to_string_lossy(),
        ],
        true,
    )?;
    if !cmd_res.status.success() {
        error!(
            "Failed to bindmount new init over old init, stder: '{}'",
            cmd_res.stderr
        );
        return Err(MigError::displayed());
    }

    info!("Bind-mounted new init as '{}'", new_init_path.display());

    debug!("calling '{} u'", TELINIT_CMD);
    let cmd_res = call(TELINIT_CMD, &["u"], true)?;
    if !cmd_res.status.success() {
        error!("Call to telinit failed, stderr: '{}'", cmd_res.stderr);
        return Err(MigError::displayed());
    }

    info!("Restarted init");

    Ok(())
}

pub fn stage1(opts: Options) -> Result<(), MigError> {
    if !is_admin()? {
        error!("please run this program as root");
        return Err(MigError::from(MigErrorKind::Displayed));
    }

    let mut mig_info = MigrateInfo::new(&opts)?;

    match prepare(&opts, &mut mig_info) {
        Ok(_) => {
            info!("Takeover initiated successfully, please wait for the device to reboot");
            Logger::flush();
            sync();
            sleep(Duration::from_secs(10));
            Ok(())
        }
        Err(why) => {
            if DO_CLEANUP {
                mig_info.umount_all();
            }
            return Err(why);
        }
    }
}
