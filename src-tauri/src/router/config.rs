use std::collections::HashSet;

#[derive(Debug, Clone)]
pub enum RedirectMode {
    List(HashSet<String>),
    All,
}

#[derive(Debug, Clone)]
pub struct SocksTarget {
    pub host: String,
    pub port: u16,
    pub username: String,
    pub password: String,
}

#[derive(Debug, Clone)]
pub struct RouterConfig {
    pub mode: RedirectMode,
    pub upstream: Option<SocksTarget>,
}

impl RouterConfig {
    pub fn should_proxy(&self, host: &str) -> bool {
        match &self.mode {
            RedirectMode::All => true,
            RedirectMode::List(hosts) => hosts.contains(host),
        }
    }
}
