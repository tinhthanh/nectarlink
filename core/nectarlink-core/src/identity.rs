// SPDX-License-Identifier: MPL-2.0
//! The device key: created on first launch, stored encrypted at rest.

use std::{
    fs,
    io::{self, Write},
    path::Path,
    sync::Arc,
};

use iroh::SecretKey;

use crate::{Error, Result};

const FILE_NAME: &str = "identity.key";
const MAGIC: &[u8; 4] = b"NLK1";

/// Protects the device key at rest. Each platform provides one: DPAPI on
/// Windows, a login-keychain wrapping key on macOS, Android Keystore wrapping
/// on Android (injected by the app).
pub trait KeyProtector: Send + Sync {
    /// A stable one-byte identifier written into the key file.
    fn id(&self) -> u8;
    fn protect(&self, plaintext: &[u8]) -> io::Result<Vec<u8>>;
    fn unprotect(&self, ciphertext: &[u8]) -> io::Result<Vec<u8>>;
    /// Whether `sealed` was written before this protector took over (as plain
    /// bytes, by an older build) and should be written again, now protected.
    fn needs_reseal(&self, _sealed: &[u8]) -> bool {
        false
    }
}

/// No protection. Only used where no platform protector exists (tests,
/// headless tools); a warning is logged when it is used for a real key.
#[derive(Debug, Default, Clone, Copy)]
pub struct PlainKeyProtector;

impl KeyProtector for PlainKeyProtector {
    fn id(&self) -> u8 {
        0
    }
    fn protect(&self, plaintext: &[u8]) -> io::Result<Vec<u8>> {
        Ok(plaintext.to_vec())
    }
    fn unprotect(&self, ciphertext: &[u8]) -> io::Result<Vec<u8>> {
        Ok(ciphertext.to_vec())
    }
}

/// The best protector available on this platform.
pub fn default_protector() -> Arc<dyn KeyProtector> {
    #[cfg(windows)]
    {
        Arc::new(dpapi::DpapiKeyProtector)
    }
    #[cfg(target_os = "macos")]
    {
        Arc::new(keychain::KeychainKeyProtector::default())
    }
    #[cfg(not(any(windows, target_os = "macos")))]
    {
        Arc::new(PlainKeyProtector)
    }
}

/// Loads the device key from `dir`, creating and saving a new one on first
/// launch.
pub(crate) fn load_or_create(dir: &Path, protector: &dyn KeyProtector) -> Result<SecretKey> {
    let path = dir.join(FILE_NAME);
    match fs::read(&path) {
        Ok(bytes) => {
            let (key, was_plain) = decode(&bytes, protector)?;
            if was_plain {
                // Written by a build without protection on this platform.
                write_atomically(&path, &encode(&key, protector)?)?;
                tracing::info!("device key moved under platform protection");
            }
            Ok(key)
        }
        Err(e) if e.kind() == io::ErrorKind::NotFound => {
            let key = SecretKey::generate();
            if protector.id() == 0 {
                tracing::warn!("device key is stored without platform protection");
            }
            write_atomically(&path, &encode(&key, protector)?)?;
            tracing::info!(device = %crate::device_id(&key.public()).short(), "created device identity");
            Ok(key)
        }
        Err(e) => Err(e.into()),
    }
}

fn encode(key: &SecretKey, protector: &dyn KeyProtector) -> Result<Vec<u8>> {
    let sealed = protector.protect(&key.to_bytes())?;
    let mut out = Vec::with_capacity(MAGIC.len() + 1 + sealed.len());
    out.extend_from_slice(MAGIC);
    out.push(protector.id());
    out.extend_from_slice(&sealed);
    Ok(out)
}

