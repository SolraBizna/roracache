use std::{
    env,
    ffi::OsString,
    process,
};

use fuser::MountOption;

mod fs;

const MOUNT_OPTIONS: &[MountOption] = &[
    MountOption::AutoUnmount, MountOption::AllowOther,
    MountOption::NoDev, MountOption::NoSuid, MountOption::RO,
    MountOption::DefaultPermissions,
];

fn main() {
    if env::args_os().count() != 3 {
        eprintln!("Usage: roracache /path/to/source /path/to/mountpoint");
        process::exit(1);
    }
    let mut args = env::args_os().skip(1);
    let srcpath: OsString = args.next().unwrap();
    let mountpoint: OsString = args.next().unwrap();
    assert!(args.next().is_none());
    let fs = fs::FS::new(srcpath.into());
    fuser::mount2(fs, mountpoint, MOUNT_OPTIONS)
        .expect("fuser::mount2 returned an error")
}
