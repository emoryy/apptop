use std::collections::HashMap;
use std::io::{Read, Write};
use std::os::unix::net::UnixStream;
use std::time::Duration;

#[derive(Clone, Debug)]
pub struct Container {
    pub name: String,
    pub project: Option<String>,
}

/// Container id -> name and compose project, from the docker socket.
#[derive(Default)]
pub struct DockerNames {
    map: HashMap<String, Container>,
    unavailable: bool,
}

impl DockerNames {
    /// Re-query only when a container id shows up that we have not seen yet.
    pub fn ensure(&mut self, ids: &[&str]) {
        if self.unavailable || ids.iter().all(|id| self.map.contains_key(*id)) {
            return;
        }
        match query() {
            Some(m) => self.map = m,
            None => self.unavailable = true,
        }
    }

    pub fn get(&self, id: &str) -> Option<&Container> {
        self.map.get(id)
    }
}

/// DOCKER_HOST when it names a unix socket, else the default socket.
fn socket_path() -> String {
    std::env::var("DOCKER_HOST")
        .ok()
        .and_then(|h| h.strip_prefix("unix://").map(String::from))
        .unwrap_or_else(|| "/var/run/docker.sock".into())
}

fn query() -> Option<HashMap<String, Container>> {
    let mut s = UnixStream::connect(socket_path()).ok()?;
    s.set_read_timeout(Some(Duration::from_secs(2))).ok()?;
    // HTTP/1.0 so the daemon closes the connection instead of chunking the body
    s.write_all(b"GET /containers/json HTTP/1.0\r\nHost: docker\r\n\r\n")
        .ok()?;
    let mut buf = Vec::new();
    s.read_to_end(&mut buf).ok()?;
    let text = String::from_utf8_lossy(&buf);
    let body = text.split_once("\r\n\r\n")?.1;
    let v: serde_json::Value = serde_json::from_str(body).ok()?;
    let mut out = HashMap::new();
    for c in v.as_array()? {
        let Some(id) = c["Id"].as_str() else { continue };
        let name = c["Names"][0].as_str().unwrap_or(id).trim_start_matches('/').to_string();
        let project = c["Labels"]["com.docker.compose.project"].as_str().map(String::from);
        out.insert(id.to_string(), Container { name, project });
    }
    Some(out)
}
