use std::env;
use std::fs;
use std::io::{self, BufRead, Write};
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};

type DynError = Box<dyn std::error::Error>;
type Result<T> = std::result::Result<T, DynError>;

/// GitHub repository used for fetching prebuilt release assets.
/// Format: `"owner/repo"`
const GITHUB_REPO: &str = "https://github.com/tyler-potyondy/";

/// Release asset containing the prebuilt BabbleSim PHY simulator, shared
/// libraries, and components.  Expected to unpack into `tools/bsim/` relative
/// to the `external/` directory.
const BSIM_ASSET: &str = "bsim-binaries-linux-x86_64.tar.gz";

/// Release asset containing the prebuilt test-application binaries
/// (`zephyr_rpc_server_app` and `cgm_peripheral_sample`).  Expected to unpack
/// into `tools/bsim/bin/` relative to the `external/` directory.
const TEST_APPS_ASSET: &str = "test-app-binaries-linux-x86_64.tar.gz";

enum InstallMode {
    BuildFromSource,
    FetchPrebuilt,
}

/// Commands that compile or run Zephyr/BabbleSim artifacts must run on Linux.
fn require_linux(cmd: &str) -> Result<()> {
    if !cfg!(target_os = "linux") {
        return Err(format!(
            "`xtask {cmd}` requires Linux. \
             Use `cargo xtask docker-build` to build the dev-container image, \
             then work inside it."
        )
        .into());
    }
    Ok(())
}

fn main() {
    if let Err(err) = run() {
        eprintln!("Error: {err}");
        std::process::exit(1);
    }
}

fn run() -> Result<()> {
    let mut args = env::args().skip(1);
    let Some(cmd) = args.next() else {
        print_usage();
        return Ok(());
    };

    match cmd.as_str() {
        "zephyr-setup" => {
            require_linux("zephyr-setup")?;
            let args: Vec<String> = args.collect();
            let clean = args.iter().any(|a| a == "--clean");
            // --yes preserves the original devcontainer postCreateCommand behaviour
            // (build from source, no prompt).  --prebuilt skips to binary fetch.
            let mode = if args.iter().any(|a| a == "--prebuilt") {
                InstallMode::FetchPrebuilt
            } else if args.iter().any(|a| a == "--yes") {
                InstallMode::BuildFromSource
            } else {
                prompt_install_mode()?
            };
            zephyr_setup(clean, mode)
        }
        "run-bsim" => {
            require_linux("run-bsim")?;
            let Some(sim_id) = args.next() else {
                return Err("Simulation ID must be provided: xtask run-bsim <SIM_ID>".into());
            };
            run_bsim(&sim_id)
        }
        "docker-build" => docker_build(),
        "docker-attach" => docker_attach(),
        "-h" | "--help" | "help" => {
            print_usage();
            Ok(())
        }
        _ => Err(format!("Unknown command: {cmd}").into()),
    }
}

fn print_usage() {
    println!("Usage:");
    println!("  cargo xtask docker-build                        Build the dev-container image");
    println!("  cargo xtask docker-attach                       Open an interactive shell in the container");
    println!("  cargo xtask zephyr-setup [--clean]              Set up Zephyr/BabbleSim (prompts for install mode)");
    println!("  cargo xtask zephyr-setup [--clean] --prebuilt   Fetch prebuilt binaries from GitHub Releases");
    println!("  cargo xtask zephyr-setup [--clean] --yes        Build from source (non-interactive, for CI)");
    println!("  cargo xtask run-bsim <SIM_ID>                   Run BabbleSim (Linux only)");
}

// ── Install-mode prompt ──────────────────────────────────────────────────────

fn prompt_install_mode() -> Result<InstallMode> {
    println!();
    println!("How would you like to set up the Zephyr/BabbleSim environment?");
    println!("  [1] Build from source   (slow, ~30 min; requires a full Zephyr toolchain)");
    println!("  [2] Fetch prebuilt binaries  (fast; downloads a release archive from GitHub)");
    print!("Enter choice [1/2] (default: 2): ");
    io::stdout().flush()?;

    let line = io::stdin()
        .lock()
        .lines()
        .next()
        .ok_or("No input received — stdin was empty")??;

    match line.trim() {
        "1" => {
            println!("Selected: build from source.");
            Ok(InstallMode::BuildFromSource)
        }
        "2" | "" => {
            println!("Selected: fetch prebuilt binaries.");
            Ok(InstallMode::FetchPrebuilt)
        }
        other => Err(format!("Invalid choice '{other}'. Please enter 1 or 2.").into()),
    }
}

