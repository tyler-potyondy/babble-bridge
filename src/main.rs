#[cfg(not(target_os = "linux"))]
compile_error!("nrf-sim-bridge xtask only supports Linux. Please either run on a linux machine or using the repo's container image definition.");

use std::env;
use std::fs;
use std::io::{self, Write};
use std::path::Path;
use std::process::{Child, Command, Stdio};

type DynError = Box<dyn std::error::Error>;
type Result<T> = std::result::Result<T, DynError>;

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
            let auto_yes = args.any(|a| a == "-y" || a == "--yes");
            zephyr_setup(auto_yes)
        }
        "run-bsim" => {
            let Some(sim_id) = args.next() else {
                return Err("Simulation ID must be provided: xtask run-bsim <SIM_ID>".into());
            };
            run_bsim(&sim_id)
        }
        "-h" | "--help" | "help" => {
            print_usage();
            Ok(())
        }
        _ => Err(format!("Unknown command: {cmd}").into()),
    }
}

fn print_usage() {
    println!("Usage:");
    println!("  cargo xtask zephyr-setup [--yes]");
    println!("  cargo xtask run-bsim <SIM_ID>");
}

/// Walk up from `cwd` until we find a directory containing `Cargo.toml`.
fn workspace_root() -> Result<std::path::PathBuf> {
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
        let joined = args.join(" ");
        return Err(format!("Command failed: {cmd} {joined}").into());
    }
    Ok(())
}

fn clean_build_dir(build_dir: &Path) -> Result<()> {
    if !build_dir.exists() {
        return Ok(());
    }

    for entry in fs::read_dir(build_dir)? {
        let entry = entry?;
        let path = entry.path();
        let name = entry.file_name();
        if name == ".gitignore" {
            continue;
        }

        if path.is_dir() {
            fs::remove_dir_all(path)?;
        } else {
            fs::remove_file(path)?;
        }
    }
    Ok(())
}

fn prompt_yes_no(prompt: &str) -> Result<bool> {
    print!("{prompt} ");
    io::stdout().flush()?;

    let mut line = String::new();
    io::stdin().read_line(&mut line)?;
    Ok(line.trim() == "y")
}

