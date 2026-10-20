use std::{
    env,
    path::{Path, PathBuf},
    process::{Command, ExitCode},
};

use shipfitter::{
    host_arch_suffix,
    package::{AppImage, cargo_build_release, crate_version},
};

const APP: &str = "irctest";

/// Libraries left to the host system, from the AppImage project's excludelist
/// (https://github.com/AppImageCommunity/pkg2appimage/blob/master/excludelist): glibc, the
/// graphics drivers' entry points, and a few others that must match the running system.
const EXCLUDED_LIBS: &[&str] = &[
    "ld-linux.so.2",
    "ld-linux-x86-64.so.2",
    "ld-linux-aarch64.so.1",
    "libanl.so.1",
    "libBrokenLocale.so.1",
    "libc.so.6",
    "libdl.so.2",
    "libm.so.6",
    "libmvec.so.1",
    "libpthread.so.0",
    "libresolv.so.2",
    "librt.so.1",
    "libthread_db.so.1",
    "libutil.so.1",
    "libstdc++.so.6",
    "libgcc_s.so.1",
    "libGL.so.1",
    "libEGL.so.1",
    "libGLdispatch.so.0",
    "libGLX.so.0",
    "libOpenGL.so.0",
    "libdrm.so.2",
    "libglapi.so.0",
    "libgbm.so.1",
    "libxcb.so.1",
    "libX11.so.6",
    "libX11-xcb.so.1",
    "libasound.so.2",
    "libfontconfig.so.1",
    "libfreetype.so.6",
    "libharfbuzz.so.0",
    "libfribidi.so.0",
    "libexpat.so.1",
    "libz.so.1",
    "libuuid.so.1",
    "libcom_err.so.2",
    "libgpg-error.so.0",
    "libICE.so.6",
    "libSM.so.6",
    "libusb-1.0.so.0",
    "libjack.so.0",
    "libpipewire-0.3.so.0",
    "libxcb-dri2.so.0",
    "libxcb-dri3.so.0",
    "libgmp.so.10",
];

fn main() -> ExitCode {
    let task = env::args().nth(1);
    let result = match task.as_deref() {
        Some("appimage") => appimage(),
        _ => {
            eprintln!("Usage: cargo xtask <task>\n\nTasks:\n  appimage  Build a release AppImage into target/release");
            return ExitCode::FAILURE;
        }
    };
    match result {
        Ok(()) => ExitCode::SUCCESS,
        Err(e) => {
            eprintln!("error: {e}");
            ExitCode::FAILURE
        }
    }
}

fn workspace_root() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).parent().unwrap().to_path_buf()
}

fn appimage() -> shipfitter::Result<()> {
    let root = workspace_root();
    cargo_build_release(&root, &[APP])?;
    let version = crate_version(&root, APP)?;
    let release_dir = root.join("target/release");
    let binary = release_dir.join(APP);
    let output = release_dir.join(format!("{APP}-{version}-{}.AppImage", host_arch_suffix()));
    let libs = bundled_libs(&binary)?;
    let mut binaries: Vec<&Path> = vec![&binary];
    binaries.extend(libs.iter().map(PathBuf::as_path));
    // The libraries sit beside the binary in usr/bin.
    let app_run = format!(
        r#"#!/bin/sh
HERE="$(dirname "$(readlink -f "${{0}}")")"
export LD_LIBRARY_PATH="${{HERE}}/usr/bin${{LD_LIBRARY_PATH:+:${{LD_LIBRARY_PATH}}}}"
exec "${{HERE}}/usr/bin/{APP}" "$@"
"#
    );
    let built = AppImage {
        binaries: &binaries,
        desktop_file: &root.join("packaging/irctest.desktop"),
        icon: &root.join("packaging/irctest.png"),
        app_run: Some(&app_run),
    }
    .build(&output)?;
    if !built {
        return Err("the AppImage wasn't built".into());
    }
    println!("Built {}", output.display());
    Ok(())
}

/// The shared libraries `binary` loads, as `ldd` resolves them on this system, less the ones in
/// [`EXCLUDED_LIBS`]. Each path is the soname link, so it's copied under the name the loader
/// looks for.
fn bundled_libs(binary: &Path) -> shipfitter::Result<Vec<PathBuf>> {
    let output = Command::new("ldd").arg(binary).output()?;
    if !output.status.success() {
        return Err(format!("ldd failed on {}", binary.display()).into());
    }
    let mut libs = Vec::new();
    for line in String::from_utf8_lossy(&output.stdout).lines() {
        let Some((name, rest)) = line.trim().split_once(" => ") else {
            continue;
        };
        if rest.starts_with("not found") {
            return Err(format!("{} needs {name}, which ldd couldn't find", binary.display()).into());
        }
        // The dynamic loader is listed by its full path.
        let name = name.rsplit('/').next().unwrap_or(name);
        if EXCLUDED_LIBS.contains(&name) {
            continue;
        }
        if let Some(path) = rest.split(" (").next().filter(|p| p.starts_with('/')) {
            libs.push(PathBuf::from(path));
        }
    }
    Ok(libs)
}
