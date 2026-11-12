use std::io;
use std::path::Path;
use std::sync::Mutex;

use serde::{Deserialize, Serialize};

/// The file that holds this client's identity, beside `config.json`.
pub const CLIENT_FILE: &str = "client.json";

/// The on-disk shape version. A future migration can move it forward; a build
/// that does not recognize it treats the file as unreadable rather than
/// guessing at its meaning.
pub const ID_VERSION: u32 = 1;

/// The identity this installation presents to every daemon it talks to.
///
/// It is deliberately *not* authentication: the value is client-asserted and
/// only used to label workspace ownership and the workspace switcher. See
/// `docs/openspec/changes/client-workspace-ownership/design.md`.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct ClientIdentity {
    pub version: u32,
    pub id: String,
    pub name: String,
    /// Whether the workspace switcher shows every workspace or only this
    /// client's and unowned ones.
    pub show_all: bool,
    /// False when the identity could only be made for this process. UI surfaces
    /// use it to warn that the identity will not survive a restart.
    #[serde(skip)]
    pub persisted: bool,
}

impl Default for ClientIdentity {
    fn default() -> Self {
        Self {
            version: ID_VERSION,
            id: String::new(),
            name: String::new(),
            show_all: false,
            persisted: false,
        }
    }
}

impl ClientIdentity {
    /// An identity that exists for this process only. Used when the config
    /// directory cannot be written; ownership still works until the app exits.
    pub fn ephemeral() -> Self {
        Self {
            version: ID_VERSION,
            id: new_id(),
            name: default_name(),
            show_all: false,
            persisted: false,
        }
    }

    pub fn set_name(&mut self, name: impl Into<String>) {
        let name = name.into();
        self.name = if name.trim().is_empty() {
            default_name()
        } else {
            name
        };
    }
}

static CURRENT: Mutex<Option<ClientIdentity>> = Mutex::new(None);

/// Adopt the identity this process should present. Called once by the GUI
/// after the config directory is resolved.
pub fn install(identity: ClientIdentity) {
    *CURRENT.lock().unwrap_or_else(|e| e.into_inner()) = Some(identity);
}

/// The identity this process presents, loading it on first use.
pub fn current() -> ClientIdentity {
    if let Some(identity) = CURRENT.lock().unwrap_or_else(|e| e.into_inner()).clone() {
        return identity;
    }
    let identity = load_or_create();
    *CURRENT.lock().unwrap_or_else(|e| e.into_inner()) = Some(identity.clone());
    identity
}

/// Update the display name and persist it. Connections made after this return
/// carry the new name; existing connections keep the old one until they
/// reconnect.
pub fn set_name(name: impl Into<String>) -> io::Result<ClientIdentity> {
    let mut identity = current();
    identity.set_name(name);
    save(&identity)?;
    identity.persisted = true;
    *CURRENT.lock().unwrap_or_else(|e| e.into_inner()) = Some(identity.clone());
    Ok(identity)
}

/// Update the workspace-switcher preference and persist it.
pub fn set_show_all(show_all: bool) -> io::Result<ClientIdentity> {
    let mut identity = current();
    identity.show_all = show_all;
    save(&identity)?;
    identity.persisted = true;
    *CURRENT.lock().unwrap_or_else(|e| e.into_inner()) = Some(identity.clone());
    Ok(identity)
}

/// Load the identity from the active config directory, creating it on first
/// run. Never fails: a missing or unwritable directory yields an ephemeral
/// identity so startup is not blocked.
pub fn load_or_create() -> ClientIdentity {
    match crate::core::config::config_dir_path() {
        Some(dir) => load_or_create_at(&dir.join(CLIENT_FILE)),
        None => ClientIdentity::ephemeral(),
    }
}

/// Read `path`, create it when absent, and always return the identity the file
/// names. Concurrent first runs converge on one id: the `create_new` winner
/// writes, everyone re-reads.
pub fn load_or_create_at(path: &Path) -> ClientIdentity {
    if let Some(identity) = read(path) {
        return identity;
    }

    let generated = ClientIdentity {
        version: ID_VERSION,
        id: new_id(),
        name: default_name(),
        show_all: false,
        persisted: true,
    };
    if let Some(parent) = path.parent() {
        let _ = std::fs::create_dir_all(parent);
    }
    if path.exists() {
        // Present but unreadable. It may still be a `create_new` winner's empty
        // file, so give that writer a moment before deciding the JSON is
        // damaged. Only a file that stays unreadable is quarantined.
        if let Some(identity) = read_when_ready(path) {
            return identity;
        }
        let _ = std::fs::rename(path, path.with_extension("json.corrupt"));
    }
    match create_new(path, &generated) {
        Ok(()) => {}
        Err(e) if e.kind() == io::ErrorKind::AlreadyExists => {
            // Another process won `create_new` but may not have written the
            // body yet: the file exists while it is still empty. Wait for that
            // writer instead of minting a second id.
            if let Some(identity) = read_when_ready(path) {
                return identity;
            }
        }
        Err(e) => {
            log::warn!("could not write client identity at {}: {e}", path.display());
            return ClientIdentity {
                persisted: false,
                ..generated
            };
        }
    }

    match read(path) {
        Some(identity) => identity,
        None => {
            // The file is there but still unreadable — another writer may have
            // replaced it between our create and our read. Do not loop; this
            // session gets an ephemeral id.
            ClientIdentity {
                persisted: false,
                ..generated
            }
        }
    }
}

/// Persist mutable fields (name, `show_all`) for an identity already loaded.
pub fn save(identity: &ClientIdentity) -> io::Result<()> {
    let dir = crate::core::config::config_dir_path()
        .ok_or_else(|| io::Error::other("no config directory for the client identity"))?;
    save_at(&dir.join(CLIENT_FILE), identity)
}

