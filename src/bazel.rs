#![forbid(unsafe_code)]
use crate::{config::Config, protocol::Budget};
use std::{
    ffi::{OsStr, OsString},
    path::{Path, PathBuf},
};

#[derive(Clone, Debug)]
pub struct Invocation {
    pub args: Vec<OsString>,
    pub startup: Vec<OsString>,
    pub command: Option<String>,
    pub managed: bool,
    pub run: bool,
    pub command_index: Option<usize>,
}

pub fn inspect(args: Vec<OsString>, cwd: &Path, config: &Config) -> Invocation {
    let commands = [
        "build",
        "test",
        "run",
        "coverage",
        "info",
        "query",
        "cquery",
        "aquery",
        "clean",
        "shutdown",
        "fetch",
        "sync",
        "vendor",
        "mod",
        "help",
        "version",
        "canonicalize-flags",
        "analyze-profile",
        "dump",
        "license",
        "mobile-install",
        "print_action",
    ];
    let mut index = None;
    let mut offset = 0;
    while offset < args.len() {
        let value = args[offset].to_string_lossy();
        if commands.contains(&value.as_ref()) {
            index = Some(offset);
            break;
        }
        if !value.starts_with('-') {
            break;
        }
        if !value.contains('=')
            && [
                "--output_base",
                "--output_user_root",
                "--bazelrc",
                "--host_jvm_args",
                "--server_javabase",
                "--max_idle_secs",
                "--connect_timeout_secs",
                "--digest_function",
                "--server_jvm_out",
                "--invocation_policy",
            ]
            .contains(&value.as_ref())
        {
            offset += 1;
        } else if !value.contains('=')
            && ![
                "--batch",
                "--nobatch",
                "--ignore_all_rc_files",
                "--workspace_rc",
                "--noworkspace_rc",
                "--home_rc",
                "--nohome_rc",
                "--system_rc",
                "--nosystem_rc",
                "--block_for_lock",
                "--noblock_for_lock",
                "--quiet",
                "--noquiet",
                "--client_debug",
                "--noclient_debug",
                "--batch_cpu_scheduling",
                "--nobatch_cpu_scheduling",
                "--shutdown_on_low_sys_mem",
                "--noshutdown_on_low_sys_mem",
                "--idle_server_tasks",
                "--noidle_server_tasks",
                "--write_command_log",
                "--nowrite_command_log",
                "--watchfs",
                "--nowatchfs",
                "--preemptible",
                "--nopreemptible",
                "--autodetect_server_javabase",
                "--noautodetect_server_javabase",
            ]
            .contains(&value.as_ref())
        {
            break;
        }
        offset += 1;
    }
    let command = index
        .map(|index| args[index].to_string_lossy().into_owned())
        .or_else(|| match args.as_slice() {
            [] => Some("help".into()),
            [flag] if flag == "--version" => Some("version".into()),
            _ => None,
        });
    let startup = args[..index.unwrap_or(args.len())].to_vec();
    let version = std::env::var("USE_BAZEL_VERSION").ok().or_else(|| {
        std::fs::read_to_string(crate::config::workspace(cwd).join(".bazelversion")).ok()
    });
    let supported = version
        .as_deref()
        .is_some_and(|version| matches!(version.trim(), "8.4.2" | "9.2.0"));
    let opaque = args
        .iter()
        .take_while(|arg| arg.as_os_str() != OsStr::new("--"))
        .any(|arg| {
            let value = arg.to_string_lossy();
            value.starts_with('@')
                || ["--invocation_policy", "--migrate", "--bisect", "--strict"]
                    .iter()
                    .any(|flag| value.starts_with(flag))
        });
    let resource_opaque = args
        .iter()
        .enumerate()
        .take_while(|(_, arg)| *arg != "--")
        .any(|(index, arg)| {
            let text = arg.to_string_lossy();
            if text.starts_with("-j") && text != "-j" && !text.starts_with("-j=") {
                return true;
            }
            if [
                "--local_resources",
                "--local_cpu_resources",
                "--local_ram_resources",
            ]
            .iter()
            .any(|flag| text.starts_with(flag))
            {
                return true;
            }
            for name in [
                "--jobs",
                "-j",
                "--worker_max_instances",
                "--worker_max_multiplex_instances",
                "--local_test_jobs",
            ] {
                let value = if text == name {
                    args.get(index + 1)
                        .map(|arg| arg.to_string_lossy().into_owned())
                } else {
                    text.strip_prefix(&format!("{name}=")).map(str::to_owned)
                };
                if let Some(value) = value
                    && value.parse::<u64>().ok().is_none_or(|value| value == 0)
                {
                    return true;
                }
            }
            false
        });
    let wrapper = crate::config::workspace(cwd).join("tools/bazel").exists();
    let managed = config.managed
        && supported
        && !opaque
        && !resource_opaque
        && !wrapper
        && matches!(
            command.as_deref(),
            Some("build" | "test" | "run" | "coverage")
        );
    let run = managed
        && command.as_deref() == Some("run")
        && !args.iter().any(|arg| {
            let value = arg.to_string_lossy();
            value.starts_with("--script_path") || value == "--norun" || value == "--run=false"
        });
    Invocation {
        args,
        startup,
        command,
        managed,
        run,
        command_index: index,
    }
}

