//! GameStream/Sunshine PIN-pairing handshake. `moonlight-common-c` (via `moonlight-sys`)
//! implements the actual streaming protocol but deliberately doesn't implement pairing itself —
//! every Moonlight client (moonlight-qt, moonlight-android, ...) implements this at the
//! application level, each against the same reverse-engineered NVIDIA GameStream protocol. This
//! is a from-scratch Rust port of that protocol (cross-checked against moonlight-qt's
//! `nvpairingmanager.cpp`), not something `moonlight-sys`/upstream provides.
//!
//! Protocol (five HTTP round-trips against the host's GameStream web server, default ports
//! 47989 plain HTTP / 47984 HTTPS):
//!
//! 1. `getservercert`: send a random 16-byte salt + our self-signed client cert (PEM, hex-
//!    encoded). Server returns its own self-signed cert (PEM). The salt, concatenated with the
//!    PIN the user entered (as ASCII digits) and hashed (SHA-256 for modern GameStream/Sunshine,
//!    "Gen 7+"), truncated to 16 bytes, becomes the AES-128 key both sides now share.
//! 2. `clientchallenge`: AES-128-ECB-encrypt a random 16-byte challenge, send it hex-encoded.
//!    Server decrypts it, does its own challenge-response dance internally, and returns an
//!    encrypted `challengeresponse` blob.
//! 3. `serverchallengeresp`: decrypt that blob into `serverResponseHash (32B) ||
//!    serverChallenge (16B)`. Build `serverChallenge || our cert's own signature bytes ||
//!    (new) 16-byte clientSecret`, SHA-256 it, AES-encrypt, send as `serverchallengeresp`.
//! 4. Server returns `pairingsecret = serverSecret (16B) || serverSignature`. Verify
//!    `serverSignature` is a valid RSA-SHA256 signature (by the server's cert's public key) over
//!    `serverSecret`. Then verify `serverResponseHash` (from step 3) equals
//!    SHA-256(ourRandomChallenge || serverCertSignature || serverSecret) — this is what proves
//!    the server actually knows the PIN-derived AES key, not just an eavesdropper replaying
//!    bytes.
//! 5. `clientpairingsecret`: send `clientSecret (16B) || RSA-SHA256-sign(clientSecret)` (signed
//!    with our own private key) so the server can verify we hold the private key matching the
//!    cert we sent in step 1.
//! 6. `phrase=pairchallenge`, over **HTTPS** (47984) this time, using our now-paired client cert
//!    for the TLS connection itself — final confirmation, server responds `paired=1`.
//!
//! **Not yet live-tested** — written from protocol documentation and a from-scratch Rust port of
//! moonlight-qt's implementation, but this repo doesn't have a running Sunshine instance to
//! pair against yet (see PLAN.md's architecture-pivot section). Exercise real care reading error
//! output the first time this runs against cwtrow.

use anyhow::{anyhow, bail, Context, Result};
use openssl::hash::MessageDigest;
use openssl::pkey::{PKey, Private};
use openssl::rsa::Rsa;
use openssl::sign::{Signer, Verifier};
use openssl::symm::{Cipher, Crypter, Mode};
use openssl::x509::{X509NameBuilder, X509};
use std::path::PathBuf;
use tracing::{debug, info};

/// Sunshine's `port` config value offsets its whole "family" of ports together (web UI, HTTP,
/// HTTPS, RTSP, ...) — confirmed against its own docs and, concretely, against the multiple
/// instances `host/src/topology.rs` sets up for multi-monitor streaming (each instance's `port`
/// is its HTTP port directly; HTTPS and the web UI are always exactly -5/+1 from it, regardless
/// of what the base value is).
pub(crate) struct Ports {
    pub http: u16,
    pub https: u16,
    pub web_ui: u16,
}

impl Ports {
    pub fn from_base(base: u16) -> Self {
        Self {
            http: base,
            https: base.saturating_sub(5),
            web_ui: base.saturating_add(1),
        }
    }
}

pub struct ClientIdentity {
    pub cert: X509,
    pub key: PKey<Private>,
    /// Stable per-install identifier GameStream expects on every request (`uniqueid=`).
    pub unique_id: String,
}

