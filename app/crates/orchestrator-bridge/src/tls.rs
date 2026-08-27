//! The bridge's TLS identity: one self-signed key, minted once and then kept.
//!
//! ## Why self-signed, and why that is not a downgrade
//!
//! No certificate authority will ever issue for `192.168.0.71` or
//! `100.68.100.56`, so a CA-signed certificate is not an option and hostname
//! validation has nothing to validate. The phone's ONLY notion of who it is
//! talking to is [`Tls::fingerprint`] — the SHA-256 of the DER
//! SubjectPublicKeyInfo — delivered out of band in the pairing QR. Pinning the
//! KEY rather than a name is what makes this Mac's address a detail: a DHCP
//! renewal, a new Wi-Fi network, a Tailscale restart, all change the address the
//! phone dials and none of them change who answers.
//!
//! ## Why the key is persisted
//!
//! The pin the phone stored at pairing has to keep matching. Minting a fresh key
//! on every start would hand every phone a certificate it has never seen, which
//! looks exactly like an interception attempt and is refused — every phone
//! SILENTLY UNPAIRED by a restart, with no error the user can act on. So the
//! identity is written once and reused, and the only thing that ever regenerates
//! it is a file that is gone or unreadable.
//!
//! ## Where it lives
//!
//! `~/Library/Application Support/orchestrator/bridge-identity.pem`, mode 0600 —
//! the same 0700 directory the store lives in, chosen because the pin must outlive
//! the system's temp reaper (see `identity_dir`). The bridge is
//! handed the socket path as its only argument, so deriving the directory from it
//! needs no second channel for the two processes to agree on.
//!
//! Certificate and key share ONE file on purpose. Two files can be half-written:
//! a crash between them leaves a key that does not match a certificate, which
//! fails to load, which regenerates — the un-pairing above, arrived at by
//! accident. One file, written to a temporary name and renamed into place, is
//! either wholly there or not there at all.

use std::fs;
use std::io::Write;
use std::net::{IpAddr, Ipv4Addr};
use std::path::{Path, PathBuf};
use std::sync::Arc;

use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use base64::Engine as _;
use rustls::pki_types::{CertificateDer, PrivateKeyDer};
use sha2::{Digest, Sha256};

/// The one file, holding the certificate and then the private key.
const IDENTITY: &str = "bridge-identity.pem";

/// A loaded TLS identity: what to serve, and what the phone must have pinned.
pub struct Tls {
    config: Arc<rustls::ServerConfig>,
    fingerprint: String,
}

impl Tls {
    /// Load the stored identity, or mint and store one on first use.
    ///
    /// `sans` are the non-loopback addresses about to be bound. They go into the
    /// certificate's SANs for the benefit of anything that DOES check names —
    /// a browser poking at the port, `openssl s_client`, a future desktop client
    /// — but they are NOT the trust anchor and must never behave like one. The
    /// certificate is minted once and then reused forever, so the day this Mac's
    /// LAN address changes its SAN list is stale by definition; a phone that
    /// refused to connect over that would be refusing on the strength of a field
    /// nobody promised to keep current. The pin is the whole check.
    pub fn load_or_mint(dir: &Path, sans: &[Ipv4Addr]) -> Result<Self, String> {
        let path = dir.join(IDENTITY);
        if let Ok(pem) = fs::read_to_string(&path) {
            match Self::from_pem(&pem) {
                Ok(tls) => return Ok(tls),
                // Regenerating here is a deliberate, LOUD choice between two bad
                // outcomes: refusing to start leaves the user with a bridge that
                // will not come up and no way back, while minting fresh gets them
                // running at the price of re-pairing. Say so, on the way past —
                // an unexplained "your phone can no longer connect" is the worst
                // of the three.
                Err(e) => eprintln!(
                    "bridge: {} is unreadable ({e}); minting a new identity. Every paired \
                     phone must scan the pairing code again.",
                    path.display()
                ),
            }
        }
        let pem = mint(sans)?;
        write_private(dir, &path, &pem)?;
        Self::from_pem(&pem)
    }

