use std::env;
use std::fs::{self, File};
use std::path::Path;
use std::process::{Command, Stdio};
use std::thread;
use std::time::Duration;

const RUN_P11_KIT_DIR: &str = "/run/p11-kit";
const P11_KIT_SERVER_ENV_BASE: &str = "/tmp/pkcs11";

// Syslog logger helper
fn logger(msg: &str) {
    if let Ok(stream) = std::os::unix::net::UnixDatagram::unbound() {
        if stream.connect("/dev/log").is_ok() {
            let _ = stream.send(format!("<13>p11-manager: {}", msg).as_bytes());
        }
    }
}

fn get_arch_triplet() -> Option<&'static str> {
    match env::var("SNAP_ARCH").as_deref() {
        Ok("arm64") => Some("aarch64-linux-gnu"),
        Ok("armhf") => Some("arm-linux-gnueabihf"),
        Ok("amd64") => Some("x86_64-linux-gnu"),
        _ => None,
    }
}

fn stop_p11_kit_servers() {
    let dir = Path::new("/tmp");
    if !dir.is_dir() {
        logger("No running p11-kit servers found");
        return;
    }

    let entries = match fs::read_dir(dir) {
        Ok(e) => e,
        Err(_) => return,
    };

    let mut found = false;
    for entry in entries.flatten() {
        let name = entry.file_name();
        let name_str = name.to_string_lossy();

        if name_str.starts_with("pkcs11-") && name_str.ends_with(".env") {
            found = true;
            let path = entry.path();

            if let Ok(content) = fs::read_to_string(&path) {
                let mut pid = "";
                let mut addr = "";
                for line in content.lines() {
                    if let Some(stripped) = line.strip_prefix("P11_KIT_SERVER_PID=") {
                        pid = stripped.trim_matches('"');
                    } else if let Some(stripped) = line.strip_prefix("P11_KIT_SERVER_ADDRESS=") {
                        addr = stripped.trim_matches('"');
                    }
                }

                logger(&format!("Stopping p11-kit server: PID={}, {}", pid, addr));

                let _ = Command::new("/usr/bin/p11-kit")
                    .arg("server")
                    .arg("-k")
                    .env("P11_KIT_SERVER_PID", pid)
                    .env("P11_KIT_SERVER_ADDRESS", addr)
                    .stdout(Stdio::null())
                    .stderr(Stdio::null())
                    .status();
            }
            let _ = fs::remove_file(path);
        }
    }

    if !found {
        logger("No running p11-kit servers found");
    }
}

fn start_p11_kit_servers_for_provider(provider_name: &str, provider_path: &str) {
    logger(&format!("start_p11_kit_servers_for_provider: {}", provider_name));

    let mut try_counter = 0;
    let max_tries = 5;

    while try_counter < max_tries {
        let output = Command::new("/usr/bin/p11tool")
            .args(["--provider", provider_path, "--list-token-urls"])
            .output();

        match output {
            Ok(out) => {
                let stdout_err = format!("{}{}", String::from_utf8_lossy(&out.stdout), String::from_utf8_lossy(&out.stderr));

                if stdout_err.contains("PKCS #11 error") {
                    try_counter += 1;
                    thread::sleep(Duration::from_secs(5));
                    continue;
                }

                let mut token_num = 0;
                for word in stdout_err.split_whitespace() {
                    if !word.starts_with("pkcs11:") {
                        continue;
                    }

                    let clean_t = match word.find(";token=") {
                        Some(idx) => &word[..idx],
                        None => word,
                    };

                    let socket_name = format!("{}-slot-{}", provider_name, token_num);
                    let socket_path = format!("{}/{}", RUN_P11_KIT_DIR, socket_name);
                    let env_file_path = format!("{}-{}.env", P11_KIT_SERVER_ENV_BASE, socket_name);

                    logger(&format!("Starting p11-kit server --provider {} --name {}", provider_path, socket_path));

                    if let Ok(env_file) = File::create(&env_file_path) {
                        let _ = Command::new("/usr/bin/p11-kit")
                            .args(["server", "--provider", provider_path, "--name", &socket_path, clean_t])
                            .stdout(Stdio::from(env_file))
                            .stderr(Stdio::null())
                            .status();
                    } else {
                        logger(&format!("Failed to create env file: {}", env_file_path));
                    }

                    token_num += 1;
                }
                break;
            }
            Err(_) => {
                try_counter += 1;
                thread::sleep(Duration::from_secs(5));
            }
        }
    }

    if try_counter == max_tries {
        logger("Maximum tries reached. Exiting...");
    }
}

