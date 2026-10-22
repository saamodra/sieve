//! Root CA management and per-host leaf certificates.

use std::{
    fs,
    io::Write as _,
    net::IpAddr,
    num::NonZeroUsize,
    path::{Path, PathBuf},
    sync::{Arc, Mutex},
};

use anyhow::{Context, Result, bail};
use lru::LruCache;
use md5::{Digest as _, Md5};
use rcgen::{
    BasicConstraints, CertificateParams, DistinguishedName, DnType, ExtendedKeyUsagePurpose, IsCa,
    Issuer, KeyPair, KeyUsagePurpose, PKCS_ECDSA_P256_SHA256, PublicKeyData, SanType,
};
use rustls::{
    ServerConfig,
    pki_types::{CertificateDer, PrivateKeyDer, PrivatePkcs8KeyDer},
};
use sha2::Sha256;
use time::{Duration, OffsetDateTime};
use tracing::{info, warn};

use crate::config::Paths;

/// How long a newly generated root CA is valid. Roots are not subject to the
/// 398-day limit that applies to leaves, and a long lifetime means devices
/// don't need the CA reinstalled every year. Existing CAs keep their own dates.
const CA_VALIDITY: Duration = Duration::days(3650);
/// Leaf certificates are valid for at most this long (Apple caps TLS server
/// certificates at 398 days).
const LEAF_VALIDITY: Duration = Duration::days(365);
/// Backdating for devices whose clock runs a little behind.
const BACKDATE: Duration = Duration::days(1);
const LEAF_CACHE_SIZE: usize = 1024;

/// A TLS server config built from a leaf, kept with that leaf so a re-signed
/// leaf invalidates it.
type CachedServerConfig = (Arc<LeafCert>, Arc<ServerConfig>);

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ExportFormat {
    Pem,
    Der,
    /// DER with a `.cer` name, as iOS expects.
    Cer,
}

/// A certificate for one host, signed by the CA.
#[derive(Debug)]
pub struct LeafCert {
    /// The leaf followed by the CA. Clients that trust the CA ignore the extra
    /// certificate, and Chrome's `--ignore-certificate-errors-spki-list` needs
    /// the CA in the served chain to match its key.
    pub chain: Vec<CertificateDer<'static>>,
    pub key: PrivateKeyDer<'static>,
}

pub struct CertAuthority {
    cert_pem: String,
    cert_der: Vec<u8>,
    issuer: Issuer<'static, KeyPair>,
    not_before: OffsetDateTime,
    not_after: OffsetDateTime,
    leaves: Mutex<LruCache<String, Arc<LeafCert>>>,
    server_configs: Mutex<LruCache<String, CachedServerConfig>>,
}

impl CertAuthority {
    /// Loads `ca.pem`/`ca.key` from `paths`, generating them on first use.
    ///
    /// Holds an advisory lock while it checks and writes, so processes that
    /// start together against an empty config dir all end up with one CA.
    pub fn load_or_create(paths: &Paths) -> Result<Self> {
        let _lock = CaLock::acquire(paths)?;
        let (cert, key) = (paths.ca_cert(), paths.ca_key());
        match (cert.exists(), key.exists()) {
            (true, true) => Self::load(&cert, &key),
            (false, false) => {
                let ca = Self::generate()?;
                ca.save(paths)?;
                info!("generated a new CA at {}", cert.display());
                Ok(ca)
            }
            _ => bail!(
                "found only one of {} and {}; run `sieve ca regenerate` to start over",
                cert.display(),
                key.display()
            ),
        }
    }

    /// Replaces the CA on disk with a freshly generated one.
    pub fn regenerate(paths: &Paths) -> Result<Self> {
        let _lock = CaLock::acquire(paths)?;
        let ca = Self::generate()?;
        ca.save(paths)?;
        Ok(ca)
    }

    pub fn cert_pem(&self) -> &str {
        &self.cert_pem
    }

    pub fn cert_der(&self) -> &[u8] {
        &self.cert_der
    }

    pub fn export(&self, format: ExportFormat) -> Vec<u8> {
        match format {
            ExportFormat::Pem => self.cert_pem.as_bytes().to_vec(),
            ExportFormat::Der | ExportFormat::Cer => self.cert_der.clone(),
        }
    }