fn numeric_limit(invocation: &Invocation, name: &str, cap: u64) -> u64 {
    let mut limit = cap;
    let start = invocation.command_index.map_or(0, |index| index + 1);
    let args = &invocation.args[start..];
    for (index, arg) in args.iter().enumerate() {
        if arg == "--" {
            break;
        }
        let text = arg.to_string_lossy();
        if name == "--jobs"
            && let Some(value) = text
                .strip_prefix("-j=")
                .and_then(|value| value.parse::<u64>().ok())
                .filter(|value| *value > 0)
        {
            limit = limit.min(value);
        }
        let value = if let Some(value) = text.strip_prefix(&format!("{name}=")) {
            Some(value.to_owned())
        } else if text == name || (name == "--jobs" && text == "-j") {
            args.get(index + 1)
                .map(|value| value.to_string_lossy().into_owned())
        } else {
            None
        };
        if let Some(value) = value
            .and_then(|value| value.parse::<u64>().ok())
            .filter(|value| *value > 0)
        {
            limit = limit.min(value);
        }
    }
    limit
}

pub fn arguments(
    invocation: &Invocation,
    budget: &Budget,
    config: &Config,
    script: Option<&Path>,
) -> Vec<OsString> {
    if !invocation.managed {
        return invocation.args.clone();
    }
    let jobs = numeric_limit(invocation, "--jobs", u64::from(config.jobs));
    let memory = config.action_memory_mib.min(budget.memory_mib * 2 / 3);
    let policy = format!(
        "flag_policies {{flag_name:'jobs' set_value {{flag_value:'{jobs}' behavior:FINAL_VALUE_IGNORE_OVERRIDES}}}} flag_policies {{flag_name:'local_resources' set_value {{flag_value:'cpu={cpu}' flag_value:'memory={memory}' behavior:APPEND}}}} flag_policies {{flag_name:'worker_max_instances' set_value {{flag_value:'={workers}' behavior:FINAL_VALUE_IGNORE_OVERRIDES}}}} flag_policies {{flag_name:'worker_max_multiplex_instances' set_value {{flag_value:'={workers}' behavior:FINAL_VALUE_IGNORE_OVERRIDES}}}} flag_policies {{flag_name:'local_test_jobs' commands:'test' set_value {{flag_value:'{tests}' behavior:FINAL_VALUE_IGNORE_OVERRIDES}}}}",
        cpu = budget.cpu,
        workers = numeric_limit(
            invocation,
            "--worker_max_instances",
            u64::from(config.worker_instances.min(budget.cpu))
        )
        .min(numeric_limit(
            invocation,
            "--worker_max_multiplex_instances",
            u64::from(config.worker_instances.min(budget.cpu))
        )),
        tests = Some(numeric_limit(invocation, "--local_test_jobs", u64::MAX))
            .filter(|tests| *tests != u64::MAX)
            .map_or(0, |tests| tests.min(u64::from(budget.cpu)))
    );
    let mut args = invocation.args.clone();
    if let Some(script) = script {
        let position = args
            .iter()
            .position(|arg| arg == "--")
            .unwrap_or(args.len());
        let mut value = OsString::from("--script_path=");
        value.push(script);
        args.insert(position, value);
    }
    args.insert(
        invocation.command_index.unwrap_or(0),
        OsString::from(format!("--invocation_policy={policy}")),
    );
    args
}