fn start_p11_kit_servers(providers: &[String]) {
    let _ = fs::create_dir_all(RUN_P11_KIT_DIR);
    stop_p11_kit_servers();

    let arch_triplet = match get_arch_triplet() {
        Some(arch) => arch,
        None => return,
    };

    let snap = env::var("SNAP").unwrap_or_default();
    let lib_dir = format!("{}/usr/lib/{}", snap, arch_triplet);

    for provider_pair in providers {
        let parts: Vec<&str> = provider_pair.splitn(2, ':').collect();

        if parts.len() == 2 {
            let name = parts[0].trim();
            let lib_name = parts[1].trim();

            let path = if lib_name.starts_with('/') {
                lib_name.to_string()
            } else {
                format!("{}/{}", lib_dir, lib_name)
            };

            logger(&format!("Handling provider: {}", name));
            if Path::new(&path).exists() {
                start_p11_kit_servers_for_provider(name, &path);
            } else {
                logger(&format!("Provider library not found: {}", path));
            }
        } else {
            logger(&format!("Invalid provider format skipped: {}", provider_pair));
        }
    }
}

fn print_help(bin_name: &str) {
    println!("p11-manager - A lightweight tool to manage p11-kit server instances.\n");
    println!("Usage: {} <COMMAND> [ARGS]", bin_name);
    println!("\nCommands:");
    println!("  start              Start p11-kit servers. Requires at least one --provider flag.");
    println!("                     Format: --provider \"name:lib.so\"");
    println!("                     Example: {} start --provider optee:libckteec.so --provider \"sc: opensc-pkcs11.so\"", bin_name);
    println!("  stop               Stop all running p11-kit servers.");
    println!("  help, -h, --help   Print this help message.");
}

fn main() {
    let args: Vec<String> = env::args().collect();
    let bin_name = args.first().map(|s| s.as_str()).unwrap_or("p11-manager");

    if args.len() < 2 {
        print_help(bin_name);
        logger("Missing arguments.");
        return;
    }

    match args[1].as_str() {
        "start" => {
            let mut providers = Vec::new();
            let mut iter = args.iter().skip(2); // Skip binary name and "start"

            while let Some(arg) = iter.next() {
                if arg == "--provider" {
                    if let Some(val) = iter.next() {
                        providers.push(val.clone());
                    } else {
                        println!("Error: Missing value for --provider\n");
                        print_help(bin_name);
                        logger("Missing value for --provider.");
                        return;
                    }
                } else {
                    println!("Error: Unknown argument '{}' for start command\n", arg);
                    print_help(bin_name);
                    logger(&format!("Unknown argument for start: {}", arg));
                    return;
                }
            }

            if providers.is_empty() {
                println!("Error: At least one --provider must be specified.\n");
                print_help(bin_name);
                logger("Missing providers string.");
                return;
            }

            start_p11_kit_servers(&providers);
        },
        "stop" => stop_p11_kit_servers(),
        "help" | "-h" | "--help" => {
            print_help(bin_name);
        },
        other => {
            println!("Error: Unknown command '{}'\n", other);
            print_help(bin_name);
            logger(&format!("Unknown option {}", other));
        },
    }
}