    /// Build from PEM without touching the filesystem — the whole of the parsing
    /// and pinning logic, so tests exercise the same path a restart takes.
    pub fn from_pem(pem: &str) -> Result<Self, String> {
        let certs: Vec<CertificateDer<'static>> = rustls_pemfile::certs(&mut pem.as_bytes())
            .collect::<Result<_, _>>()
            .map_err(|e| format!("the stored certificate is not readable PEM: {e}"))?;
        let leaf = certs
            .first()
            .ok_or_else(|| "the stored identity contains no certificate".to_string())?;
        let fingerprint = fingerprint_of(leaf)?;
        let key: PrivateKeyDer<'static> = rustls_pemfile::private_key(&mut pem.as_bytes())
            .map_err(|e| format!("the stored private key is not readable PEM: {e}"))?
            .ok_or_else(|| "the stored identity contains no private key".to_string())?;

        // `builder_with_provider`, never the bare `builder()`. The bare one reads
        // a PROCESS-WIDE default provider, and this crate is linked into the GUI,
        // whose HTTP stack installs its own — so the bare builder is either a
        // panic ("no process-level CryptoProvider available") or a silent
        // dependency on which crate got there first. Naming the provider here
        // makes the bridge's TLS independent of everything else in the process.
        let provider = Arc::new(rustls::crypto::ring::default_provider());
        let config = rustls::ServerConfig::builder_with_provider(provider)
            .with_safe_default_protocol_versions()
            .map_err(|e| format!("rustls rejected its own defaults: {e}"))?
            // No client auth: the phone proves itself with the bearer token
            // inside the tunnel, and a client certificate would be a second
            // credential to pair, revoke and lose.
            .with_no_client_auth()
            .with_single_cert(certs, key)
            .map_err(|e| format!("the certificate and key do not go together: {e}"))?;

        Ok(Self { config: Arc::new(config), fingerprint })
    }

    /// What the phone pins: base64url (no padding) of the SHA-256 of the DER
    /// SubjectPublicKeyInfo. This exact string goes in the pairing URL's `f`.
    pub fn fingerprint(&self) -> String {
        self.fingerprint.clone()
    }

    pub fn config(&self) -> Arc<rustls::ServerConfig> {
        Arc::clone(&self.config)
    }
}

/// The identity directory for a bridge started against `socket`.
///
/// APPLICATION SUPPORT, not the runtime directory. The key must survive for as
/// long as any phone is paired — months — and the runtime directory is the
/// daemon's SOCKET directory, which on macOS lives under `$TMPDIR` and is reaped
/// by the system once its contents go untouched for a few days. Storing the pin
/// there means every paired phone silently refusing to connect after a quiet
/// week, presenting as an intermittent connection bug with no visible cause and
/// no error that names the real problem. An earlier revision of this file did
/// exactly that, and documented the hazard in a comment while still defaulting to
/// it.
///
/// The directory is the one the app already keeps its store in, so it inherits
/// the 0700 that `open_store` sets on every launch, and a sandboxed run (which
/// overrides `HOME`) gets its own identity for free rather than borrowing the
/// real one.
///
/// `KOD_BRIDGE_TLS_DIR` still overrides, for tests and for anyone who wants the
/// key somewhere specific. The socket's directory is the last resort, used only
/// when there is no `HOME` at all.
pub fn identity_dir(socket: &Path) -> PathBuf {
    identity_dir_from(
        socket,
        std::env::var_os("KOD_BRIDGE_TLS_DIR"),
        std::env::var_os("HOME"),
    )
}

/// The pure half. Both inputs are passed in rather than read here, because env is
/// process-global: a test that set `KOD_BRIDGE_TLS_DIR` to check the override
/// raced the test asserting it was unset, and the pair passed alone and failed
/// together.
pub fn identity_dir_from(
    socket: &Path,
    override_dir: Option<std::ffi::OsString>,
    home: Option<std::ffi::OsString>,
) -> PathBuf {
    if let Some(dir) = override_dir {
        return PathBuf::from(dir);
    }
    if let Some(home) = home {
        let p = PathBuf::from(home).join("Library/Application Support/orchestrator");
        if std::fs::create_dir_all(&p).is_ok() {
            #[cfg(unix)]
            {
                use std::os::unix::fs::PermissionsExt;
                let _ = std::fs::set_permissions(&p, std::fs::Permissions::from_mode(0o700));
            }
            return p;
        }
    }
    socket
        .parent()
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from("."))
}