// ── Docker helpers ──────────────────────────────────────────────────────────

const DOCKER_IMAGE_TAG: &str = "nrf-sim-bridge:latest";

/// Build the dev-container image defined in `.devcontainer/Dockerfile`.
///
/// Equivalent to:
///   docker build --platform linux/amd64 -f .devcontainer/Dockerfile -t nrf-sim-bridge:latest .
fn docker_build() -> Result<()> {
    let root = workspace_root()?;
    let dockerfile = root.join(".devcontainer/Dockerfile");
    if !dockerfile.exists() {
        return Err(format!(
            "Dockerfile not found at {}",
            dockerfile.display()
        )
        .into());
    }

    let uid = std::env::var("UID").unwrap_or_else(|_| "1000".into());
    let gid = std::env::var("GID").unwrap_or_else(|_| "1000".into());

    println!("Building Docker image {DOCKER_IMAGE_TAG} …");
    run_cmd(
        "docker",
        &[
            "build",
            "--platform", "linux/amd64",
            "-f", ".devcontainer/Dockerfile",
            "--build-arg", &format!("USER_UID={uid}"),
            "--build-arg", &format!("USER_GID={gid}"),
            "-t", DOCKER_IMAGE_TAG,
            ".",
        ],
        Some(&root),
    )?;
    println!("Image built: {DOCKER_IMAGE_TAG}");
    Ok(())
}

/// Open an interactive bash shell inside the dev-container image.
///
/// The workspace is bind-mounted at `/workspace` so all source files and
/// build artifacts are available. Equivalent to:
///   docker run --rm -it --platform linux/amd64 -v <root>:/workspace -w /workspace nrf-sim-bridge:latest bash
fn docker_attach() -> Result<()> {
    let root = workspace_root()?;
    let workspace = root
        .to_str()
        .ok_or("Workspace path contains non-UTF-8 characters")?;

    run_cmd(
        "docker",
        &[
            "run",
            "--rm",
            "--interactive",
            "--tty",
            "--platform", "linux/amd64",
            "-v", &format!("{workspace}:/workspace"),
            "-w", "/workspace",
            DOCKER_IMAGE_TAG,
            "bash",
        ],
        Some(&root),
    )
}

// ── Workspace / utility helpers ──────────────────────────────────────────────

/// Walk up from `cwd` until we find a directory containing `Cargo.toml`.
fn workspace_root() -> Result<PathBuf> {
    let mut dir = env::current_dir()?;
    loop {
        if dir.join("Cargo.toml").exists() {
            return Ok(dir);
        }
        if !dir.pop() {
            return Err("Could not find workspace root (no Cargo.toml found in any parent directory)".into());
        }
    }
}

fn run_cmd(cmd: &str, args: &[&str], cwd: Option<&Path>) -> Result<()> {
    let mut command = Command::new(cmd);
    command.args(args);
    if let Some(cwd) = cwd {
        command.current_dir(cwd);
    }
    command.stdin(Stdio::inherit());
    command.stdout(Stdio::inherit());
    command.stderr(Stdio::inherit());

    let status = command.status()?;
    if !status.success() {
        return Err(format!("Command failed: {cmd} {}", args.join(" ")).into());
    }
    Ok(())
}

/// Delete all contents of `dir` except `.gitignore`, leaving the directory itself intact.
fn clean_dir(dir: &Path) -> Result<()> {
    if !dir.exists() {
        return Ok(());
    }
    for entry in fs::read_dir(dir)? {
        let entry = entry?;
        if entry.file_name() == ".gitignore" {
            continue;
        }
        let path = entry.path();
        if path.is_dir() {
            fs::remove_dir_all(path)?;
        } else {
            fs::remove_file(path)?;
        }
    }
    Ok(())
}