    /// OpenSSL's `subject_hash_old`, used as the Android cacerts file name:
    /// the first four bytes of the MD5 of the DER subject, as a little-endian
    /// u32 in lowercase hex.
    pub fn subject_hash_old(&self) -> Result<String> {
        let (_, cert) = x509_parser::parse_x509_certificate(&self.cert_der)
            .context("parsing the CA certificate")?;
        Ok(subject_hash_old(cert.subject().as_raw()))
    }

    pub fn not_before(&self) -> OffsetDateTime {
        self.not_before
    }

    pub fn not_after(&self) -> OffsetDateTime {
        self.not_after
    }

    /// SHA-256 of the DER certificate as colon-separated uppercase hex, the way
    /// browsers and keychains display it.
    pub fn sha256_fingerprint(&self) -> String {
        Sha256::digest(&self.cert_der)
            .iter()
            .map(|b| format!("{b:02X}"))
            .collect::<Vec<_>>()
            .join(":")
    }

    /// Returns a certificate for `host` (a DNS name or an IP address), signing a
    /// new one on a cache miss.
    pub fn leaf(&self, host: &str) -> Result<Arc<LeafCert>> {
        let host = normalize_host(host);
        if let Some(leaf) = self.leaves.lock().expect("leaf cache poisoned").get(&host) {
            return Ok(leaf.clone());
        }
        // Signing happens outside the lock; two racing misses for the same host
        // both sign, and the second one wins the cache slot.
        let leaf = Arc::new(self.sign_leaf(&host)?);
        self.leaves
            .lock()
            .expect("leaf cache poisoned")
            .put(host, leaf.clone());
        Ok(leaf)
    }

    /// Returns the TLS server config for `host`, calling `build` with the
    /// host's leaf on a cache miss. Reusing one config per host keeps its
    /// session cache, so clients can resume sessions instead of doing a full
    /// handshake every time.
    pub fn server_config_with(
        &self,
        host: &str,
        build: impl FnOnce(&LeafCert) -> Result<Arc<ServerConfig>>,
    ) -> Result<Arc<ServerConfig>> {
        let leaf = self.leaf(host)?;
        let host = normalize_host(host);
        if let Some((cached_leaf, config)) = self
            .server_configs
            .lock()
            .expect("server config cache poisoned")
            .get(&host)
            && Arc::ptr_eq(cached_leaf, &leaf)
        {
            return Ok(config.clone());
        }
        let config = build(&leaf)?;
        self.server_configs
            .lock()
            .expect("server config cache poisoned")
            .put(host, (leaf, config.clone()));
        Ok(config)
    }

    fn sign_leaf(&self, host: &str) -> Result<LeafCert> {
        let mut params = CertificateParams::default();
        let mut dn = DistinguishedName::new();
        // CN is limited to 64 characters; the SAN is what clients check anyway.
        if host.len() <= 64 {
            dn.push(DnType::CommonName, host);
        }
        params.distinguished_name = dn;
        params.subject_alt_names = vec![match host.parse::<IpAddr>() {
            Ok(ip) => SanType::IpAddress(ip),
            Err(_) => SanType::DnsName(
                host.try_into()
                    .with_context(|| format!("{host:?} is not a valid DNS name"))?,
            ),
        }];
        params.is_ca = IsCa::ExplicitNoCa;
        params.key_usages = vec![KeyUsagePurpose::DigitalSignature];
        params.extended_key_usages = vec![ExtendedKeyUsagePurpose::ServerAuth];
        params.use_authority_key_identifier_extension = true;
        let now = OffsetDateTime::now_utc();
        params.not_before = now - BACKDATE;
        params.not_after = (now + LEAF_VALIDITY).min(self.not_after);

        let key = KeyPair::generate_for(&PKCS_ECDSA_P256_SHA256)?;
        let cert = params
            .signed_by(&key, &self.issuer)
            .with_context(|| format!("signing a certificate for {host}"))?;
        Ok(LeafCert {
            chain: vec![cert.der().clone(), self.cert_der.clone().into()],
            key: PrivatePkcs8KeyDer::from(key.serialize_der()).into(),
        })
    }