impl ClientIdentity {
    /// Loads a persisted identity from the config dir, or generates + persists a new one.
    /// Must stay stable across runs: the host remembers pairing by this cert, so regenerating it
    /// would require re-entering the PIN.
    pub fn load_or_generate() -> Result<Self> {
        let dir = config_dir()?;
        std::fs::create_dir_all(&dir)?;
        let cert_path = dir.join("client_cert.pem");
        let key_path = dir.join("client_key.pem");
        let id_path = dir.join("unique_id");

        if cert_path.exists() && key_path.exists() && id_path.exists() {
            let cert = X509::from_pem(&std::fs::read(&cert_path)?)?;
            let key = PKey::private_key_from_pem(&std::fs::read(&key_path)?)?;
            let unique_id = std::fs::read_to_string(&id_path)?.trim().to_string();
            return Ok(Self { cert, key, unique_id });
        }

        info!("no cached client identity, generating one (this is a one-time PIN-pairing prerequisite)");
        let rsa = Rsa::generate(2048).context("generating client RSA keypair")?;
        let key = PKey::from_rsa(rsa)?;

        let mut name = X509NameBuilder::new()?;
        name.append_entry_by_text("CN", "rdclient")?;
        let name = name.build();

        let mut builder = X509::builder()?;
        builder.set_version(2)?;
        let serial = openssl::bn::BigNum::from_u32(1)?.to_asn1_integer()?;
        builder.set_serial_number(&serial)?;
        builder.set_subject_name(&name)?;
        builder.set_issuer_name(&name)?;
        builder.set_pubkey(&key)?;
        let not_before = openssl::asn1::Asn1Time::days_from_now(0)?;
        let not_after = openssl::asn1::Asn1Time::days_from_now(3650)?;
        builder.set_not_before(&not_before)?;
        builder.set_not_after(&not_after)?;
        builder.sign(&key, MessageDigest::sha256())?;
        let cert = builder.build();

        let unique_id = random_hex(8);

        std::fs::write(&cert_path, cert.to_pem()?)?;
        std::fs::write(&key_path, key.private_key_to_pem_pkcs8()?)?;
        std::fs::write(&id_path, &unique_id)?;

        Ok(Self { cert, key, unique_id })
    }
}

fn config_dir() -> Result<PathBuf> {
    let dirs = directories::ProjectDirs::from("", "", "rdclient")
        .ok_or_else(|| anyhow!("could not determine config directory"))?;
    Ok(dirs.config_dir().to_path_buf())
}

fn random_hex(bytes: usize) -> String {
    let mut buf = vec![0u8; bytes];
    openssl::rand::rand_bytes(&mut buf).expect("openssl rand_bytes");
    buf.iter().map(|b| format!("{b:02x}")).collect()
}

fn aes_128_ecb_no_padding(mode: Mode, key: &[u8], data: &[u8]) -> Result<Vec<u8>> {
    let mut crypter = Crypter::new(Cipher::aes_128_ecb(), mode, key, None)?;
    crypter.pad(false);
    let mut out = vec![0u8; data.len() + Cipher::aes_128_ecb().block_size()];
    let mut count = crypter.update(data, &mut out)?;
    count += crypter.finalize(&mut out[count..])?;
    out.truncate(count);
    Ok(out)
}

/// Pulls `<tag>value</tag>` out of the flat, single-level XML GameStream returns. Not a general
/// XML parser — deliberately minimal, since every response here is one shallow `<root>` element.
pub(crate) fn xml_tag<'a>(xml: &'a str, tag: &str) -> Option<&'a str> {
    let open = format!("<{tag}>");
    let close = format!("</{tag}>");
    let start = xml.find(&open)? + open.len();
    let end = xml[start..].find(&close)? + start;
    Some(&xml[start..end])
}

pub(crate) fn xml_ok(xml: &str) -> bool {
    xml.contains("status_code=\"200\"")
}