/// Download a single GitHub release asset and extract it into `extract_dir`.
/// The archive is deleted after successful extraction.
fn download_and_extract(asset: &str, extract_dir: &Path, root: &Path) -> Result<()> {
    let url = format!(
        "https://github.com/{GITHUB_REPO}/releases/latest/download/{asset}"
    );
    let archive = extract_dir.join(asset);
    let archive_str = archive.to_str().ok_or("Archive path contains non-UTF-8 characters")?;
    let extract_str = extract_dir.to_str().ok_or("Extract path contains non-UTF-8 characters")?;

    println!("Downloading {url} ...");
    run_cmd(
        "curl",
        &["--location", "--fail", "--progress-bar", "--output", archive_str, &url],
        Some(root),
    )?;

    println!("Extracting {asset} into {extract_str} ...");
    run_cmd(
        "tar",
        &["--extract", "--gzip", "--file", archive_str, "--directory", extract_str],
        Some(root),
    )?;

    fs::remove_file(&archive)?;
    Ok(())
}

fn fetch_prebuilt_binaries(root: &Path, external_dir: &Path) -> Result<()> {
    // 1. BabbleSim PHY simulator, shared libraries, and components.
    download_and_extract(BSIM_ASSET, external_dir, root)?;

    // 2. Zephyr test-application binaries (rpc_server_app + cgm_peripheral_sample).
    let bsim_bin = external_dir.join("tools/bsim/bin");
    fs::create_dir_all(&bsim_bin)?;
    download_and_extract(TEST_APPS_ASSET, &bsim_bin, root)?;

    println!("Done. All prebuilt binaries are in external/tools/bsim/");
    Ok(())
}