/// Returns the key, and whether it was stored without protection although
/// `protector` protects (so the caller can rewrite the file).
fn decode(bytes: &[u8], protector: &dyn KeyProtector) -> Result<(SecretKey, bool)> {
    let corrupt = || Error::Storage("the device key file is corrupt".into());
    let rest = bytes.strip_prefix(MAGIC.as_slice()).ok_or_else(corrupt)?;
    let (&method, sealed) = rest.split_first().ok_or_else(corrupt)?;
    let plain_id = PlainKeyProtector.id();
    if method == plain_id && protector.id() != plain_id {
        let bytes: [u8; 32] = sealed.try_into().map_err(|_| corrupt())?;
        return Ok((SecretKey::from_bytes(&bytes), true));
    }
    if method != protector.id() {
        return Err(Error::Storage(format!(
            "the device key is protected with method {method}, but this build uses {}",
            protector.id()
        )));
    }
    let plain =
        protector.unprotect(sealed).map_err(|e| Error::Storage(format!("cannot unlock device key: {e}")))?;
    let bytes: [u8; 32] = plain.as_slice().try_into().map_err(|_| corrupt())?;
    Ok((SecretKey::from_bytes(&bytes), false))
}

/// Writes via a temporary file and rename, so a crash never leaves a
/// half-written key behind.
fn write_atomically(path: &Path, data: &[u8]) -> io::Result<()> {
    let tmp = path.with_extension("tmp");
    {
        // A fresh file, private from the start: a leftover (or a link) at
        // the temporary path is removed rather than written through.
        let _ = fs::remove_file(&tmp);
        let mut options = fs::OpenOptions::new();
        options.write(true).create_new(true);
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt;
            options.mode(0o600);
        }
        let mut file = options.open(&tmp)?;
        file.write_all(data)?;
        file.sync_all()?;
    }
    fs::rename(&tmp, path)
}

#[cfg(windows)]
#[allow(unsafe_code)]
mod dpapi {
    //! Windows Data Protection API: encrypts with a key tied to the current
    //! Windows user account.

    use std::{io, ptr, slice};

    use windows::Win32::{
        Foundation::{HLOCAL, LocalFree},
        Security::Cryptography::{
            CRYPT_INTEGER_BLOB, CRYPTPROTECT_UI_FORBIDDEN, CryptProtectData, CryptUnprotectData,
        },
    };

    use super::KeyProtector;

    /// Extra entropy so other apps of the same user can't trivially decrypt
    /// our blob with a bare `CryptUnprotectData` call.
    const ENTROPY: &[u8] = b"nectarlink/identity/v1";

    #[derive(Debug, Default, Clone, Copy)]
    pub struct DpapiKeyProtector;

    fn blob(data: &[u8]) -> CRYPT_INTEGER_BLOB {
        CRYPT_INTEGER_BLOB { cbData: data.len() as u32, pbData: data.as_ptr() as *mut u8 }
    }

    /// Copies a DPAPI output blob into a Vec and frees the original.
    ///
    /// # Safety
    /// `out` must have been filled by a successful DPAPI call.
    unsafe fn take(out: CRYPT_INTEGER_BLOB) -> Vec<u8> {
        // SAFETY: DPAPI returns a valid buffer of `cbData` bytes allocated with LocalAlloc.
        let data = unsafe { slice::from_raw_parts(out.pbData, out.cbData as usize) }.to_vec();
        // SAFETY: the buffer was allocated by DPAPI with LocalAlloc and is freed exactly once.
        unsafe { LocalFree(Some(HLOCAL(out.pbData.cast()))) };
        data
    }

    impl KeyProtector for DpapiKeyProtector {
        fn id(&self) -> u8 {
            1
        }

        fn protect(&self, plaintext: &[u8]) -> io::Result<Vec<u8>> {
            let input = blob(plaintext);
            let entropy = blob(ENTROPY);
            let mut out = CRYPT_INTEGER_BLOB { cbData: 0, pbData: ptr::null_mut() };
            // SAFETY: all pointers reference live buffers for the duration of the call.
            unsafe {
                CryptProtectData(
                    &input,
                    windows::core::w!("NectarLink device key"),
                    Some(&entropy),
                    None,
                    None,
                    CRYPTPROTECT_UI_FORBIDDEN,
                    &mut out,
                )
            }
            .map_err(io::Error::other)?;
            // SAFETY: the call succeeded, so `out` holds a DPAPI-allocated buffer.
            Ok(unsafe { take(out) })
        }