    fn generate() -> Result<Self> {
        let key = KeyPair::generate_for(&PKCS_ECDSA_P256_SHA256)?;
        let mut params = CertificateParams::default();
        let mut dn = DistinguishedName::new();
        dn.push(DnType::CommonName, "sieve CA");
        dn.push(DnType::OrganizationName, "sieve");
        // Keeps the subject (and so the Android file name) distinct after a regenerate.
        dn.push(DnType::OrganizationalUnitName, random_id()?);
        params.distinguished_name = dn;
        params.is_ca = IsCa::Ca(BasicConstraints::Constrained(0));
        params.key_usages = vec![KeyUsagePurpose::KeyCertSign, KeyUsagePurpose::CrlSign];
        let now = OffsetDateTime::now_utc();
        params.not_before = now - BACKDATE;
        params.not_after = now + CA_VALIDITY;

        let cert = params.self_signed(&key)?;
        Self::from_parts(cert.pem(), key)
    }

    fn load(cert_path: &Path, key_path: &Path) -> Result<Self> {
        let cert_pem = fs::read_to_string(cert_path)
            .with_context(|| format!("reading {}", cert_path.display()))?;
        let key_pem = fs::read_to_string(key_path)
            .with_context(|| format!("reading {}", key_path.display()))?;
        restrict_permissions(key_path)?;
        let key = KeyPair::from_pem(&key_pem)
            .with_context(|| format!("parsing {}", key_path.display()))?;
        let ca = Self::from_parts(cert_pem, key)
            .with_context(|| format!("loading {}", cert_path.display()))?;
        let now = OffsetDateTime::now_utc();
        if ca.not_after < now {
            warn!(
                "the CA expired on {}; run `sieve ca regenerate` and trust it again",
                ca.not_after.date()
            );
        } else if ca.not_after - now < Duration::days(30) {
            warn!(
                "the CA expires on {}; run `sieve ca regenerate` soon",
                ca.not_after.date()
            );
        }
        Ok(ca)
    }

    fn from_parts(cert_pem: String, key: KeyPair) -> Result<Self> {
        let cert_der = rustls::pki_types::pem::PemObject::from_pem_slice(cert_pem.as_bytes())
            .map(|der: CertificateDer<'static>| der.to_vec())
            .context("the CA certificate is not valid PEM")?;
        let (_, parsed) =
            x509_parser::parse_x509_certificate(&cert_der).context("parsing the CA certificate")?;
        if parsed.public_key().raw != key.subject_public_key_info().as_slice() {
            bail!("the CA key does not match the CA certificate");
        }
        let not_before = parsed.validity().not_before.to_datetime();
        let not_after = parsed.validity().not_after.to_datetime();
        let issuer = Issuer::from_ca_cert_der(&cert_der.as_slice().into(), key)
            .context("the CA certificate cannot be used as an issuer")?;
        Ok(Self {
            cert_pem,
            cert_der,
            issuer,
            not_before,
            not_after,
            leaves: Mutex::new(LruCache::new(
                NonZeroUsize::new(LEAF_CACHE_SIZE).expect("non-zero"),
            )),
            server_configs: Mutex::new(LruCache::new(
                NonZeroUsize::new(LEAF_CACHE_SIZE).expect("non-zero"),
            )),
        })
    }

    fn save(&self, paths: &Paths) -> Result<()> {
        paths.ensure()?;
        write_atomic(
            &paths.ca_key(),
            self.issuer.key().serialize_pem().as_bytes(),
            0o600,
        )?;
        write_atomic(&paths.ca_cert(), self.cert_pem.as_bytes(), 0o644)
    }
}

/// An exclusive advisory lock on `ca.lock`, released when dropped (or when the
/// process exits, so a crash never leaves it held).
struct CaLock {
    _file: fs::File,
}

impl CaLock {
    fn acquire(paths: &Paths) -> Result<Self> {
        paths.ensure()?;
        let path = paths.config_dir.join("ca.lock");
        let file = fs::OpenOptions::new()
            .write(true)
            .create(true)
            .truncate(false)
            .open(&path)
            .with_context(|| format!("opening {}", path.display()))?;
        file.lock()
            .with_context(|| format!("locking {}", path.display()))?;
        Ok(Self { _file: file })
    }
}

fn subject_hash_old(subject_der: &[u8]) -> String {
    let digest = Md5::digest(subject_der);
    let hash = u32::from_le_bytes([digest[0], digest[1], digest[2], digest[3]]);
    format!("{hash:08x}")
}

/// Lowercases DNS names and strips the brackets from IPv6 literals.
fn normalize_host(host: &str) -> String {
    let host = host.trim_end_matches('.');
    let host = host
        .strip_prefix('[')
        .and_then(|h| h.strip_suffix(']'))
        .unwrap_or(host);
    host.to_ascii_lowercase()
}

fn random_id() -> Result<String> {
    let mut bytes = [0u8; 8];
    rustls::crypto::ring::default_provider()
        .secure_random
        .fill(&mut bytes)
        .map_err(|_| anyhow::anyhow!("the system random number generator failed"))?;
    Ok(bytes.iter().map(|b| format!("{b:02x}")).collect())
}

/// A temp path next to `path` that no other write shares: it carries the
/// target's file name, the pid and a random suffix.
fn temp_path(path: &Path) -> Result<PathBuf> {
    let name = path.file_name().unwrap_or_default().to_string_lossy();
    Ok(path.with_file_name(format!(
        ".{name}.{}.{}.tmp",
        std::process::id(),
        random_id()?
    )))
}

/// Writes through a temp file and a rename, so a crash never leaves a
/// half-written key behind, and the key is never readable by others.
fn write_atomic(path: &Path, contents: &[u8], mode: u32) -> Result<()> {
    let tmp = temp_path(path)?;
    let mut options = fs::OpenOptions::new();
    options.write(true).create_new(true);
    #[cfg(unix)]
    std::os::unix::fs::OpenOptionsExt::mode(&mut options, mode);
    #[cfg(not(unix))]
    let _ = mode;
    let mut file = options
        .open(&tmp)
        .with_context(|| format!("creating {}", tmp.display()))?;
    file.write_all(contents)
        .and_then(|()| file.sync_all())
        .with_context(|| format!("writing {}", tmp.display()))?;
    fs::rename(&tmp, path)
        .inspect_err(|_| {
            let _ = fs::remove_file(&tmp);
        })
        .with_context(|| format!("writing {}", path.display()))
}

#[cfg(unix)]
fn restrict_permissions(key_path: &Path) -> Result<()> {
    use std::os::unix::fs::PermissionsExt;
    let mode = fs::metadata(key_path)?.permissions().mode();
    if mode & 0o077 != 0 {
        warn!(
            "{} was readable by others; setting it to 0600",
            key_path.display()
        );
        fs::set_permissions(key_path, fs::Permissions::from_mode(0o600))
            .with_context(|| format!("setting permissions on {}", key_path.display()))?;
    }
    Ok(())
}

#[cfg(not(unix))]
fn restrict_permissions(_key_path: &Path) -> Result<()> {
    Ok(())
}

#[cfg(test)]
mod tests {
    use rustls::{
        RootCertStore,
        client::{WebPkiServerVerifier, danger::ServerCertVerifier},
        pki_types::{ServerName, UnixTime},
    };
    use x509_parser::{extensions::GeneralName, prelude::*};