/// base64url-unpadded SHA-256 over the DER SubjectPublicKeyInfo inside `cert_der`.
///
/// Taken from the CERTIFICATE, not from the key that signed it, because that is
/// the only bytes the phone ever sees: it hashes the SPKI out of the leaf the
/// handshake presented. Deriving this from the key pair instead would agree
/// today and could quietly stop agreeing the day the two are not a matched pair,
/// and the symptom would be every phone refusing every connection with nothing
/// but a generic TLS error to go on.
pub fn fingerprint_of(cert_der: &[u8]) -> Result<String, String> {
    let spki = spki_der(cert_der)?;
    Ok(URL_SAFE_NO_PAD.encode(Sha256::digest(spki)))
}

/// The DER SubjectPublicKeyInfo, tag and length included, sliced out of an X.509
/// certificate.
///
/// RFC 5280:
///
/// ```text
/// Certificate     ::= SEQUENCE { tbsCertificate, signatureAlgorithm, signature }
/// TBSCertificate  ::= SEQUENCE { [0] version DEFAULT v1, serialNumber, signature,
///                                issuer, validity, subject, subjectPublicKeyInfo, ... }
/// ```
///
/// so it is: into the outer SEQUENCE, into the first field, skip the optional
/// `[0]` version, skip five fields, take the sixth. Everything on that path is
/// structural — no OIDs are read and no key is interpreted — which is why a
/// hundred lines of ASN.1 library are not needed to reach it.
pub fn spki_der(cert_der: &[u8]) -> Result<&[u8], String> {
    let cert = tlv(cert_der)?;
    if cert.tag != SEQUENCE {
        return Err("not a certificate: the outer element is not a SEQUENCE".into());
    }
    let tbs = tlv(cert.value)?;
    if tbs.tag != SEQUENCE {
        return Err("not a certificate: tbsCertificate is not a SEQUENCE".into());
    }
    let mut rest = tbs.value;
    let first = tlv(rest)?;
    // `[0] EXPLICIT version` is optional and absent from a v1 certificate, so its
    // presence decides whether the SPKI is the sixth or the seventh field.
    if first.tag == CONTEXT_0 {
        rest = &rest[first.whole.len()..];
    }
    for field in ["serialNumber", "signature", "issuer", "validity", "subject"] {
        let f = tlv(rest).map_err(|e| format!("truncated before {field}: {e}"))?;
        rest = &rest[f.whole.len()..];
    }
    let spki = tlv(rest).map_err(|e| format!("truncated before subjectPublicKeyInfo: {e}"))?;
    if spki.tag != SEQUENCE {
        return Err(format!(
            "subjectPublicKeyInfo is tag {:#04x}, not a SEQUENCE — this is not an X.509 \
             certificate and pinning it would pin the wrong bytes",
            spki.tag
        ));
    }
    Ok(spki.whole)
}

const SEQUENCE: u8 = 0x30;
const CONTEXT_0: u8 = 0xa0;

/// One DER element: its tag, its whole encoding, and its contents.
struct Tlv<'a> {
    tag: u8,
    whole: &'a [u8],
    value: &'a [u8],
}

fn tlv(buf: &[u8]) -> Result<Tlv<'_>, String> {
    let tag = *buf.first().ok_or("truncated DER: no tag")?;
    // A low tag number is the only form on the path this walker takes. Refusing
    // the multi-byte form rather than mis-parsing it keeps the length arithmetic
    // below honest about where the element ends.
    if tag & 0x1f == 0x1f {
        return Err("multi-byte DER tag where none is expected".into());
    }
    let first = *buf.get(1).ok_or("truncated DER: no length")?;
    let (len, header) = if first < 0x80 {
        (first as usize, 2)
    } else {
        let n = (first & 0x7f) as usize;
        // 0x80 is the indefinite length: legal in BER, forbidden in DER, and a
        // certificate that used it would have no stated end to slice at.
        if n == 0 || n > 4 {
            return Err("indefinite or oversized DER length".into());
        }
        let bytes = buf.get(2..2 + n).ok_or("truncated DER length")?;
        (bytes.iter().fold(0usize, |acc, b| (acc << 8) | *b as usize), 2 + n)
    };
    let whole = buf.get(..header + len).ok_or("truncated DER: element runs past the buffer")?;
    Ok(Tlv { tag, whole, value: &whole[header..] })
}