        fn unprotect(&self, ciphertext: &[u8]) -> io::Result<Vec<u8>> {
            let input = blob(ciphertext);
            let entropy = blob(ENTROPY);
            let mut out = CRYPT_INTEGER_BLOB { cbData: 0, pbData: ptr::null_mut() };
            // SAFETY: all pointers reference live buffers for the duration of the call.
            unsafe {
                CryptUnprotectData(
                    &input,
                    None,
                    Some(&entropy),
                    None,
                    None,
                    CRYPTPROTECT_UI_FORBIDDEN,
                    &mut out,
                )
            }
            .map_err(io::Error::other)?;
            // SAFETY: the call succeeded, so `out` holds a DPAPI-allocated buffer.
            Ok(unsafe { take(out) })
        }
    }
}

#[cfg(target_os = "macos")]
mod keychain {
    //! macOS: blobs are sealed with AES-256-GCM under a random wrapping key kept
    //! in the user's login keychain, so a copy of the data folder (a backup,
    //! another account, a stolen disk image) can't be read without the keychain.
    //! Like DPAPI, it does not stop software already running as the same user.

    use std::{io, sync::Mutex};

    use ring::{
        aead,
        rand::{SecureRandom, SystemRandom},
    };
    use security_framework::passwords::{get_generic_password, set_generic_password};

    use super::KeyProtector;

    const SERVICE: &str = "app.nectarlink";
    const ACCOUNT: &str = "device-key-wrap-v1";
    const ERR_ITEM_NOT_FOUND: i32 = -25300;
    const ERR_DUPLICATE_ITEM: i32 = -25299;
    /// Sealed blobs start with this; anything else was written in plain by an
    /// older build (before this protector) and is read as is, then resealed.
    const SEALED_MAGIC: &[u8; 5] = b"NLKC\x01";
    const AAD: &[u8] = b"nectarlink/keychain/v1";
    const NONCE_LEN: usize = 12;

    #[derive(Debug, Default)]
    pub struct KeychainKeyProtector {
        wrap_key: Mutex<Option<[u8; 32]>>,
    }

    impl KeychainKeyProtector {
        /// Reads the wrapping key once per process, creating it on first use.
        ///
        /// The keychain trusts the build that created the item; an unsigned
        /// rebuild gets a one-time "allow access" prompt (signed releases keep
        /// the same designated requirement and don't).
        fn wrap_key(&self) -> io::Result<[u8; 32]> {
            // Tests never touch the keychain: a rebuilt test binary would prompt.
            if cfg!(test) {
                return Ok([0x11; 32]);
            }
            let mut cached = self.wrap_key.lock().unwrap_or_else(|e| e.into_inner());
            if let Some(key) = *cached {
                return Ok(key);
            }
            let key = match get_generic_password(SERVICE, ACCOUNT) {
                Ok(bytes) => to_key(&bytes)?,
                Err(e) if e.code() == ERR_ITEM_NOT_FOUND => {
                    let mut key = [0u8; 32];
                    SystemRandom::new().fill(&mut key).map_err(|_| io::Error::other("RNG failure"))?;
                    match set_generic_password(SERVICE, ACCOUNT, &key) {
                        Ok(()) => key,
                        // Another instance created it first: use theirs.
                        Err(e) if e.code() == ERR_DUPLICATE_ITEM => {
                            to_key(&get_generic_password(SERVICE, ACCOUNT).map_err(io::Error::other)?)?
                        }
                        Err(e) => return Err(io::Error::other(e)),
                    }
                }
                Err(e) => return Err(io::Error::other(e)),
            };
            *cached = Some(key);
            Ok(key)
        }
    }

    fn to_key(bytes: &[u8]) -> io::Result<[u8; 32]> {
        bytes.try_into().map_err(|_| io::Error::other("the keychain wrapping key has the wrong length"))
    }

    fn aead_key(wrap_key: &[u8; 32]) -> io::Result<aead::LessSafeKey> {
        let key = aead::UnboundKey::new(&aead::AES_256_GCM, wrap_key)
            .map_err(|_| io::Error::other("invalid wrapping key"))?;
        Ok(aead::LessSafeKey::new(key))
    }

