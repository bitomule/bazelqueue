use std::{
    env, fs,
    io::{Read, Write},
    os::unix::{ffi::OsStrExt, net::UnixStream},
    path::Path,
    process,
};

/// Stands in for Bazel: `info` names a server, an up-to-date check exits with the
/// code in `check`, and every check or run is appended to `log`.
fn bazel(state: &Path, arguments: &[String]) {
    let record = |line: &str| {
        let mut log = fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(state.join("log"))
            .unwrap();
        writeln!(log, "{line}").unwrap();
    };
    if arguments.iter().any(|arg| arg == "info") {
        println!("output_base: {}", state.join("output_base").display());
        println!(
            "server_pid: {}",
            fs::read_to_string(state.join("server_pid")).unwrap().trim()
        );
        println!("release: release 8.4.2");
        return;
    }
    if arguments
        .iter()
        .any(|arg| arg == "--check_up_to_date" || arg == "--check_tests_up_to_date")
    {
        record("check");
        process::exit(
            fs::read_to_string(state.join("check")).map_or(1, |code| code.trim().parse().unwrap()),
        );
    }
    record("run");
    if let Ok(socket) = fs::read_to_string(state.join("hold")) {
        let mut connection = UnixStream::connect(socket.trim()).unwrap();
        connection.write_all(b"R").unwrap();
        let mut release = [0; 1];
        connection.read_exact(&mut release).unwrap();
    }
}

fn main() {
    let arguments: Vec<_> = env::args_os().skip(1).collect();
    if let Some(state) = env::var_os("BAZELQUEUE_FIXTURE_BAZEL") {
        let arguments: Vec<_> = arguments
            .iter()
            .map(|arg| arg.to_string_lossy().into_owned())
            .collect();
        return bazel(Path::new(&state), &arguments);
    }
    match arguments.first().and_then(|arg| arg.to_str()) {
        Some("tree-hold") => {
            let status = process::Command::new(env::current_exe().unwrap())
                .arg("memory-hold")
                .arg(&arguments[1])
                .status()
                .unwrap();
            process::exit(status.code().unwrap_or(1));
        }
        Some("memory-hold") => {
            let allocation = vec![1_u8; 128 * 1024 * 1024];
            let mut connection = UnixStream::connect(&arguments[1]).unwrap();
            connection.write_all(b"R").unwrap();
            let mut release = [0; 1];
            connection.read_exact(&mut release).unwrap();
            std::hint::black_box(allocation);
        }
        Some("hold") => {
            let mut connection = UnixStream::connect(&arguments[1]).unwrap();
            connection.write_all(b"R").unwrap();
            let mut release = [0; 1];
            connection.read_exact(&mut release).unwrap();
            process::exit(
                arguments
                    .get(2)
                    .and_then(|arg| arg.to_str())
                    .unwrap_or("0")
                    .parse()
                    .unwrap(),
            );
        }
        Some("echo") => {
            let mut input = Vec::new();
            std::io::stdin().read_to_end(&mut input).unwrap();
            std::io::stdout().write_all(&input).unwrap();
        }
        Some("argv") => println!(
            "{}",
            serde_json::to_string(
                &arguments[1..]
                    .iter()
                    .map(|arg| arg.as_bytes())
                    .collect::<Vec<_>>()
            )
            .unwrap()
        ),
        Some("exit") => process::exit(arguments[1].to_str().unwrap().parse().unwrap()),
        Some("identity") => println!("{}", process::id()),
        Some("tty") => {
            use std::io::{BufRead, IsTerminal};
            println!("tty:{}", std::io::stdin().is_terminal());
            std::io::stdout().flush().unwrap();
            let mut line = String::new();
            std::io::stdin().lock().read_line(&mut line).unwrap();
            println!("echo:{}", line.trim());
            process::exit(0);
        }
        _ => process::exit(2),
    }
}
