use anyhow::{Context as _, Result, bail};
use clap::Parser;
use std::{fs, io::Write as _, path::PathBuf};
use update_feed::{
    Expected, Unsigned, generate_keypair, parse_and_verify, sha256_hex_of_file, sign,
};

const PRIVATE_KEY_ENV: &str = "UPDATE_FEED_PRIVATE_KEY";

/// Creates, signs and verifies the signed update feed (`latest.json`).
#[derive(Parser)]
enum Command {
    /// Generates a signing key. The private key is written to a new file with
    /// owner-only permissions; the public key is printed.
    Keygen {
        #[arg(long)]
        private_key_out: PathBuf,
    },
    /// Signs a build. The private key comes from $UPDATE_FEED_PRIVATE_KEY, or
    /// from --private-key-file.
    Sign {
        #[arg(long)]
        private_key_file: Option<PathBuf>,
        #[arg(long)]
        channel: String,
        #[arg(long)]
        os: String,
        #[arg(long)]
        arch: String,
        #[arg(long)]
        version: String,
        /// Where clients will download the file from.
        #[arg(long)]
        url: String,
        /// The file being published; its SHA-256 goes into the feed.
        #[arg(long)]
        file: PathBuf,
        /// Write the feed here instead of stdout.
        #[arg(long)]
        out: Option<PathBuf>,
    },
    /// Checks a feed's signature, and optionally the file it describes.
    Verify {
        #[arg(long)]
        public_key: String,
        #[arg(long)]
        channel: String,
        #[arg(long)]
        os: String,
        #[arg(long)]
        arch: String,
        /// Also check that this file matches the feed's SHA-256.
        #[arg(long)]
        file: Option<PathBuf>,
        feed: PathBuf,
    },
}

fn main() -> Result<()> {
    match Command::parse() {
        Command::Keygen { private_key_out } => {
            let keypair = generate_keypair()?;
            write_private_key(&private_key_out, &keypair.private_key)?;
            println!("{}", keypair.public_key);
        }
        Command::Sign {
            private_key_file,
            channel,
            os,
            arch,
            version,
            url,
            file,
            out,
        } => {
            let private_key = match (std::env::var(PRIVATE_KEY_ENV), private_key_file) {
                (Ok(key), _) => key,
                (Err(_), Some(path)) => fs::read_to_string(&path)
                    .with_context(|| format!("failed to read {}", path.display()))?,
                (Err(_), None) => {
                    bail!("set ${PRIVATE_KEY_ENV} or pass --private-key-file")
                }
            };
            let feed = sign(
                &private_key,
                Unsigned {
                    channel,
                    os,
                    arch,
                    version,
                    url,
                    sha256: sha256_hex_of_file(&file)?,
                },
            )?;
            let json = serde_json::to_string_pretty(&feed)?;
            match out {
                Some(path) => fs::write(&path, json + "\n")
                    .with_context(|| format!("failed to write {}", path.display()))?,
                None => println!("{json}"),
            }
        }
        Command::Verify {
            public_key,
            channel,
            os,
            arch,
            file,
            feed,
        } => {
            let body =
                fs::read(&feed).with_context(|| format!("failed to read {}", feed.display()))?;
            let feed = parse_and_verify(
                &body,
                &public_key,
                Expected {
                    channel: &channel,
                    os: &os,
                    arch: &arch,
                },
            )?;
            if let Some(file) = file {
                update_feed::verify_file(&feed, &file)?;
            }
            println!("ok: {} {}", feed.channel, feed.version);
        }
    }
    Ok(())
}

fn write_private_key(path: &PathBuf, private_key: &str) -> Result<()> {
    let mut options = fs::OpenOptions::new();
    // Never overwrite an existing key.
    options.write(true).create_new(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt as _;
        options.mode(0o600);
    }
    let mut file = options
        .open(path)
        .with_context(|| format!("failed to create {}", path.display()))?;
    file.write_all(private_key.as_bytes())
        .with_context(|| format!("failed to write {}", path.display()))
}