fn zephyr_setup(clean: bool, mode: InstallMode) -> Result<()> {
    let root = workspace_root()?;
    let external_dir = root.join("external");

    if clean {
        println!("Cleaning up {}...", external_dir.display());
        clean_dir(&external_dir)?;
    }

    fs::create_dir_all(&external_dir)?;

    if let InstallMode::FetchPrebuilt = mode {
        return fetch_prebuilt_binaries(&root, &external_dir);
    }

    println!("Setting up nrf submodule...");
    run_cmd(
        "git",
        &["submodule", "update", "--init", "external/nrf"],
        Some(&root),
    )?;

    let venv_dir = external_dir.join(".venv");
    let venv_python = venv_dir.join("bin/python3");
    // Written only after all requirements are successfully installed (see
    // below). Its absence means setup was interrupted before completion.
    let venv_stamp = venv_dir.join(".requirements_installed");

    let python_ok = venv_python.exists()
        && Command::new(&venv_python)
            .arg("--version")
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status()
            .map(|s| s.success())
            .unwrap_or(false);
    let venv_valid = python_ok && venv_stamp.exists();

    if !venv_valid {
        if venv_dir.exists() {
            if !python_ok {
                println!("Existing venv is stale or from a different Python, recreating...");
            } else {
                println!("Existing venv has incomplete requirements, recreating...");
            }
            fs::remove_dir_all(&venv_dir)?;
        } else {
            println!("Creating venv...");
        }
        run_cmd("python3", &["-m", "venv", ".venv"], Some(&external_dir))?;
    }

    let pip = external_dir.join(".venv/bin/pip");
    let west = external_dir.join(".venv/bin/west");
    let pip_str = pip.to_str().ok_or("Invalid UTF-8 path for pip")?;
    let west_str = west.to_str().ok_or("Invalid UTF-8 path for west")?;

    // Replicate what `source .venv/bin/activate` does in the original shell
    // script. Without this, cmake's find_package(Python3) searches PATH and
    // caches /usr/bin/python3 instead of the venv python. That cached value
    // is then forwarded by sysbuild_cache to every sub-cmake image invocation,
    // causing "No module named pykwalify" failures deep inside the configure
    // step. Prepending the venv bin to PATH ensures find_package(Python3)
    // picks up the venv python first, and VIRTUAL_ENV is the fallback hint
    // that python.cmake's find_program uses in sub-cmake calls that lack
    // WEST_PYTHON.
    let venv_bin = venv_dir.join("bin");
    let venv_bin_str = venv_bin.to_str().ok_or("Invalid UTF-8 path for venv bin")?;
    let path = env::var("PATH").unwrap_or_default();
    std::env::set_var("PATH", format!("{venv_bin_str}:{path}"));
    std::env::set_var("VIRTUAL_ENV", &venv_dir);

    run_cmd(pip_str, &["install", "west"], Some(&external_dir))?;

    let west_state = external_dir.join(".west");
    if west_state.exists() {
        println!("Previous west workspace found, resetting...");
        fs::remove_dir_all(west_state)?;
    }

    run_cmd(west_str, &["init", "-l", "nrf"], Some(&external_dir))?;
    println!("Fetching west dependencies (BabbleSim + Zephyr)...");
    run_cmd(
        west_str,
        &["config", "manifest.group-filter", "--", "+babblesim"],
        Some(&external_dir),
    )?;
    run_cmd(west_str, &["update"], Some(&external_dir))?;

    run_cmd(
        pip_str,
        &["install", "-r", "nrf/scripts/requirements.txt"],
        Some(&external_dir),
    )?;
    run_cmd(
        pip_str,
        &["install", "-r", "zephyr/scripts/requirements.txt"],
        Some(&external_dir),
    )?;

    // Confirm every package listed in both requirements files is satisfied
    // before starting the expensive builds. `pip install --dry-run` resolves
    // the full dependency graph against the current environment and prints
    // "Would install X" for anything that's missing or out of range; we treat
    // any such output as an installation failure.
    println!("Verifying all requirements are installed...");
    let dry_run = Command::new(pip_str)
        .args([
            "install",
            "-r", "nrf/scripts/requirements.txt",
            "-r", "zephyr/scripts/requirements.txt",
            "--dry-run",
        ])
        .current_dir(&external_dir)
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .output()?;

    let dry_run_out = String::from_utf8_lossy(&dry_run.stdout);
    let dry_run_err = String::from_utf8_lossy(&dry_run.stderr);
    let combined = format!("{dry_run_out}{dry_run_err}");
    if combined.contains("Would install") {
        return Err(format!(
            "Requirements are not fully installed after pip install — \
             the following packages are still missing or out of range:\n{combined}\n\
             Re-run with --clean to start fresh."
        )
        .into());
    }

    // All requirements confirmed present; record this so subsequent runs can
    // skip re-installation when the venv is already up to date.
    fs::write(&venv_stamp, "")?;

    println!("Building BabbleSim...");
    run_cmd(
        "make",
        &["-C", "tools/bsim", "everything", "-j", "4"],
        Some(&external_dir),
    )?;

    println!("Checking out cgm-bsim branch in nrf...");
    let current_branch = Command::new("git")
        .args(["-C", "nrf", "rev-parse", "--abbrev-ref", "HEAD"])
        .current_dir(&external_dir)
        .output()?;
    if !current_branch.status.success() {
        return Err("Failed to read current nrf branch".into());
    }
    if String::from_utf8(current_branch.stdout)?.trim() != "cgm-bsim" {
        run_cmd(
            "git",
            &["-C", "nrf", "fetch", "origin", "cgm-bsim"],
            Some(&external_dir),
        )?;
        run_cmd(
            "git",
            &["-C", "nrf", "checkout", "-B", "cgm-bsim", "FETCH_HEAD"],
            Some(&external_dir),
        )?;
    }

    println!("Building Zephyr server app...");
    run_cmd(
        west_str,
        &[
            "build", "-b", "nrf52_bsim", "-p", "always",
            "--build-dir", "build/zephyr_server_app",
            "nrf/samples/nrf_rpc/protocols_serialization/server",
            "-S", "ble",
        ],
        Some(&external_dir),
    )?;

    println!("Building CGM peripheral sample...");
    run_cmd(
        west_str,
        &[
            "build", "-b", "nrf52_bsim", "-p", "always",
            "--build-dir", "build/cgm_peripheral_sample",
            "nrf/samples/bluetooth/peripheral_cgms",
        ],
        Some(&external_dir),
    )?;

    fs::copy(
        external_dir.join("build/zephyr_server_app/server/zephyr/zephyr.exe"),
        external_dir.join("tools/bsim/bin/zephyr_rpc_server_app"),
    )?;
    fs::copy(
        external_dir.join("build/cgm_peripheral_sample/peripheral_cgms/zephyr/zephyr.exe"),
        external_dir.join("tools/bsim/bin/cgm_peripheral_sample"),
    )?;

    println!("Done. Build artifacts copied to external/tools/bsim/bin/");
    Ok(())
}