    use super::*;

    fn ca_in(dir: &tempfile::TempDir) -> CertAuthority {
        CertAuthority::load_or_create(&Paths::at(dir.path())).unwrap()
    }

    fn verify(ca: &CertAuthority, leaf: &LeafCert, name: &str) -> Result<(), rustls::Error> {
        let mut roots = RootCertStore::empty();
        roots.add(ca.cert_der().to_vec().into()).unwrap();
        let verifier = WebPkiServerVerifier::builder_with_provider(
            Arc::new(roots),
            Arc::new(rustls::crypto::ring::default_provider()),
        )
        .build()
        .unwrap();
        verifier
            .verify_server_cert(
                &leaf.chain[0],
                &leaf.chain[1..],
                &ServerName::try_from(name.to_string()).unwrap(),
                &[],
                UnixTime::now(),
            )
            .map(|_| ())
    }

    #[test]
    fn generated_ca_is_a_ca_valid_for_about_ten_years() {
        let dir = tempfile::tempdir().unwrap();
        let ca = ca_in(&dir);
        let (_, cert) = parse_x509_certificate(ca.cert_der()).unwrap();

        let bc = cert.basic_constraints().unwrap().unwrap().value;
        assert!(bc.ca);
        let ku = cert.key_usage().unwrap().unwrap().value;
        assert!(ku.key_cert_sign() && ku.crl_sign());
        assert_eq!(cert.subject(), cert.issuer());

        let validity = cert.validity();
        let lifetime = validity.not_after.to_datetime() - validity.not_before.to_datetime();
        assert!(lifetime >= Duration::days(3600) && lifetime <= Duration::days(3700));
        assert!(validity.is_valid());
    }

