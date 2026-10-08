use std::{
    env,
    io::{Read, Write},
    os::unix::{ffi::OsStrExt, net::UnixStream},
    process,
};

fn main() {
    let arguments: Vec<_> = env::args_os().skip(1).collect();
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
