use std::env;
use std::path::PathBuf;
use std::process::Command;

fn main() {
    let manifest_dir = PathBuf::from(env::var("CARGO_MANIFEST_DIR").unwrap());

    if cfg!(target_os = "windows") {
        // Build cef_hook DLL on Windows
        let cef_hook_dir = manifest_dir.join("cef_hook");
        if cef_hook_dir.exists() {
            eprintln!("build.rs: building cef_hook DLL...");

            let mut cmd = Command::new("cargo");
            cmd.args(["build", "--release", "--manifest-path"]);
            cmd.arg(cef_hook_dir.join("Cargo.toml"));

            let status = cmd
                .status()
                .expect("Failed to run cargo build for cef_hook");
            if !status.success() {
                panic!("cargo build for cef_hook failed with status: {}", status);
            }

            let cef_hook_dll = cef_hook_dir
                .join("target")
                .join("release")
                .join("lumaforge_cef_hook.dll");

            if cef_hook_dll.exists() {
                let out_dir = PathBuf::from(env::var("OUT_DIR").unwrap());
                let target_release = out_dir
                    .parent()
                    .unwrap()
                    .parent()
                    .unwrap()
                    .parent()
                    .unwrap();
                let dest = target_release.join("lumaforge_cef_hook.dll");
                eprintln!("build.rs: copying cef_hook DLL to {}", dest.display());
                std::fs::copy(&cef_hook_dll, &dest).expect("Failed to copy cef_hook DLL");
            }

            println!("cargo:rerun-if-changed={}", cef_hook_dll.display());
        }
    } else {
        // Build execve hook .so on Linux
        let hook_dir = manifest_dir.join("hook");
        if hook_dir.exists() {
            eprintln!("build.rs: building lumaforge_hook.so...");

            let mut cmd = Command::new("cargo");
            cmd.args(["build", "--release", "--manifest-path"]);
            cmd.arg(hook_dir.join("Cargo.toml"));

            let status = cmd
                .status()
                .expect("Failed to run cargo build for hook");
            if !status.success() {
                panic!("cargo build for hook failed with status: {}", status);
            }

            let hook_so = hook_dir
                .join("target")
                .join("release")
                .join("liblumaforge_hook.so");

            if hook_so.exists() {
                let out_dir = PathBuf::from(env::var("OUT_DIR").unwrap());
                let target_release = out_dir
                    .parent()
                    .unwrap()
                    .parent()
                    .unwrap()
                    .parent()
                    .unwrap();
                let dest = target_release.join("lumaforge_hook.so");
                eprintln!("build.rs: copying hook .so to {}", dest.display());
                std::fs::copy(&hook_so, &dest).expect("Failed to copy hook .so");
            }

            println!("cargo:rerun-if-changed={}", hook_so.display());
        }
    }
}