pub fn save_at(path: &Path, identity: &ClientIdentity) -> io::Result<()> {
    let mut on_disk = identity.clone();
    on_disk.version = ID_VERSION;
    on_disk.persisted = true;
    let bytes = serde_json::to_vec_pretty(&on_disk)?;
    crate::core::config::write_atomic_private(path, &bytes)
}

fn read_when_ready(path: &Path) -> Option<ClientIdentity> {
    // A `create_new` winner holds the file open for a few microseconds before
    // its write lands. Poll briefly; giving up means an ephemeral id for this
    // process, never overwriting the winner.
    for _ in 0..2_000 {
        if let Some(identity) = read(path) {
            return Some(identity);
        }
        std::thread::sleep(std::time::Duration::from_millis(1));
    }
    None
}

fn read(path: &Path) -> Option<ClientIdentity> {
    let bytes = std::fs::read(path).ok()?;
    let mut identity: ClientIdentity = serde_json::from_slice(&bytes).ok()?;
    if identity.version != ID_VERSION || identity.id.trim().is_empty() {
        return None;
    }
    if identity.name.trim().is_empty() {
        identity.name = default_name();
    }
    identity.persisted = true;
    Some(identity)
}

fn create_new(path: &Path, identity: &ClientIdentity) -> io::Result<()> {
    use std::io::Write as _;

    let bytes = serde_json::to_vec_pretty(identity)?;
    let mut open = std::fs::OpenOptions::new();
    open.write(true).create_new(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt as _;
        open.mode(0o600);
    }
    let mut file = open.open(path)?;
    file.write_all(&bytes)?;
    file.flush()?;
    let _ = file.sync_all();
    Ok(())
}

fn new_id() -> String {
    uuid::Uuid::new_v4().to_string()
}

/// The default display name. Follows the OS name where it is cheap to read and
/// falls back to the environment before giving up.
fn default_name() -> String {
    #[cfg(unix)]
    {
        let mut buf = [0u8; 256];
        let rc = unsafe { libc::gethostname(buf.as_mut_ptr().cast(), buf.len()) };
        if rc == 0 {
            let end = buf.iter().position(|b| *b == 0).unwrap_or(buf.len());
            let name = String::from_utf8_lossy(&buf[..end]).trim().to_string();
            if !name.is_empty() {
                return name;
            }
        }
    }
    for key in ["COMPUTERNAME", "HOSTNAME"] {
        if let Ok(name) = std::env::var(key) {
            let name = name.trim();
            if !name.is_empty() {
                return name.to_string();
            }
        }
    }
    "tty7 client".to_string()
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;
    use std::sync::{Arc, Barrier};

    fn path(dir: &tempfile::TempDir) -> PathBuf {
        dir.path().join(CLIENT_FILE)
    }

    #[test]
    fn a_first_run_writes_an_id_and_the_next_run_reuses_it() {
        let dir = tempfile::tempdir().unwrap();
        let file = path(&dir);

        let first = load_or_create_at(&file);
        assert!(first.persisted);
        assert!(!first.id.is_empty());
        assert!(!first.name.is_empty());
        assert!(file.exists());

        let second = load_or_create_at(&file);
        assert_eq!(first.id, second.id);
        assert_eq!(first.name, second.name);
    }

    #[cfg(unix)]
    #[test]
    fn the_identity_file_is_private() {
        use std::os::unix::fs::PermissionsExt as _;

        let dir = tempfile::tempdir().unwrap();
        let file = path(&dir);
        load_or_create_at(&file);

        let mode = std::fs::metadata(&file).unwrap().permissions().mode() & 0o777;
        assert_eq!(
            mode, 0o600,
            "client.json must stay readable by its owner only"
        );
    }

    #[test]
    fn a_damaged_file_is_replaced_without_panicking() {
        let dir = tempfile::tempdir().unwrap();
        let file = path(&dir);
        std::fs::write(&file, b"{ not json").unwrap();

        let identity = load_or_create_at(&file);
        assert!(identity.persisted);
        assert!(!identity.id.is_empty());
        assert!(
            dir.path().join("client.json.corrupt").exists(),
            "the damaged file should be kept for diagnosis"
        );

        let again = load_or_create_at(&file);
        assert_eq!(identity.id, again.id);
    }

    #[test]
    fn concurrent_first_runs_all_agree_on_one_id() {
        let dir = tempfile::tempdir().unwrap();
        let file = Arc::new(path(&dir));
        let threads = 8;
        let barrier = Arc::new(Barrier::new(threads));
        let mut handles = Vec::new();

        for _ in 0..threads {
            let file = Arc::clone(&file);
            let barrier = Arc::clone(&barrier);
            handles.push(std::thread::spawn(move || {
                barrier.wait();
                load_or_create_at(&file)
            }));
        }

        let ids: Vec<String> = handles
            .into_iter()
            .map(|handle| handle.join().unwrap().id)
            .collect();
        assert!(
            ids.windows(2).all(|pair| pair[0] == pair[1]),
            "concurrent creators must converge on one id: {ids:?}"
        );
        assert_eq!(ids[0], load_or_create_at(&file).id);
    }

    #[test]
    fn saving_updates_the_name_and_keeps_the_id() {
        let dir = tempfile::tempdir().unwrap();
        let file = path(&dir);
        let mut identity = load_or_create_at(&file);
        let id = identity.id.clone();

        identity.set_name("Alice's laptop");
        identity.show_all = true;
        save_at(&file, &identity).unwrap();

        let reloaded = load_or_create_at(&file);
        assert_eq!(reloaded.id, id);
        assert_eq!(reloaded.name, "Alice's laptop");
        assert!(reloaded.show_all);
    }
}
