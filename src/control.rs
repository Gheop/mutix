//! A datagram socket so a desktop shortcut can drive a running `mutix listen`:
//! `mutix toggle` starts or ends a capture, `mutix calibrate` re-measures the
//! still noise.

use std::os::unix::net::UnixDatagram;
use std::path::PathBuf;

use anyhow::{Context, Result};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Command {
    Toggle,
    Start,
    Stop,
    Calibrate,
    Auto,
    Manual,
}

impl Command {
    pub fn as_str(self) -> &'static str {
        match self {
            Command::Toggle => "toggle",
            Command::Start => "start",
            Command::Stop => "stop",
            Command::Calibrate => "calibrate",
            Command::Auto => "auto",
            Command::Manual => "manual",
        }
    }

    pub fn parse(s: &str) -> Option<Command> {
        [
            Command::Toggle,
            Command::Start,
            Command::Stop,
            Command::Calibrate,
            Command::Auto,
            Command::Manual,
        ]
        .into_iter()
        .find(|c| c.as_str() == s.trim())
    }
}

pub fn socket_path() -> PathBuf {
    dirs::runtime_dir()
        .unwrap_or_else(std::env::temp_dir)
        .join("mutix.sock")
}

pub struct Server {
    sock: UnixDatagram,
    path: PathBuf,
}

impl Server {
    pub fn bind() -> Result<Server> {
        let path = socket_path();
        // A socket file left by a crashed run would block bind().
        if UnixDatagram::unbound()?.connect(&path).is_err() {
            let _ = std::fs::remove_file(&path);
        }
        let sock = UnixDatagram::bind(&path)
            .with_context(|| format!("cannot listen on {}; is mutix already running?", path.display()))?;
        sock.set_nonblocking(true)?;
        Ok(Server { sock, path })
    }

    pub fn poll(&self) -> Option<Command> {
        let mut buf = [0u8; 64];
        let n = self.sock.recv(&mut buf).ok()?;
        Command::parse(std::str::from_utf8(&buf[..n]).ok()?)
    }
}

impl Drop for Server {
    fn drop(&mut self) {
        let _ = std::fs::remove_file(&self.path);
    }
}

pub fn send(cmd: Command) -> Result<()> {
    let path = socket_path();
    let sock = UnixDatagram::unbound()?;
    sock.send_to(cmd.as_str().as_bytes(), &path)
        .with_context(|| format!("no mutix listening on {}; start `mutix listen` first", path.display()))?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn round_trip() {
        for c in [Command::Toggle, Command::Calibrate, Command::Manual] {
            assert_eq!(Command::parse(c.as_str()), Some(c));
        }
        assert_eq!(Command::parse("toggle\n"), Some(Command::Toggle));
        assert_eq!(Command::parse("dance"), None);
    }
}
