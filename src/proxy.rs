use std::process::Command;
use std::sync::{OnceLock, RwLock};

use serde::{Deserialize, Serialize};

use crate::diagnose;

pub const DEFAULT_URL: &str = "http://127.0.0.1:7897";
const PROXY_ENV: [&str; 6] = [
    "HTTPS_PROXY",
    "HTTP_PROXY",
    "ALL_PROXY",
    "https_proxy",
    "http_proxy",
    "all_proxy",
];

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Deserialize, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum ProxyMode {
    #[default]
    Environment,
    Custom,
    Direct,
}

#[derive(Clone, PartialEq, Eq, Deserialize, Serialize)]
#[serde(default)]
pub struct ProxySettings {
    pub mode: ProxyMode,
    pub url: String,
}

impl std::fmt::Debug for ProxySettings {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("ProxySettings")
            .field("mode", &self.mode)
            .field("url", &"<redacted>")
            .finish()
    }
}

impl Default for ProxySettings {
    fn default() -> Self {
        Self {
            mode: ProxyMode::Environment,
            url: DEFAULT_URL.to_owned(),
        }
    }
}

impl ProxySettings {
    pub fn validated(mut self) -> Result<Self, &'static str> {
        self.url = self.url.trim().to_owned();
        if self.mode != ProxyMode::Custom {
            return Ok(self);
        }
        if !self.url.contains("://") {
            self.url = format!("http://{}", self.url);
        }
        let authority = self
            .url
            .strip_prefix("http://")
            .ok_or("Use an HTTP proxy address, such as http://127.0.0.1:7897.")?
            .trim_end_matches('/');
        if authority.is_empty()
            || authority.chars().any(char::is_whitespace)
            || authority.chars().any(char::is_control)
            || authority.contains(['/', '?', '#'])
        {
            return Err("Invalid HTTP proxy address.");
        }
        let server = authority.rsplit('@').next().unwrap_or_default();
        let (host, port) = server.split_once(':').unwrap_or((server, "80"));
        if host.is_empty()
            || !host
                .chars()
                .all(|c| c.is_ascii_alphanumeric() || matches!(c, '.' | '-' | '_'))
            || port.parse::<u16>().ok().filter(|port| *port > 0).is_none()
        {
            return Err("Invalid proxy host or port.");
        }
        ureq::Proxy::new(&self.url).map_err(|_| "Invalid HTTP proxy address.")?;
        self.url = self.url.trim_end_matches('/').to_owned();
        Ok(self)
    }

    fn agent_builder(&self) -> Result<ureq::AgentBuilder, &'static str> {
        let builder = ureq::AgentBuilder::new();
        match self.mode {
            ProxyMode::Environment => Ok(builder.try_proxy_from_env(true)),
            ProxyMode::Direct => Ok(builder.try_proxy_from_env(false)),
            ProxyMode::Custom => {
                let settings = self.clone().validated()?;
                let proxy = ureq::Proxy::new(&settings.url)
                    .map_err(|_| "Unable to configure the HTTP proxy.")?;
                Ok(builder.try_proxy_from_env(false).proxy(proxy))
            }
        }
    }

    fn apply_to_command(&self, command: &mut Command) -> Result<(), &'static str> {
        match self.mode {
            ProxyMode::Environment => {}
            ProxyMode::Direct => {
                for name in PROXY_ENV {
                    command.env_remove(name);
                }
            }
            ProxyMode::Custom => {
                // Command-local values also cover CLI token refreshes, without
                // mutating this multithreaded process's environment.
                let settings = self.clone().validated()?;
                for name in PROXY_ENV {
                    command.env(name, &settings.url);
                }
            }
        }
        Ok(())
    }
}

fn runtime() -> &'static RwLock<ProxySettings> {
    static SETTINGS: OnceLock<RwLock<ProxySettings>> = OnceLock::new();
    SETTINGS.get_or_init(|| RwLock::new(ProxySettings::default()))
}

fn current() -> ProxySettings {
    runtime().read().unwrap_or_else(|e| e.into_inner()).clone()
}

pub fn configure(settings: ProxySettings) {
    // Do not put URLs in logs: an address may contain proxy credentials.
    diagnose::log(format!("network: proxy mode={:?}", settings.mode));
    *runtime().write().unwrap_or_else(|e| e.into_inner()) = settings;
}

pub fn agent_builder() -> Result<ureq::AgentBuilder, &'static str> {
    current().agent_builder()
}