fn bsim_ld_library_path() -> String {
    match env::var("LD_LIBRARY_PATH") {
        Ok(existing) => format!("external/tools/bsim/lib:{existing}"),
        Err(_) => "external/tools/bsim/lib".to_string(),
    }
}

fn spawn_in_bsim_bin(sim_id: &str, exe: &str, args: &[&str]) -> Result<Child> {
    let bsim_bin = Path::new("external/tools/bsim/bin");
    Command::new(exe)
        .args(args)
        .current_dir(bsim_bin)
        .stdin(Stdio::inherit())
        .stdout(Stdio::inherit())
        .stderr(Stdio::inherit())
        .env("BSIM_OUT_PATH", "external/tools/bsim")
        .env("BSIM_COMPONENTS_PATH", "external/tools/bsim/components")
        .env("LD_LIBRARY_PATH", bsim_ld_library_path())
        .spawn()
        .map_err(|e| format!("Failed to spawn '{exe}' for sim '{sim_id}': {e}").into())
}

fn pkill_sim(sim_id: &str) {
    for process in ["bs_2G4_phy_v1", "zephyr_rpc_server_app", "cgm_peripheral_sample"] {
        let pattern = format!("{process} -s={sim_id}");
        let _ = Command::new("pkill").args(["-f", &pattern]).status();
        let _ = Command::new("pkill").args(["-9", "-f", &pattern]).status();
    }
}

fn run_bsim(sim_id: &str) -> Result<()> {
    pkill_sim(sim_id);

    let _ = fs::remove_dir_all(format!(
        "/tmp/bs_{}/{}",
        env::var("USER").unwrap_or_default(),
        sim_id
    ));

    println!("Starting BabbleSim PHY simulator...");
    let _phy = spawn_in_bsim_bin(
        sim_id,
        "./bs_2G4_phy_v1",
        &[&format!("-s={sim_id}"), "-D=2", "-sim_length=86400e6"],
    )?;

    println!("Starting nRF RPC server with BabbleSim...");
    println!();
    println!("=== BabbleSim Running ===");
    println!("Simulation ID: {sim_id}");
    println!("Simulation length: 86400 seconds (24 hours simulated, ~39 seconds real time at 2200x speed)");
    println!();
    println!("To test RX, run in another terminal:");
    println!("  socat UNIX-LISTEN:/tmp/nrf_rpc_server.sock,fork /dev/pts/XX,raw,echo=0");
    println!("  printf '\\x04\\x00\\xff\\x00\\xff\\x00\\x62\\x74\\x5f\\x72\\x70\\x63' | socat - UNIX-CONNECT:/tmp/nrf_rpc_server.sock");
    println!();
    println!("Starting device (Press Ctrl+C to stop)...");
    println!();

    let mut zephyr = spawn_in_bsim_bin(
        sim_id,
        "./zephyr_rpc_server_app",
        &[&format!("-s={sim_id}"), "-d=0", "-uart0_pty", "-uart_pty_pollT=1000"],
    )?;

    let cgm_log = fs::File::create("external/tools/bsim/bin/cgm_peripheral_sample.log")?;
    let mut cgm = Command::new("./cgm_peripheral_sample")
        .args([&format!("-s={sim_id}"), "-d=1"])
        .current_dir("external/tools/bsim/bin")
        .stdin(Stdio::null())
        .stdout(cgm_log.try_clone()?)
        .stderr(cgm_log)
        .env("BSIM_OUT_PATH", "external/tools/bsim")
        .env("BSIM_COMPONENTS_PATH", "external/tools/bsim/components")
        .env("LD_LIBRARY_PATH", bsim_ld_library_path())
        .spawn()?;

    let zephyr_status = zephyr.wait()?;
    let _ = cgm.kill();
    if !zephyr_status.success() {
        return Err(format!("zephyr_rpc_server_app exited with status: {zephyr_status}").into());
    }
    Ok(())
}
