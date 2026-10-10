use minisign_verify::{PublicKey, Signature};
use std::{env, fs, path::Path};

fn main() {
    if let Err(error) = run() {
        eprintln!("Updater signature verification failed: {error}");
        std::process::exit(1);
    }
}

fn run() -> Result<(), Box<dyn std::error::Error>> {
    let args: Vec<_> = env::args_os().skip(1).collect();
    if args.len() != 3 {
        return Err("usage: verify-updater-signature <public-key> <signature> <artifact>".into());
    }

    let public_key = PublicKey::from_file(Path::new(&args[0]))?;
    let signature = Signature::from_file(Path::new(&args[1]))?;
    let artifact = fs::read(Path::new(&args[2]))?;
    public_key.verify(&artifact, &signature, false)?;
    println!("Verified updater signature: {}", Path::new(&args[2]).display());
    Ok(())
}

#[cfg(test)]
mod tests {
    use minisign_verify::{PublicKey, Signature};

    const PUBLIC_KEY: &str = "RWQf6LRCGA9i53mlYecO4IzT51TGPpvWucNSCh1CBM0QTaLn73Y7GFO3";
    const SIGNATURE: &str = "untrusted comment: signature from minisign secret key\nRUQf6LRCGA9i559r3g7V1qNyJDApGip8MfqcadIgT9CuhV3EMhHoN1mGTkUidF/z7SrlQgXdy8ofjb7bNJJylDOocrCo8KLzZwo=\ntrusted comment: timestamp:1633700835\tfile:test\tprehashed\nwLMDjy9FLAuxZ3q4NlEvkgtyhrr0gtTu6KC4KBJdITbbOeAi1zBIYo0v4iTgt8jJpIidRJnp94ABQkJAgAooBQ==";

    #[test]
    fn accepts_a_valid_minisign_signature() {
        let key = PublicKey::from_base64(PUBLIC_KEY).unwrap();
        let signature = Signature::decode(SIGNATURE).unwrap();
        key.verify(b"test", &signature, false).unwrap();
    }

    #[test]
    fn rejects_a_tampered_artifact() {
        let key = PublicKey::from_base64(PUBLIC_KEY).unwrap();
        let signature = Signature::decode(SIGNATURE).unwrap();
        assert!(key.verify(b"tampered", &signature, false).is_err());
    }
}