/// Pairs with a GameStream/Sunshine host using a PIN the user enters into the host's web UI.
/// `host` is a bare hostname/IP, no port/scheme.
///
/// Sunshine's server holds the very first request (`getservercert`) open — it doesn't respond
/// until a human submits this same PIN via its web UI (`POST /api/pin`, under the "PIN" nav
/// item at `https://<host>:47990`). There's no server-side timeout on that hold (confirmed by
/// reading Sunshine's `nvhttp.cpp`: the response object just sits in `pair_session_t` until
/// `pin()` fires it), so this deliberately builds the HTTP client with no request timeout —
/// don't add one, or a slow human will get a spurious failure instead of a held connection.
pub async fn pair(identity: &ClientIdentity, host: &str, base_port: u16, pin: &str) -> Result<()> {
    let Ports { http, https, web_ui } = Ports::from_base(base_port);
    let client = reqwest::Client::builder()
        .danger_accept_invalid_certs(true)
        .build()?;

    let salt = {
        let mut s = [0u8; 16];
        openssl::rand::rand_bytes(&mut s)?;
        s
    };
    let cert_pem = identity.cert.to_pem()?;
    let cert_hex = hex_encode(&cert_pem);
    let salt_hex = hex_encode(&salt);

    // Stage 1: exchange certs, derive the shared AES key from salt + PIN. This request blocks
    // until the PIN below is entered on the host — see the doc comment above.
    info!(host, pin, "waiting for PIN to be entered at https://{host}:{} (PIN section) — this will hang until then", web_ui);
    let url = format!(
        "http://{host}:{http}/pair?uniqueid={}&devicename=rdclient&updateState=1&phrase=getservercert&salt={salt_hex}&clientcert={cert_hex}",
        identity.unique_id
    );
    let resp = client.get(&url).send().await?.text().await?;
    if !xml_ok(&resp) {
        bail!("pairing stage 1 (getservercert) failed: {resp}");
    }
    let server_cert_hex = xml_tag(&resp, "plaincert").ok_or_else(|| anyhow!("no plaincert in stage 1 response"))?;
    let server_cert_pem = hex_decode(server_cert_hex)?;
    let server_cert = X509::from_pem(&server_cert_pem).context("parsing server cert from stage 1")?;

    let mut salted_pin = salt.to_vec();
    salted_pin.extend_from_slice(pin.as_bytes());
    let aes_key = openssl::hash::hash(MessageDigest::sha256(), &salted_pin)?;
    let aes_key = &aes_key[..16];

    // Stage 2: prove we know the key by round-tripping an encrypted random challenge.
    let mut our_challenge = [0u8; 16];
    openssl::rand::rand_bytes(&mut our_challenge)?;
    let encrypted_challenge = aes_128_ecb_no_padding(Mode::Encrypt, aes_key, &our_challenge)?;
    let url = format!(
        "http://{host}:{http}/pair?uniqueid={}&devicename=rdclient&updateState=1&clientchallenge={}",
        identity.unique_id,
        hex_encode(&encrypted_challenge)
    );
    let resp = client.get(&url).send().await?.text().await?;
    if !xml_ok(&resp) {
        bail!("pairing stage 2 (clientchallenge) failed: {resp}");
    }
    let challenge_resp_hex =
        xml_tag(&resp, "challengeresponse").ok_or_else(|| anyhow!("no challengeresponse in stage 2 response"))?;
    let decrypted = aes_128_ecb_no_padding(Mode::Decrypt, aes_key, &hex_decode(challenge_resp_hex)?)?;
    if decrypted.len() < 32 + 16 {
        bail!("stage 2 challengeresponse too short ({} bytes)", decrypted.len());
    }
    let server_response_hash = decrypted[..32].to_vec();
    let server_challenge = decrypted[32..48].to_vec();

    // Stage 3: prove it back, and stash a client secret the server will need to sign in stage 5.
    let mut client_secret = [0u8; 16];
    openssl::rand::rand_bytes(&mut client_secret)?;
    let our_cert_signature = cert_signature_bytes(&identity.cert)?;
    let mut to_hash = server_challenge.clone();
    to_hash.extend_from_slice(&our_cert_signature);
    to_hash.extend_from_slice(&client_secret);
    let hashed = openssl::hash::hash(MessageDigest::sha256(), &to_hash)?;
    let encrypted = aes_128_ecb_no_padding(Mode::Encrypt, aes_key, &hashed)?;
    let url = format!(
        "http://{host}:{http}/pair?uniqueid={}&devicename=rdclient&updateState=1&serverchallengeresp={}",
        identity.unique_id,
        hex_encode(&encrypted)
    );
    let resp = client.get(&url).send().await?.text().await?;
    if !xml_ok(&resp) {
        bail!("pairing stage 3 (serverchallengeresp) failed: {resp}");
    }
    let pairing_secret_hex =
        xml_tag(&resp, "pairingsecret").ok_or_else(|| anyhow!("no pairingsecret in stage 3 response"))?;
    let pairing_secret = hex_decode(pairing_secret_hex)?;
    if pairing_secret.len() < 16 {
        bail!("stage 3 pairingsecret too short ({} bytes)", pairing_secret.len());
    }
    let server_secret = pairing_secret[..16].to_vec();
    let server_signature = pairing_secret[16..].to_vec();

    // Stage 4 (local verification, no HTTP call): confirm the server actually knows the PIN
    // (via server_response_hash) and holds the private key for its cert (via server_signature).
    let server_pubkey = server_cert.public_key()?;
    let mut verifier = Verifier::new(MessageDigest::sha256(), &server_pubkey)?;
    verifier.update(&server_secret)?;
    if !verifier.verify(&server_signature)? {
        bail!("server signature over its secret did not verify — wrong PIN or a MITM");
    }

    let server_cert_signature = cert_signature_bytes(&server_cert)?;
    let mut expected = our_challenge.to_vec();
    expected.extend_from_slice(&server_cert_signature);
    expected.extend_from_slice(&server_secret);
    let expected_hash = openssl::hash::hash(MessageDigest::sha256(), &expected)?;
    if expected_hash.as_ref() != server_response_hash.as_slice() {
        bail!("server response hash mismatch — wrong PIN entered, or pairing tampered with");
    }
    debug!("server identity + PIN verified locally");

    // Stage 5: prove we hold our own private key by signing the secret we generated in stage 3.
    let mut signer = Signer::new(MessageDigest::sha256(), &identity.key)?;
    signer.update(&client_secret)?;
    let client_secret_signature = signer.sign_to_vec()?;
    let mut client_pairing_secret = client_secret.to_vec();
    client_pairing_secret.extend_from_slice(&client_secret_signature);
    let url = format!(
        "http://{host}:{http}/pair?uniqueid={}&devicename=rdclient&updateState=1&clientpairingsecret={}",
        identity.unique_id,
        hex_encode(&client_pairing_secret)
    );
    let resp = client.get(&url).send().await?.text().await?;
    if !xml_ok(&resp) || xml_tag(&resp, "paired") != Some("1") {
        bail!("pairing stage 5 (clientpairingsecret) failed or not yet paired: {resp}");
    }

    // Stage 6: final confirmation over HTTPS, authenticated with our now-paired client cert.
    let https_client = https_client_with_cert(identity)?;
    let url = format!(
        "https://{host}:{https}/pair?uniqueid={}&devicename=rdclient&updateState=1&phrase=pairchallenge",
        identity.unique_id
    );
    let resp = https_client.get(&url).send().await?.text().await?;
    if !xml_ok(&resp) || xml_tag(&resp, "paired") != Some("1") {
        bail!("pairing stage 6 (pairchallenge) failed: {resp}");
    }

    info!(host, "paired successfully");
    Ok(())
}