    #[test]
    fn load_or_create_reuses_the_saved_ca_and_regenerate_replaces_it() {
        let dir = tempfile::tempdir().unwrap();
        let first = ca_in(&dir);
        let again = ca_in(&dir);
        assert_eq!(first.cert_pem(), again.cert_pem());

        let new = CertAuthority::regenerate(&Paths::at(dir.path())).unwrap();
        assert_ne!(first.cert_pem(), new.cert_pem());
        assert_eq!(ca_in(&dir).cert_pem(), new.cert_pem());
    }

    #[test]
    fn concurrent_first_runs_share_one_ca() {
        let dir = tempfile::tempdir().unwrap();
        let paths = Paths::at(dir.path());
        let threads = 16;
        let barrier = std::sync::Barrier::new(threads);
        let cas: Vec<CertAuthority> = std::thread::scope(|scope| {
            let handles: Vec<_> = (0..threads)
                .map(|_| {
                    scope.spawn(|| {
                        barrier.wait();
                        CertAuthority::load_or_create(&paths).unwrap()
                    })
                })
                .collect();
            handles.into_iter().map(|h| h.join().unwrap()).collect()
        });

        for ca in &cas {
            assert_eq!(ca.cert_der(), cas[0].cert_der());
        }
        let saved = CertAuthority::load(&paths.ca_cert(), &paths.ca_key()).unwrap();
        assert_eq!(saved.cert_der(), cas[0].cert_der());
        let stray: Vec<_> = fs::read_dir(dir.path())
            .unwrap()
            .map(|e| e.unwrap().file_name().to_string_lossy().into_owned())
            .filter(|name| name.ends_with(".tmp"))
            .collect();
        assert!(stray.is_empty(), "leftover temp files: {stray:?}");
    }

    #[test]
    fn temp_paths_are_unique_per_write_and_per_target() {
        let dir = Path::new("/ca");
        let (key, cert) = (dir.join("ca.key"), dir.join("ca.pem"));
        let paths = [
            temp_path(&key).unwrap(),
            temp_path(&key).unwrap(),
            temp_path(&cert).unwrap(),
        ];
        assert_ne!(paths[0], paths[1]);
        assert_ne!(paths[0], paths[2]);
        assert!(paths.iter().all(|p| p.parent() == Some(dir)));
    }

    #[test]
    fn missing_half_of_the_ca_is_an_error() {
        let dir = tempfile::tempdir().unwrap();
        ca_in(&dir);
        fs::remove_file(Paths::at(dir.path()).ca_cert()).unwrap();
        assert!(CertAuthority::load_or_create(&Paths::at(dir.path())).is_err());
    }

    #[cfg(unix)]
    #[test]
    fn key_file_is_private() {
        use std::os::unix::fs::PermissionsExt;
        let dir = tempfile::tempdir().unwrap();
        ca_in(&dir);
        let mode = fs::metadata(Paths::at(dir.path()).ca_key())
            .unwrap()
            .permissions()
            .mode();
        assert_eq!(mode & 0o777, 0o600);
    }

    #[test]
    fn export_formats() {
        let dir = tempfile::tempdir().unwrap();
        let ca = ca_in(&dir);
        assert!(
            ca.export(ExportFormat::Pem)
                .starts_with(b"-----BEGIN CERTIFICATE-----")
        );
        assert_eq!(ca.export(ExportFormat::Der), ca.cert_der());
        assert_eq!(ca.export(ExportFormat::Cer), ca.cert_der());
    }

    #[test]
    fn leaf_for_dns_name_has_san_and_chains_to_ca() {
        let dir = tempfile::tempdir().unwrap();
        let ca = ca_in(&dir);
        let leaf = ca.leaf("Example.COM").unwrap();

        let (_, cert) = parse_x509_certificate(&leaf.chain[0]).unwrap();
        let san = cert.subject_alternative_name().unwrap().unwrap().value;
        assert_eq!(san.general_names, vec![GeneralName::DNSName("example.com")]);
        assert!(!cert.basic_constraints().unwrap().unwrap().value.ca);

        verify(&ca, &leaf, "example.com").unwrap();
        assert!(verify(&ca, &leaf, "other.example.com").is_err());
    }