/// Mint a fresh self-signed identity, returning `<certificate PEM><key PEM>`.
///
/// In-process, via rcgen, rather than by driving `openssl`: a subprocess means
/// the private key travelling through argv, a pipe or a temp file, and argv on
/// macOS is world-readable.
fn mint(sans: &[Ipv4Addr]) -> Result<String, String> {
    let mut names = vec!["127.0.0.1".to_string(), "localhost".to_string()];
    names.extend(sans.iter().map(|ip| ip.to_string()));
    names.dedup();

    let key = rcgen::KeyPair::generate()
        .map_err(|e| format!("could not generate a key for the bridge: {e}"))?;
    let mut params = rcgen::CertificateParams::new(names)
        .map_err(|e| format!("could not describe the bridge's certificate: {e}"))?;
    params.distinguished_name.push(rcgen::DnType::CommonName, "Kod bridge");
    params.key_usages = vec![rcgen::KeyUsagePurpose::DigitalSignature];
    params.extended_key_usages = vec![rcgen::ExtendedKeyUsagePurpose::ServerAuth];
    // An expiry date on a PINNED key is a scheduled un-pairing: nothing renews
    // this certificate, so the day it lapses is the day every phone stops
    // connecting. The window is therefore longer than the product, and the start
    // is far enough back that a Mac with a wrong clock still serves.
    params.not_before = rcgen::date_time_ymd(2000, 1, 1);
    params.not_after = rcgen::date_time_ymd(2099, 1, 1);
    let cert = params
        .self_signed(&key)
        .map_err(|e| format!("could not sign the bridge's certificate: {e}"))?;

    Ok(format!("{}{}", cert.pem(), key.serialize_pem()))
}

/// Write the identity 0600, atomically.
///
/// The mode is set AT CREATION rather than afterwards: a `set_permissions` after
/// the write leaves a window in which the private key exists world-readable, and
/// on a shared Mac that window is the whole vulnerability. The rename is what
/// makes a crash mid-write harmless — a reader sees the old file or the new one,
/// never half of either.
fn write_private(dir: &Path, path: &Path, pem: &str) -> Result<(), String> {
    fs::create_dir_all(dir)
        .map_err(|e| format!("cannot create {} for the bridge's key: {e}", dir.display()))?;
    let tmp = dir.join(format!(".{IDENTITY}.tmp"));
    let write = || -> std::io::Result<()> {
        let mut f = {
            let mut o = fs::OpenOptions::new();
            o.write(true).create(true).truncate(true);
            #[cfg(unix)]
            {
                use std::os::unix::fs::OpenOptionsExt;
                o.mode(0o600);
            }
            o.open(&tmp)?
        };
        f.write_all(pem.as_bytes())?;
        f.sync_all()
    };
    write().map_err(|e| format!("cannot write {}: {e}", tmp.display()))?;
    fs::rename(&tmp, path).map_err(|e| {
        let _ = fs::remove_file(&tmp);
        format!("cannot install {}: {e}", path.display())
    })
}