/// Where Bazel's Build Event Protocol JSON file goes for a managed invocation.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum EventFile {
    /// The caller already asked for one: read it, never redirect or delete it.
    Caller(PathBuf),
    /// bazelqueue adds its own flag and owns the file.
    Owned,
    /// A bazelrc may set its own file; adding ours would silently override it.
    Unavailable,
}

pub fn event_file(invocation: &Invocation, cwd: &Path, home: Option<&Path>) -> EventFile {
    if !invocation.managed {
        return EventFile::Unavailable;
    }
    let start = invocation.command_index.map_or(0, |index| index + 1);
    let args = &invocation.args[start..];
    for (index, arg) in args.iter().enumerate() {
        if arg == "--" {
            break;
        }
        let text = arg.to_string_lossy();
        let value = if text == "--build_event_json_file" {
            args.get(index + 1).map(PathBuf::from)
        } else {
            text.strip_prefix("--build_event_json_file=")
                .map(PathBuf::from)
        };
        if let Some(value) = value {
            return EventFile::Caller(cwd.join(value));
        }
    }
    let workspace = crate::config::workspace(cwd);
    let mut rc_files: Vec<PathBuf> = vec!["/etc/bazel.bazelrc".into(), workspace.join(".bazelrc")];
    if let Some(home) = home {
        rc_files.push(home.join(".bazelrc"));
    }
    for (index, arg) in invocation.startup.iter().enumerate() {
        let text = arg.to_string_lossy();
        if let Some(value) = text.strip_prefix("--bazelrc=") {
            rc_files.push(cwd.join(value));
        } else if text == "--bazelrc"
            && let Some(value) = invocation.startup.get(index + 1)
        {
            rc_files.push(cwd.join(value));
        }
    }
    if rc_files
        .iter()
        .any(|file| rc_sets_event_file(file, &workspace, 0))
    {
        EventFile::Unavailable
    } else {
        EventFile::Owned
    }
}

fn rc_sets_event_file(file: &Path, workspace: &Path, depth: usize) -> bool {
    let Ok(text) = std::fs::read_to_string(file) else {
        return false;
    };
    text.lines().any(|line| {
        let line = line.trim();
        if line.starts_with('#') {
            return false;
        }
        if line.contains("build_event_json_file") {
            return true;
        }
        let imported = line
            .strip_prefix("try-import ")
            .or_else(|| line.strip_prefix("import "));
        depth < 4
            && imported.is_some_and(|path| {
                let path = path
                    .trim()
                    .trim_matches('"')
                    .replace("%workspace%", &workspace.to_string_lossy());
                rc_sets_event_file(
                    &file.parent().unwrap_or(workspace).join(path),
                    workspace,
                    depth + 1,
                )
            })
    })
}

pub fn with_event_file(mut args: Vec<OsString>, file: &Path) -> Vec<OsString> {
    let position = args
        .iter()
        .position(|arg| arg == "--")
        .unwrap_or(args.len());
    let mut flag = OsString::from("--build_event_json_file=");
    flag.push(file);
    args.insert(position, flag);
    args
}

pub fn probe_arguments(invocation: &Invocation) -> Vec<OsString> {
    let mut args = invocation.startup.clone();
    args.push("--noblock_for_lock".into());
    args.extend([
        "info".into(),
        "output_base".into(),
        "server_pid".into(),
        "release".into(),
    ]);
    args
}