fn zephyr_setup(auto_yes: bool) -> Result<()> {
    let root = workspace_root()?;
    let external_dir = root.join("external");

    // To ensure a clean install, we delete the external directory and recreate it.
    if !auto_yes {
        let proceed = prompt_yes_no(&format!(
            "WARNING: zephyr_setup will delete and reinstall a clean zephyr setup in\n  {}\nProceed? (y/n)",
            external_dir.display()
        ))?;
        if !proceed {
            println!("Aborting zephyr setup.");
            return Ok(());
        }
    }

    println!("Cleaning up {}...", external_dir.display());
    clean_build_dir(&external_dir)?;
    fs::create_dir_all(&external_dir)?;

    println!("Setting up zephyr submodule and building server app executable...");
    run_cmd(
        "git",
        &["submodule", "update", "--init", "external/nrf"],
        Some(&root),
    )?;

    // Copy the nrf submodule tree into our build dir so west can own it cleanly.
    let nrf_src = external_dir.join("nrf");
    let nrf_dst = external_dir.join("nrf");
    if nrf_src.exists() && !nrf_dst.exists() {
        run_cmd(
            "cp",
            &["-r", nrf_src.to_str().unwrap(), nrf_dst.to_str().unwrap()],
            None,
        )?;
    }

    let venv_dir = external_dir.join(".venv");
    if !venv_dir.exists() {
        println!("Creating venv");
        run_cmd("python3", &["-m", "venv", ".venv"], Some(&external_dir))?;
    }

    let pip = external_dir.join(".venv/bin/pip");
    let west = external_dir.join(".venv/bin/west");
    let pip_str = pip
        .to_str()
        .ok_or("Invalid UTF-8 path for pip executable")?;
    let west_str = west
        .to_str()
        .ok_or("Invalid UTF-8 path for west executable")?;

    run_cmd(pip_str, &["install", "west"], Some(&external_dir))?;
    println!("Entered venv");

    let west_state = external_dir.join(".west");
    if west_state.exists() {
        println!("Previous west setup found, resetting...");
        fs::remove_dir_all(west_state)?;
    }

    run_cmd(west_str, &["init", "-l", "nrf"], Some(&external_dir))?;
    println!("Updating west Babble Simulator...");
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

    println!("Building Babble Simulator...");
    run_cmd(
        "make",
        &["-C", "tools/bsim", "everything", "-j", "4"],
        Some(&external_dir),
    )?;

    println!("Building Zephyr nrf_rpc protocol_serialization server example...");
    let current_branch_output = Command::new("git")
        .args(["-C", "nrf", "rev-parse", "--abbrev-ref", "HEAD"])
        .current_dir(&external_dir)
        .output()?;
    if !current_branch_output.status.success() {
        return Err("Failed to read current nrf branch".into());
    }

    let current_branch = String::from_utf8(current_branch_output.stdout)?.trim().to_string();
    if current_branch != "cgm-bsim" {
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

    run_cmd(
        west_str,
        &[
            "build",
            "-b",
            "nrf52_bsim",
            "-p",
            "always",
            "--build-dir",
            "build/zephyr_server_app",
            "nrf/samples/nrf_rpc/protocols_serialization/server",
            "-S",
            "ble",
        ],
        Some(&external_dir),
    )?;
    run_cmd(
        west_str,
        &[
            "build",
            "-b",
            "nrf52_bsim",
            "-p",
            "always",
            "--build-dir",
            "build/cgm_peripheral_sample",
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

    println!("Build artifacts copied to tools/bsim/bin/");
    Ok(())
}

fn spawn_in_bsim_bin(sim_id: &str, args: &[&str]) -> Result<Child> {
    let bsim_bin = Path::new("external/tools/bsim/bin");
    let mut command = Command::new(args[0]);
    command.args(&args[1..]);
    command.current_dir(bsim_bin);
    command.stdin(Stdio::inherit());
    command.stdout(Stdio::inherit());
    command.stderr(Stdio::inherit());
    command.env("BSIM_OUT_PATH", "external/tools/bsim");
    command.env("BSIM_COMPONENTS_PATH", "external/tools/bsim/components");
    if let Ok(existing_ld) = env::var("LD_LIBRARY_PATH") {
        command.env(
            "LD_LIBRARY_PATH",
            format!("external/tools/bsim/lib:{existing_ld}"),
        );
    } else {
        command.env("LD_LIBRARY_PATH", "external/tools/bsim/lib");
    }
    let child = command.spawn().map_err(|e| {
        format!(
            "Failed to spawn '{}' for sim '{}': {}",
            args[0],
            sim_id,
            e
        )
    })?;
    Ok(child)
}

fn bsim_ld_library_path() -> String {
    if let Ok(existing_ld) = env::var("LD_LIBRARY_PATH") {
        format!("external/tools/bsim/lib:{existing_ld}")
    } else {
        "external/tools/bsim/lib".to_string()
    }
}

fn run_bsim(sim_id: &str) -> Result<()> {
    let _ = Command::new("pkill")
        .args(["-f", &format!("bs_2G4_phy_v1 -s={sim_id}")])
        .status();
    let _ = Command::new("pkill")
        .args(["-f", &format!("zephyr_rpc_server_app -s={sim_id}")])
        .status();
    let _ = Command::new("pkill")
        .args(["-f", &format!("cgm_peripheral_sample -s={sim_id}")])
        .status();
    let _ = Command::new("pkill")
        .args(["-9", "-f", &format!("bs_2G4_phy_v1 -s={sim_id}")])
        .status();
    let _ = Command::new("pkill")
        .args(["-9", "-f", &format!("zephyr_rpc_server_app -s={sim_id}")])
        .status();
    let _ = Command::new("pkill")
        .args(["-9", "-f", &format!("cgm_peripheral_sample -s={sim_id}")])
        .status();

    let _ = fs::remove_dir_all(format!("/tmp/bs_{}/{sim_id}", env::var("USER").unwrap_or_default()));

    println!("Starting BabbleSim PHY simulator...");
    let _phy = spawn_in_bsim_bin(
        sim_id,
        &["./bs_2G4_phy_v1", &format!("-s={sim_id}"), "-D=2", "-sim_length=86400e6"],
    )?;

    println!("Starting nRF RPC server with BabbleSim...");
    println!();
    println!("=== BabbleSim Running ===");
    println!("Simulation ID: {sim_id}");
    println!(
        "Simulation length: 86400 seconds (24 hours simulated, ~39 seconds real time at 2200x speed)"
    );
    println!();
    println!("To test RX, run in another terminal:");
    println!("  socat UNIX-LISTEN:/tmp/nrf_rpc_server.sock,fork /dev/pts/XX,raw,echo=0");
    println!(
        "  printf '\\x04\\x00\\xff\\x00\\xff\\x00\\x62\\x74\\x5f\\x72\\x70\\x63' | socat - UNIX-CONNECT:/tmp/nrf_rpc_server.sock"
    );
    println!();
    println!("Starting device (Press Ctrl+C to stop)...");
    println!();

    let mut zephyr = spawn_in_bsim_bin(
        sim_id,
        &[
            "./zephyr_rpc_server_app",
            &format!("-s={sim_id}"),
            "-d=0",
            "-uart0_pty",
            "-uart_pty_pollT=1000",
        ],
    )?;

    let cgm_log = fs::File::create("external/tools/bsim/bin/cgm_peripheral_sample.log")?;
    let cgm_log_err = cgm_log.try_clone()?;
    let mut cgm = Command::new("./cgm_peripheral_sample")
        .args([&format!("-s={sim_id}"), "-d=1"])
        .current_dir("external/tools/bsim/bin")
        .stdin(Stdio::null())
        .stdout(Stdio::from(cgm_log))
        .stderr(Stdio::from(cgm_log_err))
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