pub fn apply_to_command(command: &mut Command) -> Result<(), &'static str> {
    current().apply_to_command(command)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::{Read, Write};
    use std::net::TcpListener;
    use std::time::{Duration, Instant};

    fn custom(url: &str) -> ProxySettings {
        ProxySettings {
            mode: ProxyMode::Custom,
            url: url.to_owned(),
        }
    }

    #[test]
    fn validates_and_normalizes_http_proxy_addresses() {
        assert_eq!(
            custom(" 127.0.0.1:7897 ").validated().unwrap().url,
            DEFAULT_URL
        );
        assert_eq!(
            custom("http://localhost:7897/").validated().unwrap().url,
            "http://localhost:7897"
        );
        for invalid in [
            "",
            "http://",
            "http://:7897",
            "http://localhost:0",
            "http://localhost:65536",
            "http://localhost:nope",
            "http://localhost/path",
            "http://localhost?x=y",
            "socks5://localhost:7897",
            "https://localhost:7897",
            "http://local host:7897",
        ] {
            assert!(custom(invalid).validated().is_err(), "accepted {invalid}");
        }
    }

    #[test]
    fn defaults_preserve_environment_mode_and_custom_settings_round_trip() {
        let defaults: ProxySettings = serde_json::from_str("{}").unwrap();
        assert_eq!(defaults.mode, ProxyMode::Environment);
        let configured = custom(DEFAULT_URL);
        let restored: ProxySettings =
            serde_json::from_str(&serde_json::to_string(&configured).unwrap()).unwrap();
        assert!(restored == configured);
    }

    #[test]
    fn token_refresh_commands_receive_the_selected_proxy_mode() {
        let mut command = Command::new("cmd.exe");
        custom(DEFAULT_URL).apply_to_command(&mut command).unwrap();
        for key in ["HTTP_PROXY", "HTTPS_PROXY", "ALL_PROXY"] {
            let value = command
                .get_envs()
                .find(|(name, _)| name.to_string_lossy().eq_ignore_ascii_case(key))
                .unwrap()
                .1
                .unwrap();
            assert_eq!(value, DEFAULT_URL);
        }
        let direct = ProxySettings {
            mode: ProxyMode::Direct,
            ..Default::default()
        };
        direct.apply_to_command(&mut command).unwrap();
        assert!(command.get_envs().all(|(_, value)| value.is_none()));
        let mut inherited = Command::new("cmd.exe");
        ProxySettings::default()
            .apply_to_command(&mut inherited)
            .unwrap();
        assert_eq!(inherited.get_envs().count(), 0);
        assert!(custom("http://localhost:bad")
            .apply_to_command(&mut inherited)
            .is_err());
    }

    fn mock_server(response: &'static str) -> (String, std::thread::JoinHandle<String>) {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let address = listener.local_addr().unwrap().to_string();
        listener.set_nonblocking(true).unwrap();
        let server = std::thread::spawn(move || {
            let deadline = Instant::now() + Duration::from_secs(8);
            let (mut stream, _) = loop {
                match listener.accept() {
                    Ok(connection) => break connection,
                    Err(error)
                        if error.kind() == std::io::ErrorKind::WouldBlock
                            && Instant::now() < deadline =>
                    {
                        std::thread::sleep(Duration::from_millis(10))
                    }
                    Err(error) => panic!("mock server received no connection: {error}"),
                }
            };
            stream
                .set_read_timeout(Some(Duration::from_secs(3)))
                .unwrap();
            let mut request = Vec::new();
            let mut chunk = [0; 1024];
            while !request.ends_with(b"\r\n\r\n") && request.len() < 8192 {
                let count = stream.read(&mut chunk).unwrap();
                if count == 0 {
                    break;
                }
                request.extend_from_slice(&chunk[..count]);
            }
            stream.write_all(response.as_bytes()).unwrap();
            String::from_utf8(request).unwrap()
        });
        (address, server)
    }

    #[test]
    fn custom_proxy_routes_https_through_a_connect_tunnel() {
        let (address, server) = mock_server(
            "HTTP/1.1 502 Bad Gateway\r\nContent-Length: 0\r\nConnection: close\r\n\r\n",
        );
        let agent = custom(&format!("http://{address}"))
            .agent_builder()
            .unwrap()
            .timeout(Duration::from_secs(3))
            .build();
        assert!(agent
            .get("https://proxy-test.invalid/usage")
            .call()
            .is_err());
        assert!(server
            .join()
            .unwrap()
            .starts_with("CONNECT proxy-test.invalid:443 HTTP/1.1"));
    }

    #[test]
    fn direct_and_environment_modes_route_requests_in_isolated_processes() {
        // Each child receives its own environment so parallel tests and the
        // user's proxy variables are not modified.
        for mode in ["direct", "environment"] {
            let response = if mode == "direct" {
                "HTTP/1.1 200 OK\r\nContent-Length: 2\r\nConnection: close\r\n\r\nok"
            } else {
                "HTTP/1.1 502 Bad Gateway\r\nContent-Length: 0\r\nConnection: close\r\n\r\n"
            };
            let (address, server) = mock_server(response);
            let mut child = Command::new(std::env::current_exe().unwrap());
            child
                .args([
                    "--exact",
                    "proxy::tests::routing_subprocess_probe",
                    "--nocapture",
                ])
                .env("CODEX_PROXY_TEST_MODE", mode)
                .env("CODEX_PROXY_TEST_ADDRESS", &address);
            let env_proxy = if mode == "direct" {
                "http://127.0.0.1:1".to_owned()
            } else {
                format!("http://{address}")
            };
            for name in PROXY_ENV {
                child.env(name, &env_proxy);
            }
            let output = child.output().unwrap();
            let request = server.join().unwrap();
            assert!(
                output.status.success(),
                "{}{}",
                String::from_utf8_lossy(&output.stdout),
                String::from_utf8_lossy(&output.stderr)
            );
            assert!(request.starts_with(if mode == "direct" {
                "GET /usage HTTP/1.1"
            } else {
                "CONNECT proxy-test.invalid:443 HTTP/1.1"
            }));
        }
    }

    #[test]
    fn routing_subprocess_probe() {
        let Ok(mode) = std::env::var("CODEX_PROXY_TEST_MODE") else {
            return;
        };
        let address = std::env::var("CODEX_PROXY_TEST_ADDRESS").unwrap();
        let settings = ProxySettings {
            mode: if mode == "direct" {
                ProxyMode::Direct
            } else {
                ProxyMode::Environment
            },
            ..Default::default()
        };
        let agent = settings
            .agent_builder()
            .unwrap()
            .timeout(Duration::from_secs(3))
            .build();
        if mode == "direct" {
            assert_eq!(
                agent
                    .get(&format!("http://{address}/usage"))
                    .call()
                    .unwrap()
                    .status(),
                200
            );
        } else {
            assert!(agent
                .get("https://proxy-test.invalid/usage")
                .call()
                .is_err());
        }
    }
}
