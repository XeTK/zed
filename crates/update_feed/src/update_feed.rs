//! A signed `latest.json` describing one downloadable build of the app.
//!
//! CI signs the feed with an Ed25519 private key; the app carries the matching
//! public key and refuses to install anything the feed doesn't vouch for. The
//! feed is meant to be served from a plain static host, so nothing about the
//! host is trusted: the signature covers the channel, platform, version,
//! download URL and the SHA-256 of the file, so none of them can be swapped.

use anyhow::{Context as _, Result, bail, ensure};
use base64::{Engine as _, engine::general_purpose::STANDARD};
use ring::{
    rand::SystemRandom,
    signature::{ED25519, Ed25519KeyPair, KeyPair as _, UnparsedPublicKey},
};
use serde::{Deserialize, Serialize};
use sha2::{Digest as _, Sha256};
use std::{fs::File, io::Read as _, path::Path};

const MESSAGE_PREFIX: &str = "zed-update-feed-v1";

#[derive(Serialize, Deserialize, Clone, Debug, PartialEq, Eq)]
pub struct Feed {
    pub channel: String,
    pub os: String,
    pub arch: String,
    pub version: String,
    pub url: String,
    /// Lowercase hex SHA-256 of the file at `url`.
    pub sha256: String,
    /// Base64 Ed25519 signature over [`Feed::signed_message`].
    pub signature: String,
}

/// The build a client is looking for. A feed that is validly signed but is
/// for a different channel or platform is rejected, so a stable client can't be
/// fed a nightly build (or the other way round) by swapping files on the host.
#[derive(Clone, Copy, Debug)]
pub struct Expected<'a> {
    pub channel: &'a str,
    pub os: &'a str,
    pub arch: &'a str,
}

