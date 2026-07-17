use keyring::Entry;

const SERVICE: &str = "freetubemusic";
const USER: &str = "socks-proxy";

fn entry() -> Result<Entry, String> {
    Entry::new(SERVICE, USER).map_err(|e| e.to_string())
}

pub fn get_password() -> Option<String> {
    entry().ok()?.get_password().ok()
}

pub fn set_password(password: &str) -> Result<(), String> {
    entry()?.set_password(password).map_err(|e| e.to_string())
}