/// Checks whether `identity` is already paired with `host:base_port`, via `/serverinfo`'s
/// `PairStatus` field — works whether or not we're paired yet (unlike `/applist`, which requires
/// it), so this is safe to call before deciding whether to run `pair()` at all. Used by the
/// auto-connect launcher to skip PIN entry for instances already paired from a previous run.
pub async fn pair_status(identity: &ClientIdentity, host: &str, base_port: u16) -> Result<bool> {
    let https = Ports::from_base(base_port).https;
    let client = https_client_with_cert(identity)?;
    let url = format!("https://{host}:{https}/serverinfo?uniqueid={}", identity.unique_id);
    let resp = client.get(&url).send().await?.text().await?;
    Ok(xml_tag(&resp, "PairStatus") == Some("1"))
}

/// Builds a reqwest client presenting our client cert for mutual-TLS-style auth, for the final
/// pairing stage and for all subsequent HTTPS calls (serverinfo/launch/resume) once paired.
pub(crate) fn https_client_with_cert(identity: &ClientIdentity) -> Result<reqwest::Client> {
    let cert_pem = identity.cert.to_pem()?;
    let key_pem = identity.key.private_key_to_pem_pkcs8()?;
    let identity = reqwest::Identity::from_pkcs8_pem(&cert_pem, &key_pem)?;
    Ok(reqwest::Client::builder()
        .danger_accept_invalid_certs(true)
        .identity(identity)
        .build()?)
}

/// Extracts the raw signature bytes attached to an X.509 cert (i.e. the cert's own
/// self-signature over its TBSCertificate), which the GameStream protocol uses as an opaque
/// per-cert identifier baked into the challenge/response hashes — not a signature *of* anything
/// we control, just a stable value tied to that specific certificate.
fn cert_signature_bytes(cert: &X509) -> Result<Vec<u8>> {
    Ok(cert.signature().as_slice().to_vec())
}

pub(crate) fn hex_encode(data: &[u8]) -> String {
    data.iter().map(|b| format!("{b:02x}")).collect()
}

pub(crate) fn hex_decode(s: &str) -> Result<Vec<u8>> {
    if s.len() % 2 != 0 {
        bail!("odd-length hex string");
    }
    (0..s.len())
        .step_by(2)
        .map(|i| u8::from_str_radix(&s[i..i + 2], 16).map_err(|e| anyhow!("invalid hex: {e}")))
        .collect()
}