/// The SAN list a certificate should carry: loopback plus whatever else is being
/// bound. Public so the caller that resolves the bind can hand it straight over.
pub fn sans_for(extra: &[Ipv4Addr]) -> Vec<IpAddr> {
    let mut v = vec![IpAddr::V4(Ipv4Addr::LOCALHOST)];
    v.extend(extra.iter().copied().map(IpAddr::V4));
    v
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Minted with the SYSTEM openssl, and its pin computed the long way round:
    ///
    /// ```text
    /// openssl x509 -in c.pem -pubkey -noout \
    ///   | openssl pkey -pubin -outform DER \
    ///   | openssl dgst -sha256 -binary | openssl base64
    /// → PxzwAjQTKyQ/naZJoUs8563rFF5OMq2sZhU2+fYSBkY=
    /// ```
    ///
    /// This one was picked out of a run of candidates BECAUSE its digest contains
    /// both a `+` and a `/`: standard base64 and base64url differ in exactly those
    /// two characters plus the padding, so a vector without them would pass under
    /// all three of the encodings this could plausibly have been written as.
    const NAMED_CURVE_PEM: &str = "\
-----BEGIN CERTIFICATE-----
MIIBVTCB+6ADAgECAgkAwEA9Icj46IYwCgYIKoZIzj0EAwIwIDEeMBwGA1UEAwwV
a29kLWJyaWRnZS1waW4tdmVjdG9yMB4XDTI2MDgyNTIzNDg1NFoXDTM2MDgyMjIz
NDg1NFowIDEeMBwGA1UEAwwVa29kLWJyaWRnZS1waW4tdmVjdG9yMFkwEwYHKoZI
zj0CAQYIKoZIzj0DAQcDQgAEL8WlmDMXyZ904IexCiGbf1TPiAMslpwfsXphT/e6
s8OZTZU2H5uw7SdZFvhUhxzr92fqEa7jXt2XLn8Fi1/8fKMeMBwwGgYDVR0RBBMw
EYcEfwAAAYIJbG9jYWxob3N0MAoGCCqGSM49BAMCA0kAMEYCIQCO2ejUfaAo2mBd
CmW307b7jgVBx2HkLUWey2gQqQfZdgIhALIfAWHCbov/q1xvPtbO5LFzeeIxkW6c
1/FmQJ9Wdpi6
-----END CERTIFICATE-----
";
    const NAMED_CURVE_PIN: &str = "PxzwAjQTKyQ_naZJoUs8563rFF5OMq2sZhU2-fYSBkY";

    /// The same, for a key written with EXPLICIT curve parameters instead of a
    /// named curve. Its SubjectPublicKeyInfo is an order of magnitude longer and
    /// its AlgorithmIdentifier is a nested SEQUENCE — the case where a walker
    /// that guessed at field widths instead of reading lengths comes apart.
    ///
    /// `openssl` said: tasYgVZF57I3vCnkypCXJdD6C2nebtoU69Onyufnp8I=
    const EXPLICIT_PARAMS_PEM: &str = "\
-----BEGIN CERTIFICATE-----
MIICNzCCAdygAwIBAgIJAOh02/RG+kEDMAoGCCqGSM49BAMCMBwxGjAYBgNVBAMM
EWtvZC1icmlkZ2UtZ29sZGVuMB4XDTI2MDgyNTIzNDg0MloXDTM2MDgyMjIzNDg0
MlowHDEaMBgGA1UEAwwRa29kLWJyaWRnZS1nb2xkZW4wggFLMIIBAwYHKoZIzj0C
ATCB9wIBATAsBgcqhkjOPQEBAiEA/////wAAAAEAAAAAAAAAAAAAAAD/////////
//////8wWwQg/////wAAAAEAAAAAAAAAAAAAAAD///////////////wEIFrGNdiq
OpPns+u9VXaYhrxlHQawzFOw9jvOPD4n0mBLAxUAxJ02CIbnBJNqZnjhE50mt4Gf
fpAEQQRrF9Hy4SxCR/i85uVjpEDydwN9gS3rM6D0oTlF2JjClk/jQuL+Gn+bjufr
SnwPnhYrzjNXazFezsu2QGg3v1H1AiEA/////wAAAAD//////////7zm+q2nF56E
87nKwvxjJVECAQEDQgAESg+pHTMnxW5eVAFl4Ciyi4DXRq2AVXSCZ5imFGIj9SQt
PW0xswBreJNoXRxCsChefvjjaJX4N2VXMR7mTRDIlqMTMBEwDwYDVR0RBAgwBocE
fwAAATAKBggqhkjOPQQDAgNJADBGAiEAm7whDRv4k7+e8FYRfHmXnny8japja9zc
b7povjOFQygCIQC3SLgZJ0HFD98BLwmSdaIylyQFV/Yy2U7NavBStvOOlQ==
-----END CERTIFICATE-----
";
    const EXPLICIT_PARAMS_PIN: &str = "tasYgVZF57I3vCnkypCXJdD6C2nebtoU69Onyufnp8I";

    fn der(pem: &str) -> Vec<u8> {
        rustls_pemfile::certs(&mut pem.as_bytes())
            .next()
            .unwrap()
            .unwrap()
            .to_vec()
    }

    fn tmpdir(name: &str) -> PathBuf {
        let d = std::env::temp_dir().join(format!(
            "kod-tls-test-{name}-{}-{:?}",
            std::process::id(),
            std::thread::current().id()
        ));
        let _ = fs::remove_dir_all(&d);
        fs::create_dir_all(&d).unwrap();
        d
    }

    #[test]
    fn the_pin_is_base64url_unpadded_sha256_of_the_der_spki() {
        // The contract value. A mismatch here is a phone that refuses every
        // connection it will ever make, with nothing but a TLS error to say so —
        // so these are pinned against a digest computed by a different program.
        assert_eq!(fingerprint_of(&der(NAMED_CURVE_PEM)).unwrap(), NAMED_CURVE_PIN);
        assert_eq!(fingerprint_of(&der(EXPLICIT_PARAMS_PEM)).unwrap(), EXPLICIT_PARAMS_PIN);
        // …and it really is the URL alphabet, unpadded: the standard encoding of
        // the first vector has all three of the characters that differ.
        assert!(!NAMED_CURVE_PIN.contains(['+', '/', '=']));
    }

    #[test]
    fn the_walker_slices_the_spki_that_rcgen_says_the_key_has() {
        // Independent of the vectors above and of base64 entirely: rcgen builds
        // the SubjectPublicKeyInfo from the private key, the walker cuts it back
        // out of the finished certificate. Agreement means the bytes being
        // hashed are the public key and not some neighbouring field.
        use rcgen::PublicKeyData;
        let key = rcgen::KeyPair::generate().unwrap();
        let params = rcgen::CertificateParams::new(vec!["127.0.0.1".to_string()]).unwrap();
        let cert = params.self_signed(&key).unwrap();
        assert_eq!(spki_der(cert.der()).unwrap(), key.subject_public_key_info());
    }

    #[test]
    fn junk_is_refused_rather_than_pinned_at_the_wrong_offset() {
        // Every one of these would, under a walker that trusted its input, return
        // SOME slice — and a fingerprint over the wrong slice is a pin nothing can
        // ever satisfy.
        let good = der(NAMED_CURVE_PEM);
        assert!(spki_der(&[]).is_err());
        assert!(spki_der(&[0x30]).is_err());
        assert!(spki_der(&good[..good.len() / 2]).is_err(), "a truncated cert must not parse");
        assert!(spki_der(b"-----BEGIN CERTIFICATE-----").is_err());
        // A well-formed SEQUENCE that is not a certificate: the walker runs off
        // the end of the fields rather than returning the first thing it sees.
        assert!(spki_der(&[0x30, 0x03, 0x02, 0x01, 0x01]).is_err());
    }

    #[test]
    fn a_restart_reuses_the_stored_identity_instead_of_unpairing_every_phone() {
        let dir = tmpdir("reuse");
        let first = Tls::load_or_mint(&dir, &[]).unwrap();
        let bytes = fs::read(dir.join(IDENTITY)).unwrap();

        let second = Tls::load_or_mint(&dir, &[]).unwrap();
        assert_eq!(
            first.fingerprint(),
            second.fingerprint(),
            "the second start minted a new key: every paired phone would refuse to connect"
        );
        assert_eq!(bytes, fs::read(dir.join(IDENTITY)).unwrap(), "the identity was rewritten");
        // A DIFFERENT directory is a different bridge, so it must not somehow
        // arrive at the same key — that would mean the pin identified nothing.
        let other = Tls::load_or_mint(&tmpdir("reuse-other"), &[]).unwrap();
        assert_ne!(first.fingerprint(), other.fingerprint());
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_changed_lan_address_does_not_regenerate_the_key() {
        // The SANs are not the trust anchor, so drifting off them must cost
        // nothing. If this ever fails, a DHCP renewal silently un-pairs the phone.
        let dir = tmpdir("drift");
        let home = Tls::load_or_mint(&dir, &["192.168.0.71".parse().unwrap()]).unwrap();
        let cafe = Tls::load_or_mint(&dir, &["10.4.9.2".parse().unwrap()]).unwrap();
        assert_eq!(home.fingerprint(), cafe.fingerprint());
        let _ = fs::remove_dir_all(&dir);
    }

    #[cfg(unix)]
    #[test]
    fn the_private_key_is_not_readable_by_anyone_else() {
        use std::os::unix::fs::PermissionsExt;
        let dir = tmpdir("mode");
        Tls::load_or_mint(&dir, &[]).unwrap();
        let mode = fs::metadata(dir.join(IDENTITY)).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode, 0o600, "the bridge's private key is mode {mode:o}");
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_corrupt_identity_is_replaced_rather_than_wedging_the_bridge() {
        let dir = tmpdir("corrupt");
        let before = Tls::load_or_mint(&dir, &[]).unwrap().fingerprint();
        fs::write(dir.join(IDENTITY), "-----BEGIN CERTIFICATE-----\nnope\n").unwrap();
        let after = Tls::load_or_mint(&dir, &[]).unwrap().fingerprint();
        assert_ne!(before, after, "a fresh identity should have been minted");
        // …and the replacement is itself durable, or the bridge would re-pair on
        // every single start from here on.
        assert_eq!(after, Tls::load_or_mint(&dir, &[]).unwrap().fingerprint());
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_minted_identity_loads_as_a_server_config() {
        let dir = tmpdir("config");
        let tls = Tls::load_or_mint(&dir, &["100.68.100.56".parse().unwrap()]).unwrap();
        // Non-empty and parseable is all this proves; the handshake itself is
        // proved end-to-end in ws.rs.
        assert!(!tls.fingerprint().is_empty());
        assert!(Arc::strong_count(&tls.config()) >= 2);
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn the_identity_dir_is_durable_and_never_the_socket_directory() {
        assert!(std::env::var_os("KOD_BRIDGE_TLS_DIR").is_none());
        let dir = identity_dir_from(
            Path::new("/run/user/501/orchestrator/daemon.sock"),
            None,
            std::env::var_os("HOME"),
        );
        // The pin has to outlive the system temp reaper: a socket directory under
        // $TMPDIR is emptied after a few quiet days, and the symptom is every
        // paired phone refusing to connect with nothing naming the cause.
        assert_ne!(
            dir,
            PathBuf::from("/run/user/501/orchestrator"),
            "the identity must not live beside the socket"
        );
        if std::env::var_os("HOME").is_some() {
            assert!(
                dir.ends_with("Library/Application Support/orchestrator"),
                "expected the durable store directory, got {}",
                dir.display()
            );
        }
    }

    /// The override still wins, which is what lets a test — or a sandbox — keep
    /// its own identity instead of borrowing the real one.
    #[test]
    fn the_override_beats_everything() {
        assert_eq!(
            identity_dir_from(
                Path::new("/anywhere/daemon.sock"),
                Some("/tmp/kod-pin-test".into()),
                Some("/Users/someone".into()),
            ),
            PathBuf::from("/tmp/kod-pin-test")
        );
    }

    /// With no HOME at all there is nowhere durable, and the socket's directory is
    /// better than refusing to start.
    #[test]
    fn without_a_home_it_falls_back_rather_than_failing() {
        assert_eq!(
            identity_dir_from(Path::new("/run/orchestrator/daemon.sock"), None, None),
            PathBuf::from("/run/orchestrator")
        );
    }

    #[test]
    fn loopback_is_always_a_san_even_when_something_else_is_bound() {
        // The simulator and an SSH tunnel both arrive on 127.0.0.1, and they stay
        // bound whatever else is.
        let s = sans_for(&["192.168.0.71".parse().unwrap()]);
        assert!(s.contains(&IpAddr::V4(Ipv4Addr::LOCALHOST)));
        assert_eq!(s.len(), 2);
    }
}