impl Feed {
    fn fields(&self) -> [(&'static str, &str); 6] {
        [
            ("channel", &self.channel),
            ("os", &self.os),
            ("arch", &self.arch),
            ("version", &self.version),
            ("url", &self.url),
            ("sha256", &self.sha256),
        ]
    }

    /// The exact bytes that get signed. Fields are newline-separated, so they
    /// must not contain newlines themselves (checked by `validate_fields`).
    fn signed_message(&self) -> String {
        let mut message = String::from(MESSAGE_PREFIX);
        for (name, value) in self.fields() {
            message.push('\n');
            message.push_str(name);
            message.push(':');
            message.push_str(value);
        }
        message
    }

    fn validate_fields(&self) -> Result<()> {
        for (name, value) in self.fields() {
            ensure!(!value.is_empty(), "feed field `{name}` is empty");
            ensure!(
                !value.chars().any(char::is_control),
                "feed field `{name}` contains control characters"
            );
        }
        ensure!(
            self.sha256.len() == 64 && self.sha256.chars().all(|c| c.is_ascii_hexdigit()),
            "feed sha256 is not a 64 character hex digest"
        );
        Ok(())
    }
}

/// Parses `body` and returns the feed only if its signature verifies against
/// `public_key` (base64, 32 bytes) and it is for the `expected` build.
pub fn parse_and_verify(body: &[u8], public_key: &str, expected: Expected) -> Result<Feed> {
    let feed: Feed = serde_json::from_slice(body).context("update feed is not valid JSON")?;
    verify(&feed, public_key, expected)?;
    Ok(feed)
}

pub fn verify(feed: &Feed, public_key: &str, expected: Expected) -> Result<()> {
    feed.validate_fields()?;

    let public_key = STANDARD
        .decode(public_key.trim())
        .context("update public key is not valid base64")?;
    let signature = STANDARD
        .decode(feed.signature.trim())
        .context("update feed signature is not valid base64")?;
    UnparsedPublicKey::new(&ED25519, &public_key)
        .verify(feed.signed_message().as_bytes(), &signature)
        .map_err(|_| anyhow::anyhow!("update feed signature does not match"))?;

    ensure!(
        feed.channel == expected.channel,
        "update feed is for the `{}` channel, expected `{}`",
        feed.channel,
        expected.channel
    );
    ensure!(
        feed.os == expected.os && feed.arch == expected.arch,
        "update feed is for {}-{}, expected {}-{}",
        feed.os,
        feed.arch,
        expected.os,
        expected.arch
    );
    Ok(())
}

pub struct Keypair {
    /// Base64 PKCS#8 document; keep it secret.
    pub private_key: String,
    /// Base64 raw 32 byte public key, to be compiled into the app.
    pub public_key: String,
}

pub fn generate_keypair() -> Result<Keypair> {
    let pkcs8 = Ed25519KeyPair::generate_pkcs8(&SystemRandom::new())
        .map_err(|_| anyhow::anyhow!("failed to generate an Ed25519 key"))?;
    let pair = Ed25519KeyPair::from_pkcs8(pkcs8.as_ref())
        .map_err(|_| anyhow::anyhow!("generated key was not accepted"))?;
    Ok(Keypair {
        private_key: STANDARD.encode(pkcs8.as_ref()),
        public_key: STANDARD.encode(pair.public_key().as_ref()),
    })
}

/// Everything about a build except the signature.
#[derive(Clone, Debug)]
pub struct Unsigned {
    pub channel: String,
    pub os: String,
    pub arch: String,
    pub version: String,
    pub url: String,
    pub sha256: String,
}

pub fn sign(private_key: &str, unsigned: Unsigned) -> Result<Feed> {
    let pkcs8 = STANDARD
        .decode(private_key.trim())
        .context("private key is not valid base64")?;
    let pair = Ed25519KeyPair::from_pkcs8(&pkcs8)
        .map_err(|_| anyhow::anyhow!("private key is not a valid Ed25519 PKCS#8 key"))?;

    let mut feed = Feed {
        channel: unsigned.channel,
        os: unsigned.os,
        arch: unsigned.arch,
        version: unsigned.version,
        url: unsigned.url,
        sha256: unsigned.sha256.to_ascii_lowercase(),
        signature: String::new(),
    };
    feed.validate_fields()?;
    feed.signature = STANDARD.encode(pair.sign(feed.signed_message().as_bytes()).as_ref());
    Ok(feed)
}

pub fn sha256_hex_of_file(path: &Path) -> Result<String> {
    let mut file =
        File::open(path).with_context(|| format!("failed to open {}", path.display()))?;
    let mut hasher = Sha256::new();
    let mut buffer = vec![0u8; 1024 * 1024];
    loop {
        let read = file
            .read(&mut buffer)
            .with_context(|| format!("failed to read {}", path.display()))?;
        if read == 0 {
            break;
        }
        hasher.update(&buffer[..read]);
    }
    Ok(hex::encode(hasher.finalize()))
}

/// Checks that the file at `path` hashes to the feed's `sha256`.
pub fn verify_file(feed: &Feed, path: &Path) -> Result<()> {
    let actual = sha256_hex_of_file(path)?;
    if !actual.eq_ignore_ascii_case(&feed.sha256) {
        bail!(
            "downloaded file has sha256 {actual}, but the signed feed says {}",
            feed.sha256
        );
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn unsigned() -> Unsigned {
        Unsigned {
            channel: "nightly".into(),
            os: "macos".into(),
            arch: "aarch64".into(),
            version: "1.22.0+nightly.5.abc1234".into(),
            url: "https://zed.example/nightly/Zed.dmg".into(),
            sha256: "ab".repeat(32),
        }
    }

    const EXPECTED: Expected = Expected {
        channel: "nightly",
        os: "macos",
        arch: "aarch64",
    };

    fn signed() -> (Keypair, Feed) {
        let keypair = generate_keypair().unwrap();
        let feed = sign(&keypair.private_key, unsigned()).unwrap();
        (keypair, feed)
    }

    #[test]
    fn test_signed_feed_verifies_after_a_json_round_trip() {
        let (keypair, feed) = signed();
        let body = serde_json::to_vec(&feed).unwrap();
        let parsed = parse_and_verify(&body, &keypair.public_key, EXPECTED).unwrap();
        assert_eq!(parsed, feed);
    }

    #[test]
    fn test_every_signed_field_is_covered_by_the_signature() {
        let (keypair, feed) = signed();
        let tampered = [
            Feed {
                version: "9.9.9".into(),
                ..feed.clone()
            },
            Feed {
                url: "https://evil.example/Zed.dmg".into(),
                ..feed.clone()
            },
            Feed {
                sha256: "cd".repeat(32),
                ..feed.clone()
            },
            Feed {
                channel: "stable".into(),
                ..feed.clone()
            },
            Feed {
                os: "linux".into(),
                ..feed.clone()
            },
            Feed {
                arch: "x86_64".into(),
                ..feed
            },
        ];
        for tampered in tampered {
            let result = verify(&tampered, &keypair.public_key, EXPECTED);
            assert!(
                result.is_err(),
                "tampered feed should not verify: {tampered:?}"
            );
        }
    }

    #[test]
    fn test_feed_signed_with_another_key_is_rejected() {
        let (_, feed) = signed();
        let other = generate_keypair().unwrap();
        let error = verify(&feed, &other.public_key, EXPECTED).unwrap_err();
        assert!(error.to_string().contains("signature does not match"));
    }

    #[test]
    fn test_validly_signed_feed_for_another_channel_or_platform_is_rejected() {
        let (keypair, feed) = signed();
        let wrong_channel = Expected {
            channel: "stable",
            ..EXPECTED
        };
        assert!(verify(&feed, &keypair.public_key, wrong_channel).is_err());
        let wrong_arch = Expected {
            arch: "x86_64",
            ..EXPECTED
        };
        assert!(verify(&feed, &keypair.public_key, wrong_arch).is_err());
    }

    #[test]
    fn test_newlines_cannot_be_used_to_shift_fields_between_signed_values() {
        let keypair = generate_keypair().unwrap();
        let mut forged = unsigned();
        forged.version = "1.0.0\nurl:https://evil.example/Zed.dmg".into();
        assert!(sign(&keypair.private_key, forged).is_err());
    }

    #[test]
    fn test_malformed_input_is_rejected_not_panicked_on() {
        let (keypair, feed) = signed();
        assert!(parse_and_verify(b"not json", &keypair.public_key, EXPECTED).is_err());
        assert!(verify(&feed, "not base64!!", EXPECTED).is_err());
        let bad_signature = Feed {
            signature: "%%%".into(),
            ..feed.clone()
        };
        assert!(verify(&bad_signature, &keypair.public_key, EXPECTED).is_err());
        let short_hash = Feed {
            sha256: "abcd".into(),
            ..feed
        };
        assert!(verify(&short_hash, &keypair.public_key, EXPECTED).is_err());
    }

    #[test]
    fn test_downloaded_file_must_match_the_signed_hash() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("Zed.dmg");
        std::fs::write(&path, b"the build").unwrap();

        let keypair = generate_keypair().unwrap();
        let mut build = unsigned();
        build.sha256 = sha256_hex_of_file(&path).unwrap();
        let feed = sign(&keypair.private_key, build).unwrap();
        verify_file(&feed, &path).unwrap();

        std::fs::write(&path, b"a different build").unwrap();
        assert!(verify_file(&feed, &path).is_err());
    }
}
