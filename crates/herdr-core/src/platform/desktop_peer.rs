//! Linux desktop viewers use a Unix socket directly or through `hc connect`.
//! SSH/phone bridges also use the socket, so locality alone is not identity.

use super::AsyncRead;

pub(super) fn is_desktop(read: &AsyncRead) -> bool {
    let Ok(cred) = read.as_ref().peer_cred() else { return false };
    if cred.uid() != unsafe { libc::geteuid() } {
        return false;
    }
    cred.pid().is_some_and(|pid| desktop_process(pid as u32))
}

fn desktop_process(pid: u32) -> bool {
    let exe = |pid| std::fs::read_link(format!("/proc/{pid}/exe")).ok();
    let Some(path) = exe(pid) else { return false };
    if path.file_name().is_some_and(|n| n == "herdr-app") {
        return true;
    }
    if !path.file_name().is_some_and(|n| n == "herdr-core" || n == "hc") {
        return false;
    }
    // Only the immediate parent: an hc launched in a terminal inside Herdr
    // must not be mistaken for the app's own bridge.
    let parent = std::fs::read_to_string(format!("/proc/{pid}/stat"))
        .ok()
        .and_then(|s| s.rsplit_once(')').and_then(|(_, fields)| fields.split_whitespace().nth(1)?.parse::<u32>().ok()));
    parent.and_then(exe).as_deref().and_then(std::path::Path::file_name).is_some_and(|n| n == "herdr-app")
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::{BufRead, BufReader};
    use std::path::PathBuf;
    use std::process::{Command, Stdio};

    #[test]
    fn desktop_identity_uses_the_executable_and_only_one_parent_hop() {
        assert!(!desktop_process(std::process::id()));
        let root = PathBuf::from(format!("/var/tmp/hc-peer-{}", crate::platform::random_hex()));
        std::fs::create_dir(&root).unwrap();
        let app = root.join("herdr-app");
        std::fs::copy("/bin/sh", &app).unwrap();
        let helper = root.join("herdr-core");
        let sleep = std::env::split_paths(&std::env::var_os("PATH").unwrap())
            .map(|dir| dir.join("sleep"))
            .find(|path| path.is_file())
            .unwrap();
        std::fs::copy(sleep, &helper).unwrap();
        let mut child = Command::new(&app)
            .args(["-c", "\"$1\" 100 & echo $!; wait", "app"])
            .arg(&helper)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .spawn()
            .unwrap();
        let mut line = String::new();
        BufReader::new(child.stdout.take().unwrap()).read_line(&mut line).unwrap();
        let helper_pid: u32 = line.trim().parse().unwrap();
        assert!(desktop_process(child.id()));
        assert!(desktop_process(helper_pid), "the app's connect helper belongs to the desktop too");
        unsafe {
            libc::kill(helper_pid as i32, libc::SIGKILL);
        }
        child.kill().unwrap();
        child.wait().unwrap();
        assert!(!desktop_process(child.id()));
        let mut other = Command::new(&helper).arg("100").stdin(Stdio::piped()).spawn().unwrap();
        assert!(!desktop_process(other.id()), "an unrelated hc is not a desktop viewer");
        other.kill().unwrap();
        other.wait().unwrap();
        std::fs::remove_dir_all(root).unwrap();
    }
}