pub fn parse_info(stdout: &[u8]) -> Option<(PathBuf, u32)> {
    let text = std::str::from_utf8(stdout).ok()?;
    let mut base = None;
    let mut pid = None;
    for line in text.lines() {
        if let Some(value) = line.strip_prefix("output_base: ") {
            base = Some(PathBuf::from(value));
        }
        if let Some(value) = line.strip_prefix("server_pid: ") {
            pid = value.parse().ok();
        }
    }
    Some((base?, pid?))
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn event_files_are_added_only_where_no_caller_or_bazelrc_chose_one() {
        let root = tempfile::tempdir().unwrap();
        let workspace = root.path().join("w");
        let home = root.path().join("home");
        std::fs::create_dir_all(&workspace).unwrap();
        std::fs::create_dir_all(&home).unwrap();
        std::fs::write(workspace.join("MODULE.bazel"), "").unwrap();
        std::fs::write(workspace.join(".bazelversion"), "8.4.2").unwrap();
        let config = Config::default();
        let classify = |args: &[&str]| {
            let invocation = inspect(
                args.iter().map(OsString::from).collect(),
                &workspace,
                &config,
            );
            event_file(&invocation, &workspace, Some(&home))
        };
        assert_eq!(classify(&["test", "//:x"]), EventFile::Owned);
        assert_eq!(classify(&["info"]), EventFile::Unavailable);
        assert_eq!(
            classify(&["build", "--build_event_json_file=out/bep.json", "//:x"]),
            EventFile::Caller(workspace.join("out/bep.json"))
        );
        assert_eq!(
            classify(&["build", "--build_event_json_file", "/abs.json", "//:x"]),
            EventFile::Caller("/abs.json".into())
        );
        std::fs::write(
            workspace.join(".bazelrc"),
            "try-import %workspace%/ci.bazelrc\n",
        )
        .unwrap();
        assert_eq!(classify(&["test", "//:x"]), EventFile::Owned);
        std::fs::write(
            workspace.join("ci.bazelrc"),
            "build --build_event_json_file=/ci.json\n",
        )
        .unwrap();
        assert_eq!(classify(&["test", "//:x"]), EventFile::Unavailable);
        std::fs::remove_file(workspace.join("ci.bazelrc")).unwrap();
        std::fs::write(home.join(".bazelrc"), "# build --build_event_json_file=x\n").unwrap();
        assert_eq!(classify(&["test", "//:x"]), EventFile::Owned);
        assert_eq!(
            with_event_file(
                ["run", "//:x", "--", "--flag"].map(OsString::from).to_vec(),
                Path::new("/s/1.bep.json")
            ),
            [
                "run",
                "//:x",
                "--build_event_json_file=/s/1.bep.json",
                "--",
                "--flag"
            ]
            .map(OsString::from)
        );
    }
    #[test]
    fn startup_values_cannot_be_confused_with_commands() {
        let args = ["--output_base", "build", "test", "//:x", "--", "run"]
            .map(OsString::from)
            .to_vec();
        let invocation = inspect(args.clone(), Path::new("/tmp"), &Config::default());
        assert_eq!(invocation.command.as_deref(), Some("test"));
        assert_eq!(invocation.args, args);
        assert_eq!(invocation.startup.len(), 2);
    }
    #[test]
    fn client_only_version_and_help_are_named() {
        for (args, command) in [(vec![], "help"), (vec!["--version"], "version")] {
            let invocation = inspect(
                args.into_iter().map(OsString::from).collect(),
                Path::new("/tmp"),
                &Config::default(),
            );
            assert_eq!(invocation.command.as_deref(), Some(command));
            assert!(!invocation.managed);
        }
        let unknown = inspect(
            vec!["--unknown_startup".into(), "build".into()],
            Path::new("/tmp"),
            &Config::default(),
        );
        assert_eq!(unknown.command, None);
    }
    #[test]
    fn compatibility_arguments_are_unchanged() {
        let args = ["--batch", "run", "//:x", "--", "", "a b", "-z"]
            .map(OsString::from)
            .to_vec();
        let invocation = inspect(
            args.clone(),
            Path::new("/tmp"),
            &Config {
                managed: false,
                ..Config::default()
            },
        );
        assert_eq!(
            arguments(
                &invocation,
                &Config::default().budget(false),
                &Config::default(),
                None
            ),
            args
        );
    }
    fn policy(args: &[&str], budget: &Budget) -> String {
        let invocation = Invocation {
            args: args.iter().map(OsString::from).collect(),
            startup: Vec::new(),
            command: Some(args[0].into()),
            managed: true,
            run: false,
            command_index: Some(0),
        };
        arguments(&invocation, budget, &Config::default(), None)[0]
            .to_string_lossy()
            .into_owned()
    }
    #[test]
    fn cache_checks_run_beyond_the_reserved_cpus() {
        let budget = Budget {
            cpu: 2,
            ..Config::default().budget(true)
        };
        let policy = policy(&["test", "//:x"], &budget);
        assert!(policy.contains("flag_name:'jobs' set_value {flag_value:'64'"));
        assert!(policy.contains("flag_value:'cpu=2'"));
    }
    #[test]
    fn local_tests_are_limited_by_reserved_resources() {
        let budget = Budget {
            cpu: 8,
            ..Config::default().budget(true)
        };
        let tests = |args: &[&str]| {
            policy(args, &budget)
                .split("flag_name:'local_test_jobs' commands:'test' set_value {flag_value:'")
                .nth(1)
                .and_then(|rest| rest.split('\'').next())
                .map(str::to_owned)
        };
        assert_eq!(tests(&["test", "//:x"]).as_deref(), Some("0"));
        assert_eq!(
            tests(&["test", "--local_test_jobs=3", "//:x"]).as_deref(),
            Some("3")
        );
        assert_eq!(
            tests(&["test", "--local_test_jobs", "20", "//:x"]).as_deref(),
            Some("8")
        );
    }
    #[test]
    fn stricter_direct_limits_are_kept() {
        let invocation = Invocation {
            args: vec!["build".into(), "--jobs=1".into(), "//:x".into()],
            startup: vec![],
            command: Some("build".into()),
            managed: true,
            run: false,
            command_index: Some(0),
        };
        let result = arguments(
            &invocation,
            &Config::default().budget(true),
            &Config::default(),
            None,
        );
        assert!(result[0].to_string_lossy().contains("flag_value:'1'"));
    }
    #[test]
    fn job_abbreviations_are_clamped_and_opaque_mnemonics_preserved() {
        let invocation = Invocation {
            args: vec!["build".into(), "-j=1".into(), "//:x".into()],
            startup: Vec::new(),
            command: Some("build".into()),
            managed: true,
            run: false,
            command_index: Some(0),
        };
        assert!(
            arguments(
                &invocation,
                &Config::default().budget(true),
                &Config::default(),
                None
            )[0]
            .to_string_lossy()
            .contains("flag_value:'1'")
        );
        let root = tempfile::tempdir().unwrap();
        std::fs::write(root.path().join(".bazelversion"), "8.4.2").unwrap();
        for args in [
            vec!["build", "--worker_max_instances", "CppCompile=1", "//:x"],
            vec!["build", "--worker_max_instances=0", "//:x"],
            vec!["build", "--jobs=HOST_CPUS*.1", "//:x"],
            vec!["build", "-j1", "//:x"],
        ] {
            let args = args.into_iter().map(OsString::from).collect::<Vec<_>>();
            let classified = inspect(args.clone(), root.path(), &Config::default());
            assert!(!classified.managed);
            assert_eq!(
                arguments(
                    &classified,
                    &Config::default().budget(false),
                    &Config::default(),
                    None
                ),
                args
            );
        }
    }
}