    #[test]
    fn leaf_for_ip_has_ip_san_and_chains_to_ca() {
        let dir = tempfile::tempdir().unwrap();
        let ca = ca_in(&dir);
        for (host, ip, octets) in [
            ("10.0.2.2", "10.0.2.2", vec![10, 0, 2, 2]),
            ("[::1]", "::1", {
                let mut v = vec![0; 15];
                v.push(1);
                v
            }),
        ] {
            let leaf = ca.leaf(host).unwrap();
            let (_, cert) = parse_x509_certificate(&leaf.chain[0]).unwrap();
            let san = cert.subject_alternative_name().unwrap().unwrap().value;
            assert_eq!(san.general_names, vec![GeneralName::IPAddress(&octets)]);
            verify(&ca, &leaf, ip).unwrap();
        }
    }

    #[test]
    fn bracketed_ipv6_literal_shares_the_cache_entry_with_the_bare_address() {
        let dir = tempfile::tempdir().unwrap();
        let ca = ca_in(&dir);
        let bracketed = ca.leaf("[::1]").unwrap();
        assert!(Arc::ptr_eq(&bracketed, &ca.leaf("::1").unwrap()));

        let (_, cert) = parse_x509_certificate(&bracketed.chain[0]).unwrap();
        let san = cert.subject_alternative_name().unwrap().unwrap().value;
        let mut octets = vec![0; 15];
        octets.push(1);
        assert_eq!(san.general_names, vec![GeneralName::IPAddress(&octets)]);
        verify(&ca, &bracketed, "::1").unwrap();
    }

    #[test]
    fn leaf_does_not_outlive_the_ca() {
        let dir = tempfile::tempdir().unwrap();
        let ca = ca_in(&dir);
        let leaf = ca.leaf("example.com").unwrap();
        let (_, cert) = parse_x509_certificate(&leaf.chain[0]).unwrap();
        assert!(cert.validity().not_after.to_datetime() <= ca.not_after());
    }

    #[test]
    fn leaves_are_cached_per_host() {
        let dir = tempfile::tempdir().unwrap();
        let ca = ca_in(&dir);
        let a = ca.leaf("a.example").unwrap();
        assert!(Arc::ptr_eq(&a, &ca.leaf("a.example").unwrap()));
        assert!(Arc::ptr_eq(&a, &ca.leaf("A.example.").unwrap()));
        assert!(!Arc::ptr_eq(&a, &ca.leaf("b.example").unwrap()));
    }

    #[test]
    fn subject_hash_old_matches_openssl() {
        let dir = tempfile::tempdir().unwrap();
        let ca = ca_in(&dir);
        let ours = ca.subject_hash_old().unwrap();
        assert_eq!(ours.len(), 8);
        assert!(
            ours.chars()
                .all(|c| c.is_ascii_hexdigit() && !c.is_ascii_uppercase())
        );

        let output = std::process::Command::new("openssl")
            .args(["x509", "-noout", "-subject_hash_old", "-in"])
            .arg(Paths::at(dir.path()).ca_cert())
            .output();
        match output {
            Ok(out) if out.status.success() => {
                assert_eq!(String::from_utf8(out.stdout).unwrap().trim(), ours);
            }
            _ => eprintln!("skipping the openssl comparison: openssl is not available"),
        }
    }

    #[test]
    fn subject_hash_old_matches_a_known_value() {
        // ISRG Root X1; `openssl x509 -subject_hash_old` prints 6187b673.
        const ISRG_ROOT_X1_SUBJECT: &[u8] = &[
            0x30, 0x4f, 0x31, 0x0b, 0x30, 0x09, 0x06, 0x03, 0x55, 0x04, 0x06, 0x13, 0x02, 0x55,
            0x53, 0x31, 0x29, 0x30, 0x27, 0x06, 0x03, 0x55, 0x04, 0x0a, 0x13, 0x20, 0x49, 0x6e,
            0x74, 0x65, 0x72, 0x6e, 0x65, 0x74, 0x20, 0x53, 0x65, 0x63, 0x75, 0x72, 0x69, 0x74,
            0x79, 0x20, 0x52, 0x65, 0x73, 0x65, 0x61, 0x72, 0x63, 0x68, 0x20, 0x47, 0x72, 0x6f,
            0x75, 0x70, 0x31, 0x15, 0x30, 0x13, 0x06, 0x03, 0x55, 0x04, 0x03, 0x13, 0x0c, 0x49,
            0x53, 0x52, 0x47, 0x20, 0x52, 0x6f, 0x6f, 0x74, 0x20, 0x58, 0x31,
        ];
        assert_eq!(subject_hash_old(ISRG_ROOT_X1_SUBJECT), "6187b673");
    }
}