    pub(super) fn seal(wrap_key: &[u8; 32], plaintext: &[u8]) -> io::Result<Vec<u8>> {
        let mut nonce = [0u8; NONCE_LEN];
        SystemRandom::new().fill(&mut nonce).map_err(|_| io::Error::other("RNG failure"))?;
        let mut sealed = plaintext.to_vec();
        aead_key(wrap_key)?
            .seal_in_place_append_tag(
                aead::Nonce::assume_unique_for_key(nonce),
                aead::Aad::from(AAD),
                &mut sealed,
            )
            .map_err(|_| io::Error::other("can't seal"))?;
        let mut out = Vec::with_capacity(SEALED_MAGIC.len() + NONCE_LEN + sealed.len());
        out.extend_from_slice(SEALED_MAGIC);
        out.extend_from_slice(&nonce);
        out.extend_from_slice(&sealed);
        Ok(out)
    }

    /// `None` when `blob` isn't sealed (a plain blob from an older build).
    pub(super) fn open(wrap_key: &[u8; 32], blob: &[u8]) -> Option<io::Result<Vec<u8>>> {
        let rest = blob.strip_prefix(SEALED_MAGIC.as_slice())?;
        if rest.len() < NONCE_LEN {
            return Some(Err(io::Error::other("sealed data is truncated")));
        }
        let (nonce, sealed) = rest.split_at(NONCE_LEN);
        let nonce = aead::Nonce::try_assume_unique_for_key(nonce).ok()?;
        Some(aead_key(wrap_key).and_then(|key| {
            let mut buf = sealed.to_vec();
            key.open_in_place(nonce, aead::Aad::from(AAD), &mut buf)
                .map(|plain| plain.to_vec())
                .map_err(|_| io::Error::other("sealed data doesn't match this keychain's key"))
        }))
    }

    impl KeyProtector for KeychainKeyProtector {
        fn id(&self) -> u8 {
            3
        }

        fn protect(&self, plaintext: &[u8]) -> io::Result<Vec<u8>> {
            seal(&self.wrap_key()?, plaintext)
        }

        fn unprotect(&self, ciphertext: &[u8]) -> io::Result<Vec<u8>> {
            if !ciphertext.starts_with(SEALED_MAGIC) {
                return Ok(ciphertext.to_vec());
            }
            open(&self.wrap_key()?, ciphertext)
                .unwrap_or_else(|| Err(io::Error::other("sealed data is corrupt")))
        }

        fn needs_reseal(&self, sealed: &[u8]) -> bool {
            !sealed.starts_with(SEALED_MAGIC)
        }
    }
}

