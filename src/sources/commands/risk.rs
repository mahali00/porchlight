use super::template::{Word, has_input, inputs_of, word_text};
use crate::config::Input;
use crate::policy::Risk;
use indexmap::IndexMap;
use std::path::Path;

pub(super) fn runs_anything(name: &str) -> bool {
    const INTERPRETERS: [&str; 21] = [
        "sh",
        "bash",
        "zsh",
        "fish",
        "csh",
        "ksh",
        "tcsh",
        "dash",
        "node",
        "bun",
        "deno",
        "ruby",
        "perl",
        "php",
        "lua",
        "awk",
        "gawk",
        "mawk",
        "nawk",
        "osascript",
        "pwsh",
    ];

    INTERPRETERS.contains(&name)
        || matches!(name, "powershell" | "python" | "ssh" | "eval")
        || runs_a_program(name)
        || name.strip_prefix("python").is_some_and(|version| {
            !version.is_empty() && version.chars().all(|character| character.is_ascii_digit() || character == '.')
        })
}

pub(super) fn runs_a_program(name: &str) -> bool {
    const DISPATCHERS: [&str; 27] = [
        "env",
        "nice",
        "timeout",
        "xargs",
        "sudo",
        "doas",
        "su",
        "runuser",
        "nohup",
        "stdbuf",
        "setsid",
        "chroot",
        "command",
        "exec",
        "time",
        "ionice",
        "taskset",
        "chrt",
        "unbuffer",
        "flock",
        "script",
        "watch",
        "strace",
        "ltrace",
        "unshare",
        "nsenter",
        "systemd-run",
    ];

    DISPATCHERS.contains(&name)
}

pub(super) fn executable(words: &[Word]) -> Option<(String, &[Word])> {
    let program = words.first()?;
    let text = word_text(program)?;
    let name = Path::new(&text).file_name()?.to_str()?.to_owned();

    Some((name, words.get(1..)?))
}

pub fn runs_client_code(words: &[Word]) -> bool {
    if words.first().is_some_and(has_input) {
        return true;
    }

    let Some((program, arguments)) = executable(words) else {
        return false;
    };
    let input_reaches = words.iter().any(has_input);

    if matches!(program.as_str(), "busybox" | "toybox") {
        return runs_client_code(arguments);
    }

    if runs_a_program(&program) {
        return input_reaches;
    }

    runs_anything(&program) && input_reaches
}

pub(super) fn destroys_with(program: &str, arguments: &[Word]) -> Option<&'static str> {
    let has = |flag: &str| arguments.iter().any(|word| word_text(word).is_some_and(|text| text == flag));

    match program {
        "rm" | "rmdir" | "unlink" | "shred" | "srm" => Some("deletes files"),
        "dd" => Some("overwrites files and disks"),
        "truncate" => Some("empties files"),
        "mv" => Some("moves and overwrites files"),
        "chmod" | "chown" | "chgrp" => Some("changes who can use files"),
        "kill" | "pkill" | "killall" => Some("stops programs"),
        "shutdown" | "reboot" | "halt" | "poweroff" => Some("shuts this computer down"),
        "wipefs" | "fdisk" | "sfdisk" | "gdisk" | "parted" | "mke2fs" | "diskutil" => Some("erases disks"),
        name if name.starts_with("mkfs") => Some("erases disks"),
        "find" if has("-delete") => Some("deletes files"),
        "rsync" if has("--delete") => Some("deletes files"),
        "git"
            if has("clean")
                || has("--hard")
                || has("-D")
                || (has("push") && (has("--force") || has("-f") || has("--force-with-lease"))) =>
        {
            Some("throws away work")
        }
        "docker" | "podman" if has("rm") || has("rmi") || has("prune") || has("kill") => {
            Some("removes containers and images")
        }
        "kubectl" if has("delete") => Some("deletes cluster resources"),
        _ => None,
    }
}

pub(super) fn destroys(words: &[Word]) -> Option<&'static str> {
    let (program, arguments) = executable(words)?;

    if runs_a_program(&program) || matches!(program.as_str(), "busybox" | "toybox") {
        let next = arguments.iter().position(|word| {
            word_text(word).is_some_and(|text| {
                !text.starts_with('-') && !text.contains('=') && !text.starts_with(|first: char| first.is_ascii_digit())
            })
        })?;
        return destroys(arguments.get(next..)?);
    }

    destroys_with(&program, arguments)
}

pub fn assess(words: &[Word], inputs: &IndexMap<String, Input>, claimed_read_only: bool) -> Risk {
    Risk {
        runs_code: runs_client_code(words),
        claimed_read_only,
        effect: destroys(words),
        open: inputs_of(words).into_iter().filter(|name| inputs.get(name).is_none_or(Input::is_open)).collect(),
    }
}
