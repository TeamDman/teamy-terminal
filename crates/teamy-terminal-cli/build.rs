use std::process::Command;
use std::time::SystemTime;
use std::time::UNIX_EPOCH;

fn main() {
    add_build_script_inputs();
    add_exe_resources();
    add_git_metadata();
    add_toolchain_metadata();
    add_build_timestamp();
}

fn add_build_script_inputs() {
    println!("cargo:rerun-if-changed=build.rs");
    println!("cargo:rerun-if-changed=Cargo.toml");
    println!("cargo:rerun-if-changed=src");
    println!("cargo:rerun-if-changed=resources");
}

fn add_exe_resources() {
    embed_resource::compile("resources/app.rc", embed_resource::NONE)
        .manifest_required()
        .expect("failed to embed resources");
}

fn add_git_metadata() {
    add_git_metadata_inputs();

    let rev =
        git_output(&["rev-parse", "--short", "HEAD"]).unwrap_or_else(|| "unknown".to_string());
    let branch = git_output(&["branch", "--show-current"]).unwrap_or_else(|| {
        git_output(&["describe", "--tags", "--exact-match"])
            .unwrap_or_else(|| "detached".to_string())
    });
    let repository = git_output(&["remote", "get-url", "origin"])
        .or_else(|| std::env::var("CARGO_PKG_REPOSITORY").ok())
        .unwrap_or_else(|| "unknown".to_string());
    let dirty = match git_output_allow_empty(&["status", "--short", "--untracked-files=no"]) {
        Some(status) if status.is_empty() => "clean",
        Some(_) => "dirty",
        None => "unknown",
    };

    println!("cargo:rustc-env=GIT_REVISION={rev}");
    println!("cargo:rustc-env=GIT_BRANCH={branch}");
    println!("cargo:rustc-env=GIT_REPOSITORY_URL={repository}");
    println!("cargo:rustc-env=GIT_WORKTREE_STATUS={dirty}");
}

fn add_git_metadata_inputs() {
    if let Some(git_config_path) = git_output(&["rev-parse", "--git-path", "config"]) {
        println!("cargo:rerun-if-changed={git_config_path}");
    }

    if let Some(head_path) = git_output(&["rev-parse", "--git-path", "HEAD"]) {
        println!("cargo:rerun-if-changed={head_path}");
    }

    if let Some(head_ref) = git_output(&["symbolic-ref", "--quiet", "HEAD"])
        && let Some(head_ref_path) = git_output(&["rev-parse", "--git-path", &head_ref])
    {
        println!("cargo:rerun-if-changed={head_ref_path}");
    }
}

fn add_toolchain_metadata() {
    let rustc = std::env::var_os("RUSTC").unwrap_or_else(|| "rustc".into());
    let rustc_version = Command::new(rustc)
        .arg("--version")
        .output()
        .ok()
        .filter(|output| output.status.success())
        .and_then(|output| String::from_utf8(output.stdout).ok())
        .map_or_else(
            || "unknown".to_string(),
            |version| version.trim().to_string(),
        );
    let target = std::env::var("TARGET").unwrap_or_else(|_| "unknown".to_string());
    let host = std::env::var("HOST").unwrap_or_else(|_| "unknown".to_string());
    let profile = std::env::var("PROFILE").unwrap_or_else(|_| "unknown".to_string());
    println!("cargo:rustc-env=RUSTC_VERSION={rustc_version}");
    println!("cargo:rustc-env=BUILD_TARGET={target}");
    println!("cargo:rustc-env=BUILD_HOST={host}");
    println!("cargo:rustc-env=BUILD_PROFILE={profile}");
}

fn git_output(args: &[&str]) -> Option<String> {
    git_output_allow_empty(args).filter(|s| !s.is_empty())
}

fn git_output_allow_empty(args: &[&str]) -> Option<String> {
    Command::new("git")
        .args(args)
        .output()
        .ok()
        .and_then(|o| o.status.success().then_some(o.stdout))
        .and_then(|v| String::from_utf8(v).ok())
        .map(|s| s.trim().to_string())
}

fn add_build_timestamp() {
    let timestamp = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .expect("system clock should be after Unix epoch")
        .as_secs();

    println!("cargo:rustc-env=BUILD_TIMESTAMP_UNIX={timestamp}");
}