/// The install ID a device reports to its peers ([`DeviceInfo::install_id`]): a salted
/// BLAKE3 hash of a platform ID that survives app reinstalls (Android's `ANDROID_ID`,
/// Windows' `MachineGuid`). The raw ID never leaves the device; the salt keeps the hash
/// specific to NectarLink, so it can't be matched against other apps' identifiers.
///
/// [`DeviceInfo::install_id`]: nectarlink_protocol::messages::DeviceInfo::install_id
pub fn install_id_from(platform_id: &str) -> Option<String> {
    let raw = platform_id.trim();
    if raw.is_empty() {
        return None;
    }
    let mut hasher = blake3::Hasher::new_derive_key("nectarlink 2026 install id v1");
    hasher.update(raw.as_bytes());
    let hex = hasher.finalize().to_hex();
    Some(hex[..nectarlink_protocol::messages::INSTALL_ID_LEN].to_owned())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn creates_then_reloads_the_same_key() {
        let dir = tempfile::tempdir().unwrap();
        let protector = default_protector();
        let first = load_or_create(dir.path(), protector.as_ref()).unwrap();
        let second = load_or_create(dir.path(), protector.as_ref()).unwrap();
        assert_eq!(first.public(), second.public());
    }

    #[test]
    fn key_file_does_not_contain_the_plain_key_when_protected() {
        let dir = tempfile::tempdir().unwrap();
        let protector = default_protector();
        let key = load_or_create(dir.path(), protector.as_ref()).unwrap();
        let file = fs::read(dir.path().join(FILE_NAME)).unwrap();
        if protector.id() != 0 {
            let raw = key.to_bytes();
            assert!(!file.windows(raw.len()).any(|w| w == raw), "plaintext key found on disk");
        }
    }

    #[test]
    fn rejects_corrupt_and_mismatched_files() {
        let dir = tempfile::tempdir().unwrap();
        fs::write(dir.path().join(FILE_NAME), b"garbage").unwrap();
        assert!(matches!(load_or_create(dir.path(), &PlainKeyProtector), Err(Error::Storage(_))));

        let mut wrong_method = MAGIC.to_vec();
        wrong_method.push(42);
        wrong_method.extend_from_slice(&[0; 32]);
        fs::write(dir.path().join(FILE_NAME), wrong_method).unwrap();
        assert!(matches!(load_or_create(dir.path(), &PlainKeyProtector), Err(Error::Storage(_))));
    }

    /// A stand-in platform protector for tests.
    struct XorProtector;

    impl KeyProtector for XorProtector {
        fn id(&self) -> u8 {
            7
        }
        fn protect(&self, plaintext: &[u8]) -> io::Result<Vec<u8>> {
            Ok(plaintext.iter().map(|b| b ^ 0x5a).collect())
        }
        fn unprotect(&self, ciphertext: &[u8]) -> io::Result<Vec<u8>> {
            self.protect(ciphertext)
        }
    }

    #[test]
    fn upgrades_a_plain_key_file_to_the_platform_protector() {
        let dir = tempfile::tempdir().unwrap();
        let plain = load_or_create(dir.path(), &PlainKeyProtector).unwrap();

        let upgraded = load_or_create(dir.path(), &XorProtector).unwrap();
        assert_eq!(plain.public(), upgraded.public(), "same identity after the upgrade");
        let file = fs::read(dir.path().join(FILE_NAME)).unwrap();
        assert_eq!(file[MAGIC.len()], XorProtector.id(), "rewritten with the new protector");
        let raw = plain.to_bytes();
        assert!(!file.windows(raw.len()).any(|w| w == raw), "plaintext key left on disk");

        assert_eq!(load_or_create(dir.path(), &XorProtector).unwrap().public(), plain.public());
    }

    #[cfg(target_os = "macos")]
    #[test]
    fn keychain_seal_round_trips_and_detects_tampering_and_plain_blobs() {
        let wrap_key = [7u8; 32];
        let sealed = keychain::seal(&wrap_key, b"secret").unwrap();
        assert!(!sealed.windows(6).any(|w| w == b"secret"));
        assert_eq!(keychain::open(&wrap_key, &sealed).unwrap().unwrap(), b"secret");

        let mut tampered = sealed.clone();
        *tampered.last_mut().unwrap() ^= 1;
        assert!(keychain::open(&wrap_key, &tampered).unwrap().is_err());
        assert!(keychain::open(&[8u8; 32], &sealed).unwrap().is_err(), "another wrapping key");
        assert!(keychain::open(&wrap_key, b"{\"items\":[]}").is_none(), "plain JSON from an older build");

        let protector = keychain::KeychainKeyProtector::default();
        assert!(protector.needs_reseal(b"{}"));
        assert!(!protector.needs_reseal(&sealed));
    }

    #[test]
    fn install_ids_are_stable_salted_and_well_formed() {
        let a = install_id_from("9774d56d682e549c").unwrap();
        assert_eq!(a, install_id_from(" 9774d56d682e549c ").unwrap(), "stable for the same device");
        assert_ne!(a, install_id_from("9774d56d682e549d").unwrap(), "differs per device");
        assert!(nectarlink_protocol::messages::is_valid_install_id(&a));
        assert!(!a.contains("9774d56d682e549c"), "the raw ID never appears");
        assert_eq!(install_id_from("  "), None);
    }

    #[cfg(unix)]
    #[test]
    fn key_file_has_0600_permissions_on_unix() {
        use std::os::unix::fs::PermissionsExt;

        let dir = tempfile::tempdir().unwrap();
        let protector = default_protector();
        load_or_create(dir.path(), protector.as_ref()).unwrap();
        let meta = fs::metadata(dir.path().join(FILE_NAME)).unwrap();
        assert_eq!(meta.permissions().mode() & 0o777, 0o600);
    }
}
