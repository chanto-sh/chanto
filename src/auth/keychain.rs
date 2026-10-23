//! Optional OS-keychain storage for provider credentials.
//!
//! By default credentials live in `auth.json` (mode 0600). With
//! `CHANTO_KEYCHAIN=1` they live in the login keychain instead and the file
//! keeps only a placeholder, so a copy of `~/.chanto` leaks nothing. A
//! placeholder is always resolved from the keychain on read, so toggling the
//! setting never loses a saved login.

#[cfg(test)]
use std::collections::BTreeMap;

/// The keychain service every entry is filed under.
#[cfg(all(not(test), target_os = "macos"))]
const SERVICE: &str = "chanto";

/// Whether new credentials should be written to the keychain (`CHANTO_KEYCHAIN`).
pub fn enabled() -> bool {
    matches!(
        std::env::var("CHANTO_KEYCHAIN").ok().as_deref(),
        Some("1") | Some("true") | Some("yes")
    )
}

/// Whether this build can read and write a keychain at all.
pub fn available() -> bool {
    cfg!(test) || cfg!(target_os = "macos")
}

trait Backend: Sync {
    fn load(&self, account: &str) -> Option<Vec<u8>>;
    fn save(&self, account: &str, secret: &[u8]) -> Result<(), String>;
    fn delete(&self, account: &str) -> Result<(), String>;
}

fn backend() -> &'static dyn Backend {
    #[cfg(test)]
    {
        &TestBackend
    }
    #[cfg(all(not(test), target_os = "macos"))]
    {
        static B: Mac = Mac;
        &B
    }
    #[cfg(all(not(test), not(target_os = "macos")))]
    {
        static B: NoBackend = NoBackend;
        &B
    }
}

/// The credential JSON stored for `account`, if any.
pub fn load(account: &str) -> Option<Vec<u8>> {
    backend().load(account)
}

/// Store the credential JSON for `account`.
pub fn save(account: &str, secret: &[u8]) -> Result<(), String> {
    backend().save(account, secret)
}

/// Forget `account`. Missing entries are not an error.
pub fn delete(account: &str) -> Result<(), String> {
    backend().delete(account)
}

#[cfg(all(not(test), not(target_os = "macos")))]
struct NoBackend;

#[cfg(all(not(test), not(target_os = "macos")))]
impl Backend for NoBackend {
    fn load(&self, _: &str) -> Option<Vec<u8>> {
        None
    }
    fn save(&self, _: &str, _: &[u8]) -> Result<(), String> {
        Err("no keychain on this platform".into())
    }
    fn delete(&self, _: &str) -> Result<(), String> {
        Ok(())
    }
}

#[cfg(all(not(test), target_os = "macos"))]
struct Mac;

#[cfg(all(not(test), target_os = "macos"))]
impl Backend for Mac {
    fn load(&self, account: &str) -> Option<Vec<u8>> {
        security_framework::passwords::get_generic_password(SERVICE, account).ok()
    }
    fn save(&self, account: &str, secret: &[u8]) -> Result<(), String> {
        security_framework::passwords::set_generic_password(SERVICE, account, secret)
            .map_err(|e| e.to_string())
    }
    fn delete(&self, account: &str) -> Result<(), String> {
        match security_framework::passwords::delete_generic_password(SERVICE, account) {
            Ok(()) => Ok(()),
            // errSecItemNotFound: deleting something that is already gone is fine.
            Err(e) if e.code() == -25300 => Ok(()),
            Err(e) => Err(e.to_string()),
        }
    }
}

#[cfg(test)]
struct TestBackend;

#[cfg(test)]
static MEM: std::sync::Mutex<Option<BTreeMap<String, Vec<u8>>>> = std::sync::Mutex::new(None);

#[cfg(test)]
impl Backend for TestBackend {
    fn load(&self, account: &str) -> Option<Vec<u8>> {
        MEM.lock()
            .unwrap_or_else(|e| e.into_inner())
            .as_ref()?
            .get(account)
            .cloned()
    }
    fn save(&self, account: &str, secret: &[u8]) -> Result<(), String> {
        MEM.lock()
            .unwrap_or_else(|e| e.into_inner())
            .get_or_insert_with(BTreeMap::new)
            .insert(account.to_string(), secret.to_vec());
        Ok(())
    }
    fn delete(&self, account: &str) -> Result<(), String> {
        if let Some(m) = MEM.lock().unwrap_or_else(|e| e.into_inner()).as_mut() {
            m.remove(account);
        }
        Ok(())
    }
}

/// Empty the in-memory test keychain.
#[cfg(test)]
pub fn reset() {
    *MEM.lock().unwrap_or_else(|e| e.into_inner()) = None;
}
